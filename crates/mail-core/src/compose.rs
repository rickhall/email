//! Writing mail: building the RFC 5322 message, and the rules for replies and forwards
//! (subjects, recipients, threading headers). Sending is in `smtp`; queuing in `store`.

use anyhow::Result;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;

use crate::model::Addr;

/// A file to send with the message.
#[derive(Debug, Clone)]
pub struct OutAttachment {
    pub name: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Everything needed to build one outgoing message.
#[derive(Debug, Clone, Default)]
pub struct Outgoing {
    pub from: Option<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    /// Envelope only: never written into the message.
    pub bcc: Vec<Addr>,
    pub subject: String,
    /// The body as HTML (the editor's output, plus any quoted original).
    pub html: String,
    pub attachments: Vec<OutAttachment>,
    /// Message-IDs without angle brackets.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

impl Outgoing {
    /// Envelope recipients: To, Cc and Bcc, each address once.
    pub fn recipients(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for a in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            let addr = a.addr.trim().to_string();
            if !addr.is_empty() && !out.iter().any(|x| x.eq_ignore_ascii_case(&addr)) {
                out.push(addr);
            }
        }
        out
    }
}

fn address(list: &[Addr]) -> Address<'_> {
    Address::new_list(
        list.iter()
            .map(|a| match &a.name {
                Some(n) if !n.trim().is_empty() => Address::new_address(Some(n.trim()), a.addr.trim()),
                _ => Address::new_address(None::<&str>, a.addr.trim()),
            })
            .collect(),
    )
}

/// MIME type for an attachment by file extension (application/octet-stream otherwise).
pub fn content_type_for(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "ics" => "text/calendar",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "json" => "application/json",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// A new Message-ID (without brackets) on the sender's domain.
pub fn new_message_id(from_addr: &str) -> String {
    let domain = from_addr.rsplit_once('@').map(|(_, d)| d.trim()).filter(|d| !d.is_empty()).unwrap_or("localhost");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut rnd = [0u8; 8];
    getrandom(&mut rnd);
    let tail: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    format!("{nanos:x}.{tail}@{domain}")
}

/// Random bytes without another dependency: the OS's source via std's hasher seeds.
fn getrandom(buf: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    for chunk in buf.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(chunk.as_ptr() as u64);
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
}

/// An image carried in the HTML itself (`<img src="data:image/png;base64,…">`, as a picture
/// pasted into a signature): sent as a part of its own that the HTML refers to by `cid:`, since
/// Gmail, Outlook and others don't show `data:` images.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineImage {
    pub content_type: String,
    pub cid: String,
    pub data: Vec<u8>,
}

/// The plain-text version of a message's HTML: as the parser makes it, except code and lines.
/// A code block keeps its lines and indentation, fenced with ``` (as in Markdown, and as the
/// composer takes it), inline code is in backticks, and a horizontal line is `---`.
pub fn plain_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut blocks: Vec<String> = Vec::new();
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    while let Some(start) = lower[at..].find("<pre").map(|i| at + i) {
        // `<pre>` or `<pre ...>`, not `<prefix…>`
        if !matches!(lower.as_bytes().get(start + 4), Some(b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/')) {
            out.push_str(&inline_code_marks(&html[at..start + 4], &lower[at..start + 4]));
            at = start + 4;
            continue;
        }
        let Some(open_end) = lower[start..].find('>').map(|i| start + i + 1) else { break };
        let Some(close) = lower[open_end..].find("</pre>").map(|i| open_end + i) else { break };
        out.push_str(&inline_code_marks(&html[at..start], &lower[at..start]));
        out.push_str(&format!("<p>\u{1}CODEBLOCK{}\u{1}</p>", blocks.len()));
        blocks.push(code_text(&html[open_end..close]));
        at = close + "</pre>".len();
    }
    out.push_str(&inline_code_marks(&html[at..], &lower[at..]));
    let out = rules_as_dashes(&out);
    let mut text = mail_parser::decoders::html::html_to_text(&out);
    for (i, code) in blocks.iter().enumerate() {
        text = text.replace(&format!("\u{1}CODEBLOCK{i}\u{1}"), &format!("```\n{}\n```", code.trim_end_matches('\n')));
    }
    text
}

