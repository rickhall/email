//! MIME parsing and HTML sanitization.

use std::collections::{HashMap, HashSet};

use base64::Engine;
use mail_parser::{Address, MessageParser, MimeHeaders};

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

/// Message-IDs in a raw Message-ID / In-Reply-To / References value, without angle brackets.
///
/// Tolerant of what's found in real archives (after the emailer prototype's parser): only
/// `<...>`-delimited ids count, so prose like `<id> (Your message of Mon...)` adds nothing;
/// whitespace and quotes inside an id are removed; RFC 2047 encoded-words are decoded first;
/// and ids that are really one of the message's own addresses (`<bob@example.com>`) are
/// ignored. Without any `<`, bare tokens containing `@` are accepted.
pub fn parse_msg_ids(raw: &str, ignore_addrs: &HashSet<String>) -> Vec<String> {
    let decoded;
    let text = if raw.contains("=?") {
        decoded = raw
            .split_whitespace()
            .map(|w| if w.starts_with("=?") { decode_rfc2047(w) } else { w.to_string() })
            .collect::<Vec<_>>()
            .join(" ");
        decoded.as_str()
    } else {
        raw
    };
    let mut out: Vec<String> = Vec::new();
    let mut push = |id: String| {
        if id.len() >= 3 && !ignore_addrs.contains(&id.to_ascii_lowercase()) && !out.contains(&id) {
            out.push(id);
        }
    };
    if text.contains('<') {
        let mut start = None;
        for (i, c) in text.char_indices() {
            match c {
                // No nesting: a later '<' restarts the id.
                '<' => start = Some(i + 1),
                '>' => {
                    if let Some(s) = start.take() {
                        push(text[s..i].chars().filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'').collect());
                    }
                }
                _ => {}
            }
        }
    } else {
        for tok in text.split(|c: char| c.is_whitespace() || c == ',') {
            let tok = tok.trim_matches(|c: char| c == '"' || c == '\'' || c == '(' || c == ')');
            if tok.contains('@') {
                push(tok.to_string());
            }
        }
    }
    out
}

