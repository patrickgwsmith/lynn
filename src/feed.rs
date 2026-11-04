use crate::session::{DiffMode, Resource};
use quick_xml::events::Event;
use quick_xml::Reader;
use quick_xml::XmlVersion;
use std::collections::HashSet;

#[derive(Default)]
struct Entry {
    id: String,
    title: String,
    link: String,
    /// Atom `rel="related"` link, such as a link blog's own post about the linked article.
    related: String,
}

#[derive(Clone, Copy)]
enum FeedFormat {
    Atom,
    Rss,
}

impl FeedFormat {
    fn item_path(self) -> &'static [&'static str] {
        match self {
            Self::Atom => &["feed", "entry"],
            Self::Rss => &["rss", "channel", "item"],
        }
    }
}

pub fn is_feed_content_type(content_type: &str) -> bool {
    content_type.split(';').next().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "application/atom+xml" | "application/rss+xml"
        )
    })
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

pub fn render(resource: &Resource) -> Result<String, String> {
    let mut reader = Reader::from_str(&resource.content);
    let mut path = Vec::new();
    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    let mut format = None;
    let mut saw_channel = false;
    loop {
        let event = reader
            .read_event()
            .map_err(|error| format!("invalid feed XML: {error}"))?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => {
                let name = local_name(tag.name().as_ref()).to_owned();
                if path.is_empty() {
                    format = Some(match name.as_str() {
                        "feed" => {
                            let namespace_name = tag
                                .name()
                                .as_ref()
                                .split_once(':')
                                .map(|(prefix, _)| format!("xmlns:{prefix}"))
                                .unwrap_or_else(|| "xmlns".to_owned());
                            let mut atom_namespace = false;
                            for attribute in tag.attributes() {
                                let attribute = attribute
                                    .map_err(|error| format!("invalid Atom attribute: {error}"))?;
                                if attribute.key.as_ref() == namespace_name {
                                    atom_namespace = attribute
                                        .normalized_value(XmlVersion::Implicit1_0)
                                        .map_err(|error| {
                                            format!("invalid Atom attribute: {error}")
                                        })?
                                        == "http://www.w3.org/2005/Atom";
                                }
                            }
                            if !atom_namespace {
                                return Err("XML root is not in the Atom namespace".into());
                            }
                            FeedFormat::Atom
                        }
                        "rss" => FeedFormat::Rss,
                        _ => return Err("XML root is not an Atom or RSS feed".into()),
                    });
                }
                let format = format.expect("feed root was checked");
                if matches!(format, FeedFormat::Rss) && path == ["rss"] && name == "channel" {
                    saw_channel = true;
                }
                if path.as_slice() == &format.item_path()[..format.item_path().len() - 1]
                    && name == format.item_path()[format.item_path().len() - 1]
                {
                    current = Some(Entry::default());
                }
                if matches!(format, FeedFormat::Atom) && path == ["feed", "entry"] && name == "link"
                {
                    let mut href = None;
                    let mut rel = None;
                    for attribute in tag.attributes() {
                        let attribute = attribute
                            .map_err(|error| format!("invalid Atom attribute: {error}"))?;
                        let value = attribute
                            .normalized_value(XmlVersion::Implicit1_0)
                            .map_err(|error| format!("invalid Atom attribute: {error}"))?
                            .into_owned();
                        match local_name(attribute.key.as_ref()) {
                            "href" => href = Some(value),
                            "rel" => rel = Some(value),
                            _ => {}
                        }
                    }
                    if let (Some(entry), Some(href)) = (&mut current, href) {
                        match rel.as_deref() {
                            None | Some("alternate") => entry.link = href,
                            Some("related") if entry.related.is_empty() => entry.related = href,
                            _ => {}
                        }
                    }
                }
                path.push(name);
                if empty {
                    path.pop();
                }
            }
            Event::End(_) => {
                let format = format.ok_or("XML has no feed root")?;
                if path.as_slice() == format.item_path() {
                    if let Some(entry) = current.take() {
                        if matches!(format, FeedFormat::Atom)
                            && (entry.id.trim().is_empty() || entry.title.trim().is_empty())
                        {
                            return Err("Atom entry is missing an id or title".into());
                        }
                        if matches!(format, FeedFormat::Rss) && entry.title.trim().is_empty() {
                            return Err("RSS item is missing a title".into());
                        }
                        let title = one_line(&entry.title);
                        let link = if !entry.link.trim().is_empty() {
                            entry.link.trim()
                        } else if matches!(format, FeedFormat::Atom)
                            || entry.id.trim().starts_with("https://")
                            || entry.id.trim().starts_with("http://")
                        {
                            entry.id.trim()
                        } else {
                            ""
                        };
                        let related = entry.related.trim();
                        entries.push(
                            match (link.is_empty(), related.is_empty() || related == link) {
                                (true, true) => title,
                                (true, false) => format!("{title} — {related}"),
                                (false, true) => format!("{title} — {link}"),
                                (false, false) => format!("{title} — {link} (post: {related})"),
                            },
                        );
                    }
                }
                path.pop();
            }
            Event::Text(text) => {
                append_item_text(format, &path, &mut current, &text.xml10_content());
            }
            Event::CData(text) => {
                append_item_text(format, &path, &mut current, &text.xml10_content());
            }
            Event::GeneralRef(reference) if current.is_some() => {
                let character = reference
                    .resolve_char_ref()
                    .map_err(|error| format!("invalid feed entity: {error}"))?
                    .or_else(|| match reference.as_ref() {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => None,
                    })
                    .ok_or("unsupported feed entity")?;
                append_item_text(format, &path, &mut current, &character.to_string());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if format.is_none() || matches!(format, Some(FeedFormat::Rss)) && !saw_channel {
        return Err("response is not an Atom or RSS feed".into());
    }
    Ok(entries.join("\n"))
}

fn append_item_text(
    format: Option<FeedFormat>,
    path: &[String],
    current: &mut Option<Entry>,
    text: &str,
) {
    let Some(format) = format else { return };
    let item_path = format.item_path();
    if path.len() != item_path.len() + 1
        || !path
            .iter()
            .zip(item_path)
            .all(|(actual, expected)| actual == expected)
    {
        return;
    }
    if let Some(entry) = current {
        match (format, path.last().map(String::as_str)) {
            (FeedFormat::Atom, Some("id")) | (FeedFormat::Rss, Some("guid")) => {
                entry.id.push_str(text)
            }
            (_, Some("title")) => entry.title.push_str(text),
            (FeedFormat::Rss, Some("link")) => entry.link.push_str(text),
            _ => {}
        }
    }
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn diff(previous: &str, current: &str, mode: DiffMode) -> String {
    let old: HashSet<_> = previous.lines().collect();
    let new: HashSet<_> = current.lines().collect();
    let mut lines = Vec::new();
    if mode == DiffMode::AddedAndRemoved {
        for line in previous.lines().filter(|line| !new.contains(line)) {
            lines.push(format!("-{line}"));
        }
    }
    for line in current.lines().filter(|line| !old.contains(line)) {
        lines.push(format!("+{line}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_entries_render_and_diff_without_reorder_noise() {
        let resource = Resource {
            body_url: "https://example.com/commits.atom".into(),
            redirect_targets: Default::default(),
            content_type: "application/atom+xml; charset=utf-8".into(),
            content: r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>tag:example,1</id><title>Fix &amp; test</title><link href="https://example.com/1"/></entry><entry><id>tag:example,2</id><title>Other</title><link rel="alternate" href="https://example.com/2"/></entry></feed>"#.into(),
        };
        let rendered = render(&resource).unwrap();
        assert_eq!(
            rendered,
            "Fix & test — https://example.com/1\nOther — https://example.com/2"
        );
        assert!(is_feed_content_type(&resource.content_type));
        assert_eq!(
            diff("Other — https://example.com/2", &rendered, DiffMode::Added),
            "+Fix & test — https://example.com/1"
        );
        assert_eq!(
            diff(
                &rendered,
                "Other — https://example.com/2",
                DiffMode::AddedAndRemoved
            ),
            "-Fix & test — https://example.com/1"
        );
    }

    #[test]
    fn atom_related_link_is_shown_as_the_post() {
        let resource = Resource {
            body_url: "https://daringfireball.net/feeds/main".into(),
            redirect_targets: Default::default(),
            content_type: "application/atom+xml".into(),
            content: r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><title>Linked</title><link rel="alternate" type="text/html" href="https://example.com/article"/><link rel="shorturl" href="http://df4.us/x"/><link rel="related" type="text/html" href="https://daringfireball.net/linked/1"/><id>tag:df,1</id></entry><entry><title>Same</title><link rel="related" href="https://daringfireball.net/2"/><link rel="alternate" href="https://daringfireball.net/2"/><id>tag:df,2</id></entry><entry><title>Article</title><link rel="alternate" href="https://daringfireball.net/3"/><id>tag:df,3</id></entry></feed>"#.into(),
        };
        assert_eq!(
            render(&resource).unwrap(),
            "Linked — https://example.com/article (post: https://daringfireball.net/linked/1)\nSame — https://daringfireball.net/2\nArticle — https://daringfireball.net/3"
        );
    }

    #[test]
    fn rss_items_render_and_diff_with_entities_and_cdata() {
        let resource = Resource {
            body_url: "https://static.crates.io/rss/updates.xml".into(),
            redirect_targets: Default::default(),
            content_type: "text/xml; charset=utf-8".into(),
            content: r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Crates.io updates</title><item><title>serde &amp; friends 1.0</title><link>https://crates.io/crates/serde/1.0</link><guid>serde-1.0</guid></item><item><title><![CDATA[quick xml 2.0]]></title><guid>https://crates.io/crates/quick-xml/2.0</guid></item><item><title>Title only</title><guid isPermaLink="false">opaque-id</guid></item></channel></rss>"#.into(),
        };
        let rendered = render(&resource).unwrap();
        assert_eq!(
            rendered,
            "serde & friends 1.0 — https://crates.io/crates/serde/1.0\nquick xml 2.0 — https://crates.io/crates/quick-xml/2.0\nTitle only"
        );
        assert!(!is_feed_content_type(&resource.content_type));
        assert!(is_feed_content_type("application/rss+xml; charset=utf-8"));
        assert_eq!(
            diff(
                "quick xml 2.0 — https://crates.io/crates/quick-xml/2.0\nTitle only",
                &rendered,
                DiffMode::Added
            ),
            "+serde & friends 1.0 — https://crates.io/crates/serde/1.0"
        );
    }

    #[test]
    fn rejects_non_feed_xml_and_rss_without_channel() {
        let mut resource = Resource {
            body_url: "https://example.com/other.xml".into(),
            redirect_targets: Default::default(),
            content_type: "text/xml".into(),
            content: "<html><title>Not a feed</title></html>".into(),
        };
        assert!(render(&resource).is_err());
        resource.content = "<rss version=\"2.0\"></rss>".into();
        assert!(render(&resource).is_err());
    }
}