/// `<hr>` as a paragraph of `---`, for the plain-text version.
fn rules_as_dashes(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    while let Some(i) = lower[at..].find("<hr").map(|i| at + i) {
        let Some(end) = lower[i..].find('>').map(|e| i + e + 1) else { break };
        out.push_str(&html[at..i]);
        if matches!(lower.as_bytes().get(i + 3), Some(b'>' | b' ' | b'/' | b'\t' | b'\n' | b'\r')) {
            out.push_str("<p>---</p>");
        } else {
            out.push_str(&html[i..end]);
        }
        at = end;
    }
    out.push_str(&html[at..]);
    out
}

/// Backticks around inline code (`<code>`), for the plain-text version.
fn inline_code_marks(html: &str, lower: &str) -> String {
    let mut out = String::with_capacity(html.len() + 8);
    let mut at = 0;
    while let Some(i) = lower[at..].find("<code").map(|i| at + i) {
        let tag_end = match lower[i..].find('>') {
            Some(e) => i + e + 1,
            None => break,
        };
        if !matches!(lower.as_bytes().get(i + 5), Some(b'>' | b' ' | b'\t' | b'\n' | b'\r')) {
            out.push_str(&html[at..tag_end]);
            at = tag_end;
            continue;
        }
        out.push_str(&html[at..i]);
        out.push('`');
        at = tag_end;
        if let Some(c) = lower[at..].find("</code>").map(|c| at + c) {
            out.push_str(&html[at..c]);
            out.push('`');
            at = c + "</code>".len();
        }
    }
    out.push_str(&html[at..]);
    out
}

