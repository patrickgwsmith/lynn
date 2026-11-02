use crate::session::Resource;
use indexmap::IndexMap;
use std::time::{Duration, Instant};
use url::Url;

const MAX_RESOURCE_BYTES: u64 = 4 * 1024 * 1024;

pub fn origin(url: &str) -> Result<String, String> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|error| format!("invalid URL: {error}"))?;
    let scheme = uri.scheme_str().ok_or("URL has no scheme")?;
    if !matches!(scheme, "http" | "https") {
        return Err(format!("unsupported URL scheme: {scheme}"));
    }
    let host = uri.host().ok_or("URL has no host")?;
    let port = uri
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    Ok(format!("{scheme}://{}:{port}", host.to_ascii_lowercase()))
}

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_idle_connections_per_host(1)
        .max_idle_age(Duration::from_secs(60))
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .new_agent()
}

const MAX_REDIRECTS: usize = 5;

/// The result of a GET that may have followed redirects.
pub enum Loaded {
    Resource(Resource),
    /// The redirects reached `url`, which `is_stored` reported as already loaded.
    Stored {
        url: String,
        redirect_targets: IndexMap<String, String>,
    },
}

/// GETs a URL the user entered, following up to five redirects to any http(s) origin.
/// Stops early when a redirect reaches a URL for which `is_stored` returns true.
pub fn load(
    agent: &ureq::Agent,
    url: &str,
    is_stored: &dyn Fn(&str) -> bool,
) -> Result<Loaded, String> {
    let mut redirect_targets = IndexMap::new();
    let mut current = url.to_owned();
    let mut response = loop {
        let response = agent
            .get(&current)
            .call()
            .map_err(|error| error.to_string())?;
        if !response.status().is_redirection() {
            break response;
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| format!("HTTP {} has no Location header", response.status()))?;
        let target = Url::parse(&current)
            .and_then(|base| base.join(location))
            .map_err(|error| format!("invalid redirect Location {location}: {error}"))?
            .to_string();
        origin(&target).map_err(|error| format!("redirect to {target}: {error}"))?;
        if target == url || redirect_targets.contains_key(&target) {
            return Err(format!("redirect loop at {target}"));
        }
        if redirect_targets.len() == MAX_REDIRECTS {
            return Err(format!(
                "more than {MAX_REDIRECTS} redirects; last went to {target}"
            ));
        }
        redirect_targets.insert(current, target.clone());
        if is_stored(&target) {
            return Ok(Loaded::Stored {
                url: target,
                redirect_targets,
            });
        }
        current = target;
    };
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_owned();
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    if media_type != "unknown"
        && !media_type.starts_with("text/")
        && !matches!(media_type, "application/json" | "application/xml")
        && !media_type.ends_with("+json")
        && !media_type.ends_with("+xml")
    {
        return Err(format!("resource is not text ({content_type})"));
    }
    let content = response
        .body_mut()
        .with_config()
        .limit(MAX_RESOURCE_BYTES)
        .read_to_string()
        .map_err(|error| error.to_string())?;
    Ok(Loaded::Resource(Resource {
        body_url: current,
        redirect_targets,
        content_type,
        content,
    }))
}

pub fn head(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let start = Instant::now();
    let response = agent.head(url).call().map_err(|error| error.to_string())?;
    let elapsed = start.elapsed();
    let mut output = format!("HTTP {:?}\n", response.status());
    for (name, value) in response.headers() {
        output.push_str(&format!(
            "{}: {}\n",
            name,
            value.to_str().unwrap_or("<non-UTF-8>")
        ));
    }
    output.push_str(&format!(
        "Time to headers: {:.1} ms",
        elapsed.as_secs_f64() * 1000.0
    ));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{agent, load, origin, Loaded};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// Serves `/hop/N` as a relative redirect to `/hop/N-1`, `/hop/0` as text,
    /// `/loop` as a redirect to itself, and `/away` as a redirect to a non-HTTP scheme.
    fn serve() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 2 {
                    line.clear();
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                let response = match path.strip_prefix("/hop/") {
                    Some("0") => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody".to_owned(),
                    Some(n) => format!(
                        "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        n.parse::<usize>().unwrap() - 1
                    ),
                    None if path == "/loop" => "HTTP/1.1 302 Found\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                    None => "HTTP/1.1 302 Found\r\nLocation: ftp://example.com/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        base
    }

    #[test]
    fn load_follows_redirects_and_records_each_hop() {
        let base = serve();
        let Loaded::Resource(resource) =
            load(&agent(), &format!("{base}/hop/2"), &|_| false).unwrap()
        else {
            panic!("expected a resource");
        };
        assert_eq!(resource.content, "body");
        assert_eq!(resource.body_url, format!("{base}/hop/0"));
        assert_eq!(
            resource.redirect_targets.into_iter().collect::<Vec<_>>(),
            [
                (format!("{base}/hop/2"), format!("{base}/hop/1")),
                (format!("{base}/hop/1"), format!("{base}/hop/0")),
            ]
        );
    }

    #[test]
    fn load_stops_at_a_stored_url() {
        let base = serve();
        let stored = format!("{base}/hop/1");
        let Loaded::Stored {
            url,
            redirect_targets,
        } = load(&agent(), &format!("{base}/hop/3"), &|url| url == stored).unwrap()
        else {
            panic!("expected the stored URL");
        };
        assert_eq!(url, stored);
        assert_eq!(redirect_targets.len(), 2);
    }

    #[test]
    fn load_rejects_loops_long_chains_and_non_http_redirects() {
        let base = serve();
        let client = agent();
        assert!(load(&client, &format!("{base}/hop/5"), &|_| false).is_ok());
        let error = load(&client, &format!("{base}/hop/6"), &|_| false)
            .err()
            .unwrap();
        assert!(error.contains("more than 5 redirects"), "{error}");
        let error = load(&client, &format!("{base}/loop"), &|_| false)
            .err()
            .unwrap();
        assert!(error.contains("redirect loop"), "{error}");
        let error = load(&client, &format!("{base}/away"), &|_| false)
            .err()
            .unwrap();
        assert!(error.contains("unsupported URL scheme: ftp"), "{error}");
    }

    #[test]
    fn origin_includes_scheme_and_port() {
        assert_eq!(
            origin("https://EXAMPLE.com/a").unwrap(),
            "https://example.com:443"
        );
        assert_eq!(
            origin("http://example.com:8080/").unwrap(),
            "http://example.com:8080"
        );
        assert!(origin("https:///missing-host").is_err());
    }
}