/// Clean a stored id list from before `parse_msg_ids` existed (space-separated, brackets
/// already stripped): keep only plausible ids.
pub fn clean_stored_ids(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in s.split_whitespace() {
        let tok = tok.trim_matches(['<', '>']);
        let tok = if tok.starts_with("=?") { decode_rfc2047(tok) } else { tok.to_string() };
        for id in parse_msg_ids(&tok, &HashSet::new()) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

fn join_ids(ids: Vec<String>) -> Option<String> {
    (!ids.is_empty()).then(|| ids.join(" "))
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
    // Broken clients sometimes put an address where a message id belongs; it would glue
    // together every message to or from that person.
    let own_addrs: HashSet<String> = [msg.from(), msg.to(), msg.cc(), msg.reply_to()]
        .into_iter()
        .flatten()
        .flat_map(|a| a.iter())
        .filter_map(|a| a.address().map(|s| s.to_ascii_lowercase()))
        .collect();
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
        message_id: msg
            .header_raw("Message-ID")
            .and_then(|r| parse_msg_ids(r, &HashSet::new()).into_iter().next())
            .or_else(|| msg.message_id().map(str::to_string)),
        in_reply_to: msg.header_raw("In-Reply-To").and_then(|r| join_ids(parse_msg_ids(r, &own_addrs))),
        refs: msg.header_raw("References").and_then(|r| join_ids(parse_msg_ids(r, &own_addrs))),
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
    let text = msg.body_text(0).map(|t| match flowed_params(&msg) {
        Some(delsp) => decode_flowed(&t, delsp),
        None => t.into_owned(),
    });
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

/// `Some(delsp)` when the first text body part is `text/plain; format=flowed` (RFC 3676).
fn flowed_params(msg: &mail_parser::Message) -> Option<bool> {
    let part = msg.part(*msg.text_body.first()?)?;
    let ct = part.content_type()?;
    let plain = ct.ctype().eq_ignore_ascii_case("text") && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case("plain"));
    if !plain || !ct.attribute("format").is_some_and(|f| f.eq_ignore_ascii_case("flowed")) {
        return None;
    }
    Some(ct.attribute("delsp").is_some_and(|d| d.eq_ignore_ascii_case("yes")))
}

/// Undo RFC 3676 `format=flowed` soft line breaks (after the emailer prototype's decoder).
/// A line ending in a space continues on the next line at the same quote depth; space-stuffing
/// is removed; `delsp=yes` drops the space at each soft break. Quote depth is re-emitted as
/// `> ` prefixes so the reader's quote rendering still applies.
pub fn decode_flowed(text: &str, delsp: bool) -> String {
    let mut paras: Vec<(usize, String)> = Vec::new();
    let mut continues = false;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let depth = line.chars().take_while(|&c| c == '>').count();
        let mut rest = &line[depth..];
        rest = rest.strip_prefix(' ').unwrap_or(rest);
        // The signature separator is never a soft break.
        let soft = rest.ends_with(' ') && rest != "-- ";
        let piece = if soft && delsp { &rest[..rest.len() - 1] } else { rest };
        match paras.last_mut() {
            Some((d, p)) if continues && *d == depth => p.push_str(piece),
            _ => paras.push((depth, piece.to_string())),
        }
        continues = soft;
    }
    paras
        .into_iter()
        .map(|(depth, p)| if depth == 0 { p } else { format!("{} {}", ">".repeat(depth), p) })
        .collect::<Vec<_>>()
        .join("\n")
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

    /// FLOWED_IDS=1,2,3 BODIES_DB=/path/bodies.db cargo test -p mail-core real_flowed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_flowed() {
        let db = rusqlite::Connection::open_with_flags(
            std::env::var("BODIES_DB").unwrap(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for id in std::env::var("FLOWED_IDS").unwrap().split(',') {
            let raw: Vec<u8> = db.query_row("SELECT raw FROM bodies WHERE message_id=?1", [id], |r| r.get(0)).unwrap();
            let msg = MessageParser::default().parse(&raw).unwrap();
            let before = msg.body_text(0).unwrap().into_owned();
            let after = render(0, &raw, 0).unwrap().text.unwrap();
            let longest = |s: &str| s.lines().map(|l| l.chars().count()).max().unwrap_or(0);
            println!(
                "message {id}: flowed={:?} lines {} -> {}, longest line {} -> {} chars, words equal: {}",
                flowed_params(&msg),
                before.lines().count(),
                after.lines().count(),
                longest(&before),
                longest(&after),
                before.split_whitespace().filter(|w| !w.starts_with('>')).collect::<Vec<_>>()
                    == after.split_whitespace().filter(|w| !w.starts_with('>')).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn flowed_text() {
        let src = "This is a long \r\nparagraph.\r\n\r\n> quoted \r\n> text\r\n>> deeper\r\n -- \r\n-- \r\nsig";
        assert_eq!(
            decode_flowed(src, false),
            "This is a long paragraph.\n\n> quoted text\n>> deeper\n-- \n-- \nsig"
        );
        assert_eq!(decode_flowed("ab \r\ncd", true), "abcd");
    }

    #[test]
    fn message_ids_from_damaged_headers() {
        let none = HashSet::new();
        assert_eq!(parse_msg_ids("<a@x> <b@y>", &none), vec!["a@x", "b@y"]);
        // Prose after the id must not become ids.
        assert_eq!(parse_msg_ids("<a@x> (Your message of Mon, 3 Mar 1998)", &none), vec!["a@x"]);
        // Folding/quotes inside brackets.
        assert_eq!(parse_msg_ids("<\"a b\"@x>", &none), vec!["ab@x"]);
        // Encoded-word ids.
        assert_eq!(parse_msg_ids("=?utf-8?q?=3CPR=5Fk=40apache=2Eorg=3E?=", &none), vec!["PR_k@apache.org"]);
        // An address used as an id is ignored.
        let me: HashSet<String> = ["bob@example.com".to_string()].into();
        assert_eq!(parse_msg_ids("<Bob@Example.com> <c@z>", &me), vec!["c@z"]);
        // Bare ids without brackets.
        assert_eq!(parse_msg_ids("a@x, b@y", &none), vec!["a@x", "b@y"]);
        assert!(parse_msg_ids("org", &none).is_empty());
        // Old stored lists: fragments dropped, real ids kept.
        assert_eq!(clean_stored_ids("a@x org .org (Your b@y"), vec!["a@x", "b@y"]);
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