/// A code block's text as written: tags dropped (`<br>` a line break), entities decoded,
/// spaces and line breaks kept.
fn code_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..lt]));
        let Some(gt) = rest[lt..].find('>') else {
            rest = "";
            break;
        };
        if rest[lt..lt + gt].to_ascii_lowercase().starts_with("<br") {
            out.push('\n');
        }
        rest = &rest[lt + gt + 1..];
    }
    out.push_str(&decode_entities(rest));
    out
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let end = after.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| {
            let name = &after[..e];
            let c = match name {
                "lt" => '<',
                "gt" => '>',
                "amp" => '&',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ if name.starts_with("#x") || name.starts_with("#X") => char::from_u32(u32::from_str_radix(&name[2..], 16).ok()?)?,
                _ if name.starts_with('#') => char::from_u32(name[1..].parse().ok()?)?,
                _ => return None,
            };
            Some((c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &after[e + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The HTML with its `data:` images taken out into [`InlineImage`]s (one per distinct image),
/// the `src`s pointing at their `cid:`. `tag` makes the Content-IDs unique (a Message-ID).
pub fn inline_images(html: &str, tag: &str) -> (String, Vec<InlineImage>) {
    use base64::Engine;
    let mut out = String::with_capacity(html.len());
    let mut images: Vec<InlineImage> = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("data:image/") {
        // Only an attribute value: src="data:…" or src='data:…'.
        let quote = rest[..at].chars().next_back();
        let is_src = matches!(quote, Some('"') | Some('\'')) && rest[..at - 1].trim_end().to_ascii_lowercase().ends_with("src=");
        let Some(q) = quote.filter(|_| is_src) else {
            out.push_str(&rest[..at + 11]);
            rest = &rest[at + 11..];
            continue;
        };
        let value_end = rest[at..].find(q).map(|e| at + e).unwrap_or(rest.len());
        let value = &rest[at..value_end];
        let decoded = value.split_once(";base64,").and_then(|(head, b64)| {
            let ct = head.strip_prefix("data:")?.to_ascii_lowercase();
            let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
            let data = base64::engine::general_purpose::STANDARD.decode(clean).ok()?;
            Some((ct, data))
        });
        out.push_str(&rest[..at]);
        match decoded {
            Some((content_type, data)) => {
                let cid = match images.iter().find(|i| i.data == data) {
                    Some(i) => i.cid.clone(),
                    None => {
                        let cid = format!("img{}.{tag}", images.len() + 1);
                        images.push(InlineImage { content_type, cid: cid.clone(), data });
                        cid
                    }
                };
                out.push_str("cid:");
                out.push_str(&cid);
            }
            None => out.push_str(value),
        }
        rest = &rest[value_end..];
    }
    out.push_str(rest);
    (out, images)
}

/// Build the message: multipart/alternative (plain text from the HTML, then HTML — in
/// multipart/related with its images when it has any), inside multipart/mixed when there are
/// attachments.
pub fn build(msg: &Outgoing, message_id: &str, date: i64) -> Result<Vec<u8>> {
    use mail_builder::mime::MimePart;
    let (html, images) = inline_images(&msg.html, message_id);
    let text = plain_text(&html);
    let mut b = MessageBuilder::new().message_id(message_id).date(date).subject(msg.subject.as_str());
    if let Some(from) = &msg.from {
        b = b.from(address(std::slice::from_ref(from)));
    }
    if !msg.to.is_empty() {
        b = b.to(address(&msg.to));
    }
    if !msg.cc.is_empty() {
        b = b.cc(address(&msg.cc));
    }
    if let Some(id) = &msg.in_reply_to {
        b = b.in_reply_to(id.as_str());
    }
    if !msg.references.is_empty() {
        b = b.references(msg.references.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    }
    let html_part = if images.is_empty() {
        MimePart::new("text/html", html.as_str())
    } else {
        let mut parts = vec![MimePart::new("text/html", html.as_str())];
        for i in &images {
            parts.push(MimePart::new(i.content_type.as_str(), i.data.as_slice()).inline().cid(i.cid.as_str()));
        }
        MimePart::new("multipart/related", parts)
    };
    let alternative = MimePart::new("multipart/alternative", vec![MimePart::new("text/plain", text.as_str()), html_part]);
    let body = if msg.attachments.is_empty() {
        alternative
    } else {
        let mut parts = vec![alternative];
        for a in &msg.attachments {
            parts.push(MimePart::new(a.content_type.as_str(), a.data.as_slice()).attachment(a.name.as_str()));
        }
        MimePart::new("multipart/mixed", parts)
    };
    Ok(b.body(body).write_to_vec()?)
}

// ------------------------------------------------------------- replies and forwards

/// "Re: x" (once, whatever prefixes the original carried).
pub fn reply_subject(subject: &str) -> String {
    format!("Re: {}", strip_prefixes(subject))
}

/// "Fwd: x".
pub fn forward_subject(subject: &str) -> String {
    format!("Fwd: {}", strip_prefixes(subject))
}

/// Drop leading Re:/Fwd:/Fw:/AW:/WG:/SV: (any case, repeated, with "[n]" counters).
fn strip_prefixes(subject: &str) -> &str {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(prefix) = ["re", "fwd", "fw", "aw", "wg", "sv", "vs", "antw"].iter().find(|p| lower.starts_with(*p)) else {
            return s;
        };
        let rest = &s[prefix.len()..];
        let rest = rest.trim_start_matches(|c: char| c == '[' || c.is_ascii_digit() || c == ']');
        let Some(rest) = rest.strip_prefix(':') else { return s };
        s = rest.trim_start();
    }
}

/// The original message's headers that a reply needs.
#[derive(Debug, Clone, Default)]
pub struct Original {
    pub from: Vec<Addr>,
    pub reply_to: Vec<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub message_id: Option<String>,
    /// Its References, in order.
    pub references: Vec<String>,
    pub in_reply_to: Option<String>,
}

// ------------------------------------------------------------- quoting

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Plain text as HTML for quoting: escaped, line breaks kept.
pub fn text_to_html(text: &str) -> String {
    escape_html(text.trim_end()).replace("\r\n", "\n").replace('\n', "<br>\n")
}

/// "Jane Doe <jane@example.net>" (escaped), or just the address.
pub fn display_addr(a: &Addr) -> String {
    match a.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("{} &lt;{}&gt;", escape_html(n), escape_html(&a.addr)),
        None => escape_html(&a.addr),
    }
}

const QUOTE_STYLE: &str = "margin:0px 0px 0px 0.8ex;border-left:1px solid rgb(204,204,204);padding-left:1ex";

/// The original under a reply, in Gmail's markup (Gmail and Thunderbird fold it away; our own
/// reader recognises it too). `attribution`: "On …, Jane <jane@…> wrote:" (escaped HTML).
pub fn reply_quote(attribution: &str, original_html: &str) -> String {
    format!(
        "<div class=\"gmail_quote\"><div dir=\"ltr\" class=\"gmail_attr\">{attribution}<br></div>\
         <blockquote class=\"gmail_quote\" style=\"{QUOTE_STYLE}\">{original_html}</blockquote></div>"
    )
}

/// A forwarded message under the new text: Gmail's "Forwarded message" header block, then the
/// original. `headers`: (label, escaped value) pairs, e.g. ("From", …).
pub fn forward_quote(headers: &[(&str, String)], original_html: &str) -> String {
    let mut h = String::from("---------- Forwarded message ---------<br>");
    for (label, value) in headers {
        if !value.is_empty() {
            h.push_str(&format!("{label}: {value}<br>"));
        }
    }
    format!("<div class=\"gmail_quote\"><div dir=\"ltr\" class=\"gmail_attr\">{h}</div><br><br>{original_html}</div>")
}

/// To and Cc for a reply. `me`: this account's addresses (never replied to, unless the
/// original is our own message, then it goes back to its recipients).
pub fn reply_recipients(orig: &Original, me: &[String], all: bool) -> (Vec<Addr>, Vec<Addr>) {
    let mine = |a: &Addr| me.iter().any(|m| m.eq_ignore_ascii_case(a.addr.trim()));
    let from_me = !orig.from.is_empty() && orig.from.iter().all(mine);
    let mut to: Vec<Addr> = if from_me {
        // Replying to our own message: write to whom it went to.
        orig.to.clone()
    } else if !orig.reply_to.is_empty() {
        orig.reply_to.clone()
    } else {
        orig.from.clone()
    };
    let mut cc: Vec<Addr> = Vec::new();
    if all {
        let more = if from_me { orig.cc.clone() } else { orig.to.iter().chain(&orig.cc).cloned().collect() };
        cc = more;
    }
    dedupe(&mut to, &[], &mine);
    let to_now = to.clone();
    dedupe(&mut cc, &to_now, &mine);
    if to.is_empty() {
        // Everyone was us (a note to self): reply to ourselves.
        to = orig.from.clone();
    }
    (to, cc)
}

fn dedupe(list: &mut Vec<Addr>, already: &[Addr], mine: &dyn Fn(&Addr) -> bool) {
    let mut seen: Vec<String> = already.iter().map(|a| a.addr.trim().to_ascii_lowercase()).collect();
    list.retain(|a| {
        let k = a.addr.trim().to_ascii_lowercase();
        if k.is_empty() || mine(a) || seen.contains(&k) {
            return false;
        }
        seen.push(k);
        true
    });
}

/// In-Reply-To and References for a reply (RFC 5322 3.6.4): the original's References (or,
/// lacking those, its In-Reply-To) followed by its Message-ID.
pub fn reply_threading(orig: &Original) -> (Option<String>, Vec<String>) {
    let mut refs: Vec<String> = if !orig.references.is_empty() {
        orig.references.clone()
    } else {
        orig.in_reply_to.iter().cloned().collect()
    };
    if let Some(id) = &orig.message_id {
        refs.retain(|r| r != id);
        refs.push(id.clone());
    }
    (orig.message_id.clone(), refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_keeps_code() {
        let html = "<p>Run <code style=\"font-family:monospace\">age &lt;file&gt;</code> first:</p>\
                    <pre style=\"background:#f6f8fa\"><code>age() {\n    local t\n    [[ \"$t\" =~ ^[0-9]+$ ]] &amp;&amp; echo $((now - t))\n}</code></pre>\
                    <p>Then <b>bold</b> as usual.</p><p>A <code>&lt;pre&gt;</code> and a <prefix>tag</prefix>.</p>";
        let text = plain_text(html);
        assert!(text.contains("Run `age <file>` first:"), "{text}");
        assert!(
            text.contains("```\nage() {\n    local t\n    [[ \"$t\" =~ ^[0-9]+$ ]] && echo $((now - t))\n}\n```"),
            "lines and indentation kept, fenced: {text}"
        );
        assert!(text.contains("Then bold as usual."), "{text}");
        assert!(text.contains("A `<pre>` and a tag."), "{text}");
        // A horizontal line.
        let ruled = plain_text("<p>Above</p><hr style=\"border:0\"><p>Below</p><hr/>");
        assert!(ruled.contains("Above") && ruled.contains("---") && ruled.find("---") < ruled.find("Below"), "{ruled}");
        // A code block written with line breaks (<br>) and nothing else.
        assert_eq!(plain_text("<pre>a<br>  b</pre>").trim(), "```\na\n  b\n```");
        // No code: as the parser makes it.
        let plain = "<p>Hi &amp; bye</p><ul><li>one</li></ul>";
        assert_eq!(plain_text(plain), mail_parser::decoders::html::html_to_text(plain));
    }
    use mail_parser::MimeHeaders;

    fn a(addr: &str) -> Addr {
        Addr { name: None, addr: addr.into() }
    }
    fn n(name: &str, addr: &str) -> Addr {
        Addr { name: Some(name.into()), addr: addr.into() }
    }

    #[test]
    fn pasted_pictures_go_as_parts_of_the_message() {
        // a 1×1 PNG, pasted twice, and one that isn't a picture
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let html = format!(
            "<p>Look:</p><p><img src=\"data:image/png;base64,{png}\"></p><p><img src='data:image/png;base64,{png}' alt=\"again\"></p><p><img src=\"data:text/html;base64,PGI+\"></p>"
        );
        let msg = Outgoing {
            from: Some(a("john.doe@example.com")),
            to: vec![a("jane.doe@example.net")],
            subject: "Picture".into(),
            html,
            attachments: vec![OutAttachment { name: "notes.txt".into(), content_type: "text/plain".into(), data: b"hi".to_vec() }],
            ..Default::default()
        };
        let raw = build(&msg, "pic@example.com", 1_700_000_000).unwrap();
        let parsed = mail_parser::MessageParser::default().parse(&raw).unwrap();
        assert_eq!(parsed.content_type().map(|c| c.c_type.as_ref()), Some("multipart"));
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("multipart/mixed") && s.contains("multipart/related") && s.contains("multipart/alternative"), "{s}");
        let html = parsed.body_html(0).unwrap();
        assert_eq!(html.matches("src=\"cid:img1.pic@example.com\"").count() + html.matches("src='cid:img1.pic@example.com'").count(), 2, "{html}");
        assert!(!html.contains("data:image/") && html.contains("data:text/html"), "only pictures become parts: {html}");
        // one picture part, sent once; the file still an attachment
        let pictures = parsed.parts.iter().filter(|p| p.content_type().is_some_and(|c| c.c_type == "image")).count();
        assert_eq!(pictures, 1);
        assert!(parsed.attachments().any(|p| p.attachment_name() == Some("notes.txt")));
        // and the reader shows it in place, not as an attachment
        let d = crate::mime::render(1, &raw, 1_700_000_000).unwrap();
        assert!(d.html.unwrap().contains("data:image/png;base64"));
        assert_eq!(d.attachments.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["notes.txt"]);
    }

    #[test]
    fn builds_a_message_without_bcc() {
        let msg = Outgoing {
            from: Some(n("John Doe", "john.doe@example.com")),
            to: vec![n("Jane Doe", "jane.doe@example.net")],
            cc: vec![a("team@example.org")],
            bcc: vec![a("secret@example.org")],
            subject: "Grüße — plans".into(),
            html: "<p>Hello <b>Jane</b></p><ul><li>one</li></ul>".into(),
            attachments: vec![OutAttachment { name: "notes.txt".into(), content_type: "text/plain".into(), data: b"hi".to_vec() }],
            in_reply_to: Some("orig@example.net".into()),
            references: vec!["first@example.net".into(), "orig@example.net".into()],
        };
        let raw = build(&msg, "new@example.com", 1_700_000_000).unwrap();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("Message-ID: <new@example.com>"), "{s}");
        assert!(s.contains("In-Reply-To: <orig@example.net>"));
        assert!(s.contains("References: <first@example.net>"));
        assert!(s.contains("<orig@example.net>"));
        // (Not "no 'bcc' anywhere": MIME boundaries are random letters and can contain it.)
        assert!(!s.lines().any(|l| l.to_ascii_lowercase().starts_with("bcc:")) && !s.contains("secret@"), "Bcc must not be in the message");
        assert_eq!(msg.recipients(), vec!["jane.doe@example.net", "team@example.org", "secret@example.org"]);

        // Our own parser reads it back the way other clients will.
        let parsed = mail_parser::MessageParser::default().parse(&raw).unwrap();
        assert_eq!(parsed.subject(), Some("Grüße — plans"));
        assert_eq!(parsed.from().unwrap().first().unwrap().address(), Some("john.doe@example.com"));
        assert!(parsed.body_text(0).unwrap().contains("Hello Jane"));
        assert!(parsed.body_html(0).unwrap().contains("<b>Jane</b>"));
        assert_eq!(parsed.attachment(0).unwrap().attachment_name(), Some("notes.txt"));
    }

    #[test]
    fn pictures_in_the_html_go_as_inline_parts() {
        use base64::Engine;
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let html = format!(
            r#"<p>Hi</p><p><img src="data:image/png;base64,{b64}" width="120"> <img alt="again" src='data:image/png;base64,{b64}'></p><p>data:image/png is just text here</p>"#
        );
        let (out, images) = inline_images(&html, "m1@x");
        assert_eq!(images.len(), 1, "the same picture once");
        assert_eq!(images[0].content_type, "image/png");
        assert_eq!(images[0].data, png);
        assert_eq!(out.matches("cid:img1.m1@x").count(), 2);
        assert!(out.contains("data:image/png is just text here"), "text left alone");

        let msg = Outgoing {
            from: Some(a("me@x.com")),
            to: vec![a("you@y.com")],
            subject: "Logo".into(),
            html,
            attachments: vec![OutAttachment { name: "a.txt".into(), content_type: "text/plain".into(), data: b"hi".to_vec() }],
            ..Default::default()
        };
        let raw = build(&msg, "m1@x", 1_700_000_000).unwrap();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("multipart/related") && s.contains("multipart/mixed"), "{s}");
        assert!(!s.contains("data:image/png;base64"), "no data: URL left");
        let parsed = mail_parser::MessageParser::default().parse(&raw).unwrap();
        assert!(parsed.body_html(0).unwrap().contains("cid:img1.m1@x"));
        let parts: Vec<_> = parsed.attachments().collect();
        let inline = parts.iter().find(|p| p.content_id() == Some("img1.m1@x")).expect("the picture's part");
        assert_eq!(inline.contents(), png.as_slice());
        assert!(parts.iter().any(|p| p.attachment_name() == Some("a.txt")));
        // No pictures: as before.
        let plain = build(&Outgoing { html: "<p>x</p>".into(), ..msg.clone() }, "m2@x", 0).unwrap();
        assert!(!String::from_utf8_lossy(&plain).contains("multipart/related"));
    }

    #[test]
    fn a_message_forwarded_as_an_attachment() {
        let inner = b"From: a@x.com\r\nTo: b@y.com\r\nSubject: Original\r\nMessage-ID: <o@x>\r\n\r\nHello there\r\n".to_vec();
        let msg = Outgoing {
            from: Some(a("me@x.com")),
            to: vec![a("you@y.com")],
            subject: "Fwd: 1 message".into(),
            html: "<p>See attached</p>".into(),
            attachments: vec![OutAttachment { name: "Original.eml".into(), content_type: "message/rfc822".into(), data: inner }],
            ..Default::default()
        };
        let raw = build(&msg, "f@x", 0).unwrap();
        let parsed = mail_parser::MessageParser::default().parse(&raw).unwrap();
        let part = parsed.attachment(0).unwrap();
        let nested = part.message().expect("read as a message");
        assert_eq!(nested.subject(), Some("Original"));
        assert!(nested.body_text(0).unwrap().contains("Hello there"));
    }

    #[test]
    fn subjects() {
        assert_eq!(reply_subject("Plans"), "Re: Plans");
        assert_eq!(reply_subject("RE: re: Fwd: Plans"), "Re: Plans");
        assert_eq!(reply_subject("AW: Re[2]: Plans"), "Re: Plans");
        assert_eq!(forward_subject("Re: Plans"), "Fwd: Plans");
        assert_eq!(reply_subject("Reading list"), "Re: Reading list", "a word starting with 're' isn't a prefix");
    }

    #[test]
    fn recipients_for_reply_and_reply_all() {
        let me = vec!["john.doe@example.com".to_string()];
        let orig = Original {
            from: vec![n("Jane", "jane@example.net")],
            to: vec![a("John.Doe@Example.com"), a("bob@example.org")],
            cc: vec![a("carol@example.org"), a("jane@example.net")],
            ..Default::default()
        };
        assert_eq!(reply_recipients(&orig, &me, false), (vec![n("Jane", "jane@example.net")], vec![]));
        let (to, cc) = reply_recipients(&orig, &me, true);
        assert_eq!(to, vec![n("Jane", "jane@example.net")]);
        assert_eq!(cc, vec![a("bob@example.org"), a("carol@example.org")], "no me, no duplicate of To");

        // Reply-To wins over From.
        let list = Original { reply_to: vec![a("list@example.org")], ..orig.clone() };
        assert_eq!(reply_recipients(&list, &me, false).0, vec![a("list@example.org")]);

        // Replying to our own sent message goes to its recipients.
        let sent = Original { from: vec![a("john.doe@example.com")], to: vec![a("bob@example.org")], cc: vec![a("carol@example.org")], ..Default::default() };
        assert_eq!(reply_recipients(&sent, &me, true), (vec![a("bob@example.org")], vec![a("carol@example.org")]));
    }

    #[test]
    fn quotes() {
        assert_eq!(text_to_html("a < b\r\nc & d\n"), "a &lt; b<br>\nc &amp; d");
        assert_eq!(display_addr(&n("Jane <x>", "jane@example.net")), "Jane &lt;x&gt; &lt;jane@example.net&gt;");
        let q = reply_quote("On Thu, Jane wrote:", "<p>hi</p>");
        assert!(q.starts_with("<div class=\"gmail_quote\"><div dir=\"ltr\" class=\"gmail_attr\">On Thu, Jane wrote:<br></div><blockquote"));
        assert!(q.ends_with("<p>hi</p></blockquote></div>"));
        let f = forward_quote(&[("From", "Jane".into()), ("Cc", String::new()), ("Subject", "Plans".into())], "<p>body</p>");
        assert!(f.contains("Forwarded message") && f.contains("From: Jane<br>") && f.contains("Subject: Plans<br>"));
        assert!(!f.contains("Cc:"), "empty headers are left out");

        // A reply as sent: threading headers, our text, then the quote.
        let msg = Outgoing {
            from: Some(a("john.doe@example.com")),
            to: vec![n("Jane", "jane@example.net")],
            subject: reply_subject("Plans"),
            html: format!("<p>Sounds good.</p>{q}"),
            in_reply_to: Some("c@x".into()),
            references: vec!["a@x".into(), "c@x".into()],
            ..Default::default()
        };
        let parsed_raw = build(&msg, "r@example.com", 1_700_000_000).unwrap();
        let parsed = mail_parser::MessageParser::default().parse(&parsed_raw).unwrap();
        assert_eq!(parsed.subject(), Some("Re: Plans"));
        assert_eq!(parsed.in_reply_to().as_text(), Some("c@x"));
        let text = parsed.body_text(0).unwrap();
        assert!(text.contains("Sounds good.") && text.contains("Jane wrote:") && text.contains("hi"), "{text}");
    }

    #[test]
    fn threading_headers() {
        let orig = Original {
            message_id: Some("c@x".into()),
            references: vec!["a@x".into(), "b@x".into()],
            ..Default::default()
        };
        assert_eq!(reply_threading(&orig), (Some("c@x".into()), vec!["a@x".into(), "b@x".into(), "c@x".into()]));
        let first = Original { message_id: Some("b@x".into()), in_reply_to: Some("a@x".into()), ..Default::default() };
        assert_eq!(reply_threading(&first).1, vec!["a@x".to_string(), "b@x".to_string()]);
    }
}

