//! Readable text from an HTML page, for agent context excerpts.
//!
//! Pages often begin with many kilobytes of inline CSS, scripts, SVG and
//! navigation, so a raw excerpt can end before the page's own text starts.

/// Elements whose contents are dropped entirely.
const SKIPPED: &[&str] = &[
    "style", "script", "svg", "noscript", "template", "nav", "iframe", "canvas",
];

/// Elements that start a new line.
const BLOCKS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "br",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "hr",
    "html",
    "li",
    "main",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "thead",
    "tfoot",
    "title",
    "tr",
    "ul",
];

pub fn is_html(content_type: &str) -> bool {
    let essence = content_type.split(';').next().unwrap_or("").trim();
    essence.eq_ignore_ascii_case("text/html")
        || essence.eq_ignore_ascii_case("application/xhtml+xml")
}

/// The visible text of `html`, with headings, list items and table cells
/// marked in a Markdown-like way and whitespace collapsed outside `<pre>`.
pub fn text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let mut rest = html;
    let mut pre_depth = 0usize;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut out, rest, pre_depth > 0);
            break;
        };
        push_text(&mut out, &rest[..lt], pre_depth > 0);
        rest = &rest[lt..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        let Some(tag) = Tag::parse(rest) else {
            push_text(&mut out, "<", pre_depth > 0);
            rest = &rest[1..];
            continue;
        };
        rest = &rest[tag.len..];
        let name = tag.name.as_str();
        if !tag.closing && !tag.self_closing && SKIPPED.contains(&name) {
            rest = skip_element(rest, name);
            continue;
        }
        match name {
            "pre" if tag.closing => pre_depth = pre_depth.saturating_sub(1),
            "pre" => pre_depth += 1,
            _ => {}
        }
        if let Some(level) = heading_level(name) {
            new_line(&mut out, true);
            if !tag.closing {
                out.push_str(&"#".repeat(level));
                out.push(' ');
            }
        } else if name == "li" && !tag.closing {
            new_line(&mut out, false);
            out.push_str("- ");
        } else if matches!(name, "td" | "th") && !tag.closing {
            out.push_str(" | ");
        } else if BLOCKS.contains(&name) {
            new_line(
                &mut out,
                matches!(name, "p" | "pre" | "table" | "blockquote"),
            );
        }
    }
    tidy(&out)
}

struct Tag {
    name: String,
    closing: bool,
    self_closing: bool,
    len: usize,
}

impl Tag {
    /// A tag at the start of `text`, which begins with `<`.
    fn parse(text: &str) -> Option<Tag> {
        let bytes = text.as_bytes();
        let mut index = 1;
        let closing = bytes.get(index) == Some(&b'/');
        if closing {
            index += 1;
        }
        let first = *bytes.get(index)?;
        if first == b'!' || first == b'?' {
            let end = text.find('>')?;
            return Some(Tag {
                name: String::new(),
                closing: false,
                self_closing: true,
                len: end + 1,
            });
        }
        if !first.is_ascii_alphabetic() {
            return None;
        }
        let name_start = index;
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            index += 1;
        }
        let name = text[name_start..index].to_ascii_lowercase();
        let mut quote = None;
        while let Some(&byte) = bytes.get(index) {
            match (quote, byte) {
                (Some(open), _) if byte == open => quote = None,
                (Some(_), _) => {}
                (None, b'"' | b'\'') => quote = Some(byte),
                (None, b'>') => {
                    return Some(Tag {
                        name,
                        closing,
                        self_closing: bytes[index - 1] == b'/',
                        len: index + 1,
                    });
                }
                _ => {}
            }
            index += 1;
        }
        None
    }
}

/// The text after the element `name` whose start tag was just read.
fn skip_element<'a>(mut rest: &'a str, name: &str) -> &'a str {
    let raw_text = matches!(name, "style" | "script");
    let mut depth = 1usize;
    while let Some(lt) = rest.find('<') {
        rest = &rest[lt..];
        match Tag::parse(rest) {
            Some(tag) if tag.name == name => {
                rest = &rest[tag.len..];
                if tag.closing {
                    depth -= 1;
                    if depth == 0 {
                        return rest;
                    }
                } else if !tag.self_closing && !raw_text {
                    depth += 1;
                }
            }
            _ => rest = &rest[1..],
        }
    }
    ""
}

fn heading_level(name: &str) -> Option<usize> {
    match name.as_bytes() {
        [b'h', level @ b'1'..=b'6'] => Some((level - b'0') as usize),
        _ => None,
    }
}

fn new_line(out: &mut String, blank: bool) {
    while out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        return;
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if blank && !out.ends_with("\n\n") {
        out.push('\n');
    }
}

fn push_text(out: &mut String, text: &str, preformatted: bool) {
    let decoded = decode_entities(text);
    if preformatted {
        out.push_str(&decoded);
        return;
    }
    for character in decoded.chars() {
        if character.is_whitespace() {
            if !(out.is_empty() || out.ends_with([' ', '\n'])) {
                out.push(' ');
            }
        } else {
            out.push(character);
        }
    }
}

fn decode_entities(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('&') {
        return text.into();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest[1..]
            .find(';')
            .filter(|end| *end <= 10)
            .and_then(|end| {
                let entity = &rest[1..end + 1];
                let character = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                        .map(|hex| u32::from_str_radix(hex, 16))
                        .or_else(|| entity.strip_prefix('#').map(str::parse))
                        .and_then(Result::ok)
                        .and_then(char::from_u32),
                }?;
                Some((character, end + 2))
            });
        match decoded {
            Some((character, len)) => {
                out.push(character);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out.into()
}

/// Trims each line's trailing spaces and limits runs of blank lines to one.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_lines = 0;
    for line in text.trim().lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_lines += 1;
            if blank_lines > 1 {
                continue;
            }
        } else {
            blank_lines = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::{is_html, text};

    #[test]
    fn drops_styles_scripts_svg_and_navigation() {
        let page = r#"<!doctype html><html><head><title>QIP Docs</title>
            <style>:root { --a: 1; } a > b { color: red }</style>
            <script>if (a < b) { x = "</nav>" }</script></head>
            <body><header><a href="/"><svg><g><rect/></g><svg><rect/></svg></svg></a></header>
            <nav><ol><li><a href="/docs">Docs</a></li><nav>inner</nav><li>More</li></ol></nav>
            <!-- a <p>comment</p> -->
            <main><article><h1>QIP   Docs</h1><p>A &lt;component&gt; &amp; its
            <code>render</code>&#8217;s output.</p></article></main></body></html>"#;
        assert_eq!(
            text(page),
            "QIP Docs\n\n# QIP Docs\n\nA <component> & its render\u{2019}s output."
        );
    }

    #[test]
    fn keeps_lists_tables_and_preformatted_text() {
        let page = "<ul><li>one</li><li>two</li></ul>\
            <table><tr><th>Export</th><th>Rule</th></tr><tr><td>memory</td><td>shared</td></tr></table>\
            <pre><code>a  -&gt; b\n  c</code></pre><p>after</p>";
        assert_eq!(
            text(page),
            "- one\n- two\n\n | Export | Rule\n | memory | shared\n\na  -> b\n  c\n\nafter"
        );
    }

    #[test]
    fn recognises_html_content_types() {
        assert!(is_html("text/html; charset=utf-8"));
        assert!(is_html("application/xhtml+xml"));
        assert!(!is_html("text/plain; charset=utf-8"));
    }
}
