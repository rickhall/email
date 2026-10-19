//! MIME parsing and HTML sanitization.

use std::collections::{HashMap, HashSet};

use base64::Engine;
use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders};

use crate::model::{Addr, Attachment, MessageDetail};

#[derive(Debug, Default)]
pub struct ParsedHeader {
    pub subject: String,
    pub from_name: String,
    pub from_addr: String,
    pub to_addrs: String,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub refs: Option<String>,
    pub likely_attachments: bool,
}

fn first_addr(a: Option<&Address>) -> (String, String) {
    match a.and_then(|a| a.first()) {
        Some(addr) => (
            addr.name().unwrap_or_default().to_string(),
            addr.address().unwrap_or_default().to_string(),
        ),
        None => (String::new(), String::new()),
    }
}

fn addr_list(a: Option<&Address>) -> Vec<Addr> {
    a.map(|a| {
        a.iter()
            .map(|x| Addr {
                name: x.name().map(str::to_string),
                addr: x.address().unwrap_or_default().to_string(),
            })
            .collect()
    })
    .unwrap_or_default()
}

fn id_list(v: &HeaderValue) -> Option<String> {
    match v {
        HeaderValue::Text(t) => Some(t.to_string()),
        HeaderValue::TextList(l) => Some(l.join(" ")),
        _ => None,
    }
}

/// Decode RFC 2047 encoded-words (`=?UTF-8?B?...?=`), e.g. in attachment filenames.
pub fn decode_rfc2047(s: &str) -> String {
    if !s.contains("=?") {
        return s.to_string();
    }
    let raw = format!("Subject: {s}\r\n\r\n");
    MessageParser::default()
        .parse_headers(raw.as_bytes())
        .and_then(|m| m.subject().map(str::to_string))
        .unwrap_or_else(|| s.to_string())
}

/// Parse a `BODY[HEADER.FIELDS (...)]` block.
pub fn parse_header_block(raw: &[u8]) -> ParsedHeader {
    let Some(msg) = MessageParser::default().parse_headers(raw) else {
        return ParsedHeader::default();
    };
    let (from_name, from_addr) = first_addr(msg.from());
    // Recipients (To + Cc) as "Name <addr>", searchable by name or address.
    let to_addrs = addr_list(msg.to())
        .into_iter()
        .chain(addr_list(msg.cc()))
        .map(|a| match a.name {
            Some(n) if !n.is_empty() => format!("{n} <{}>", a.addr),
            _ => a.addr,
        })
        .collect::<Vec<_>>()
        .join(", ");
    let likely_attachments = msg
        .content_type()
        .map(|ct| {
            ct.ctype().eq_ignore_ascii_case("multipart")
                && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case("mixed"))
        })
        .unwrap_or(false);
    ParsedHeader {
        subject: msg.subject().unwrap_or_default().to_string(),
        from_name,
        from_addr,
        to_addrs,
        date: msg.date().map(|d| d.to_timestamp()),
        message_id: msg.message_id().map(str::to_string),
        in_reply_to: id_list(msg.in_reply_to()),
        refs: id_list(msg.references()),
        likely_attachments,
    }
}

/// Snippet and real attachment presence, computed once a body is downloaded.
pub fn body_summary(raw: &[u8]) -> (Option<String>, bool) {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return (None, false);
    };
    let snippet = msg.body_preview(160).map(|s| {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    });
    let has_attachments = msg.attachments().any(|a| !is_inline_image(a));
    (snippet, has_attachments)
}

fn is_inline_image(part: &mail_parser::MessagePart) -> bool {
    part.content_id().is_some()
        && part.content_type().is_some_and(|ct| ct.ctype().eq_ignore_ascii_case("image"))
}

/// Build the reader view of a message.
pub fn render(id: i64, raw: &[u8], date: i64) -> Option<MessageDetail> {
    let msg = MessageParser::default().parse(raw)?;
    let (from_name, from_addr) = first_addr(msg.from());

    let mut attachments = Vec::new();
    let mut cid_map: HashMap<String, String> = HashMap::new();
    for part in msg.attachments() {
        let mime = part
            .content_type()
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".into());
        if let Some(cid) = part.content_id() {
            if mime.starts_with("image/") {
                let data = base64::engine::general_purpose::STANDARD.encode(part.contents());
                cid_map.insert(cid.trim_matches(['<', '>']).to_string(), format!("data:{mime};base64,{data}"));
                continue;
            }
        }
        attachments.push(Attachment {
            name: part.attachment_name().unwrap_or("attachment").to_string(),
            mime,
            size: part.contents().len(),
        });
    }

    let html = if msg.html_body_count() > 0 && !is_text_only(&msg) {
        msg.body_html(0).map(|h| sanitize_html(&h, &cid_map))
    } else {
        None
    };
    let text = msg.body_text(0).map(|t| t.into_owned());
    let has_remote_content = html.as_deref().is_some_and(has_remote);

    Some(MessageDetail {
        id,
        subject: msg.subject().unwrap_or_default().to_string(),
        from: Addr { name: (!from_name.is_empty()).then_some(from_name), addr: from_addr },
        to: addr_list(msg.to()),
        cc: addr_list(msg.cc()),
        date,
        html,
        text,
        attachments,
        has_remote_content,
        remote_allowed: false,
    })
}

