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

/// Build the message: multipart/alternative (plain text from the HTML, then HTML), inside
/// multipart/mixed when there are attachments.
pub fn build(msg: &Outgoing, message_id: &str, date: i64) -> Result<Vec<u8>> {
    let text = mail_parser::decoders::html::html_to_text(&msg.html);
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
    b = b.text_body(text).html_body(msg.html.as_str());
    for a in &msg.attachments {
        b = b.attachment(a.content_type.as_str(), a.name.as_str(), a.data.as_slice());
    }
    Ok(b.write_to_vec()?)
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
    use mail_parser::MimeHeaders;

    fn a(addr: &str) -> Addr {
        Addr { name: None, addr: addr.into() }
    }
    fn n(name: &str, addr: &str) -> Addr {
        Addr { name: Some(name.into()), addr: addr.into() }
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
        assert!(!s.to_ascii_lowercase().contains("bcc") && !s.contains("secret@"), "Bcc must not be in the message");
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