/// mail-parser synthesizes an HTML body from text/plain; we'd rather render real text.
fn is_text_only(msg: &mail_parser::Message) -> bool {
    msg.html_body
        .iter()
        .all(|&i| msg.part(i).is_some_and(|p| matches!(p.body, mail_parser::PartType::Text(_))))
}

/// Remove scripts, event handlers, forms and other active content while keeping the layout
/// features (tables, inline styles, `<style>` blocks) that real-world HTML email depends on.
/// Remote loads are further gated by the CSP the reader injects into its sandboxed iframe.
pub fn sanitize_html(html: &str, cid_map: &HashMap<String, String>) -> String {
    let html = if cid_map.is_empty() {
        html.to_string()
    } else {
        replace_cids(html, cid_map)
    };

    let tags: HashSet<&str> = [
        "a", "abbr", "address", "area", "article", "aside", "b", "bdi", "bdo", "big", "blockquote",
        "br", "caption", "center", "cite", "code", "col", "colgroup", "dd", "del", "details", "dfn",
        "div", "dl", "dt", "em", "figcaption", "figure", "font", "footer", "h1", "h2", "h3", "h4",
        "h5", "h6", "header", "hr", "i", "img", "ins", "kbd", "li", "main", "map", "mark", "nav",
        "ol", "p", "picture", "pre", "q", "s", "samp", "section", "small", "source", "span",
        "strike", "strong", "style", "sub", "summary", "sup", "table", "tbody", "td", "tfoot", "th",
        "thead", "time", "tr", "tt", "u", "ul", "var", "wbr",
    ]
    .into();
    let generic: HashSet<&str> = [
        "style", "class", "id", "align", "valign", "width", "height", "bgcolor", "background",
        "border", "cellpadding", "cellspacing", "color", "face", "size", "dir", "lang", "title",
        "colspan", "rowspan", "nowrap", "role", "alt", "src", "srcset", "href", "name", "type",
        "media", "start", "reversed", "datetime", "span", "frame", "rules", "summary", "usemap",
        "shape", "coords", "target",
    ]
    .into();
    let schemes: HashSet<&str> = ["http", "https", "mailto", "tel", "data", "cid"].into();

    ammonia::Builder::default()
        .tags(tags)
        .clean_content_tags(["script", "noscript", "iframe", "object", "embed", "form"].into())
        .generic_attributes(generic)
        .tag_attributes(HashMap::new())
        .url_schemes(schemes)
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true)
        .clean(&html)
        .to_string()
}

fn replace_cids(html: &str, cid_map: &HashMap<String, String>) -> String {
    let mut out = html.to_string();
    for (cid, data) in cid_map {
        out = out.replace(&format!("cid:{cid}"), data);
    }
    out
}

fn has_remote(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    ["src=\"http", "src='http", "url(http", "url('http", "url(\"http", "background=\"http", "srcset=\"http"]
        .iter()
        .any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_active_content_keeps_layout() {
        let out = sanitize_html(
            r#"<style>.a{color:red}</style><table width="100%"><tr><td style="padding:4px" onclick="x()">hi</td></tr></table><script>alert(1)</script><img src="http://t.co/p.gif">"#,
            &HashMap::new(),
        );
        assert!(out.contains("<style>"));
        assert!(out.contains(r#"width="100%""#));
        assert!(out.contains("padding:4px"));
        assert!(!out.contains("onclick"));
        assert!(!out.contains("alert"));
        assert!(has_remote(&out));
    }

    #[test]
    fn parses_header_block() {
        let raw = b"From: \"Ada L\" <ada@example.com>\r\nTo: bob@example.com\r\nSubject: =?utf-8?q?Caf=C3=A9?=\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n";
        let h = parse_header_block(raw);
        assert_eq!(h.from_name, "Ada L");
        assert_eq!(h.from_addr, "ada@example.com");
        assert_eq!(h.subject, "Café");
        assert!(h.likely_attachments);
    }
}
