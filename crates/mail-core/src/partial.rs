//! Body-only downloads: fetch a message's text parts (and the inline images its HTML needs)
//! without its attachments, then rebuild a valid MIME message from those parts.
//!
//! The rebuilt message keeps the original headers and multipart tree minus the omitted parts,
//! and carries `X-Mail-Partial: attachments-omitted`. Omitted attachments are described by
//! [`Attachment`] records stored next to the body, so the reader can still list them.

use std::borrow::Cow;

use imap_proto::{BodyStructure, ContentEncoding};
use serde::{Deserialize, Serialize};

/// Inline images larger than this are treated as attachments.
const MAX_INLINE_IMAGE: u32 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OmittedPart {
    pub name: String,
    pub mime: String,
    /// Decoded size estimate in bytes.
    pub size: usize,
    /// IMAP section (e.g. "2" or "1.3") to fetch it later; absent in entries from before
    /// attachments could be downloaded (then BODYSTRUCTURE is fetched again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Content-Transfer-Encoding to undo after fetching ("base64", "quoted-printable", ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Multipart { subtype: String, children: Vec<Node> },
    Leaf { path: Vec<u32> },
}

/// What to fetch for one message. `None` from [`plan`] means "no attachments: fetch whole".
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Kept parts; `None` if nothing displayable remains (e.g. a bare PDF message).
    pub tree: Option<Node>,
    pub omitted: Vec<OmittedPart>,
}

impl Plan {
    /// Leaf section paths in tree order.
    pub fn leaves(&self) -> Vec<Vec<u32>> {
        fn walk(n: &Node, out: &mut Vec<Vec<u32>>) {
            match n {
                Node::Leaf { path } => out.push(path.clone()),
                Node::Multipart { children, .. } => children.iter().for_each(|c| walk(c, out)),
            }
        }
        let mut out = Vec::new();
        if let Some(t) = &self.tree {
            walk(t, &mut out);
        }
        out
    }

    /// The FETCH items needed: the top-level header plus MIME header and content of each leaf.
    pub fn fetch_items(&self) -> String {
        let mut items = vec!["UID".to_string(), "BODY.PEEK[HEADER]".to_string()];
        for p in self.leaves() {
            let p = section(&p);
            items.push(format!("BODY.PEEK[{p}.MIME]"));
            items.push(format!("BODY.PEEK[{p}]"));
        }
        format!("({})", items.join(" "))
    }
}

pub fn section(path: &[u32]) -> String {
    path.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(".")
}

fn param<'a>(params: &'a imap_proto::BodyParams<'_>, key: &str) -> Option<&'a str> {
    params
        .as_ref()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v): &(Cow<'_, str>, Cow<'_, str>)| v.as_ref())
}

fn decoded_size(octets: u32, enc: &ContentEncoding) -> usize {
    match enc {
        ContentEncoding::Base64 => octets as usize * 3 / 4,
        _ => octets as usize,
    }
}

/// Decide which parts to keep. Returns `None` when the message has no attachments.
pub fn plan(bs: &BodyStructure) -> Option<Plan> {
    let mut omitted = Vec::new();
    let tree = match bs {
        BodyStructure::Multipart { .. } => walk(bs, &[], &mut omitted),
        // A non-multipart message's content is section 1.
        _ => walk(bs, &[1], &mut omitted),
    };
    // (A single-part message that is itself an attachment yields no tree: nothing to show.)
    (!omitted.is_empty()).then_some(Plan { tree, omitted })
}

fn walk(bs: &BodyStructure, path: &[u32], omitted: &mut Vec<OmittedPart>) -> Option<Node> {
    let child_path = |i: usize| {
        let mut p = path.to_vec();
        p.push(i as u32 + 1);
        p
    };
    match bs {
        BodyStructure::Multipart { common, bodies, .. } => {
            let children: Vec<Node> =
                bodies.iter().enumerate().filter_map(|(i, b)| walk(b, &child_path(i), omitted)).collect();
            (!children.is_empty())
                .then(|| Node::Multipart { subtype: common.ty.subtype.to_ascii_lowercase(), children })
        }
        BodyStructure::Text { common, other, .. } => {
            if is_attachment(common) {
                omitted.push(describe(common, other, path, "attachment.txt"));
                None
            } else {
                Some(Node::Leaf { path: path.to_vec() })
            }
        }
        BodyStructure::Basic { common, other, .. } => {
            let inline_image = common.ty.ty.eq_ignore_ascii_case("image")
                && other.id.is_some()
                && !is_attachment(common)
                && other.octets <= MAX_INLINE_IMAGE;
            if inline_image {
                Some(Node::Leaf { path: path.to_vec() })
            } else {
                omitted.push(describe(common, other, path, "attachment"));
                None
            }
        }
        BodyStructure::Message { common, other, .. } => {
            omitted.push(describe(common, other, path, "forwarded message.eml"));
            None
        }
    }
}

fn is_attachment(common: &imap_proto::BodyContentCommon) -> bool {
    common.disposition.as_ref().is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"))
}

fn describe(
    common: &imap_proto::BodyContentCommon,
    other: &imap_proto::BodyContentSinglePart,
    path: &[u32],
    fallback: &str,
) -> OmittedPart {
    let name = common
        .disposition
        .as_ref()
        .and_then(|d| param(&d.params, "filename"))
        .or_else(|| param(&common.ty.params, "name"))
        .map(|n| crate::mime::decode_rfc2047(n))
        .unwrap_or_else(|| fallback.to_string());
    OmittedPart {
        name,
        mime: format!("{}/{}", common.ty.ty, common.ty.subtype).to_ascii_lowercase(),
        size: decoded_size(other.octets, &other.transfer_encoding),
        section: Some(section(path)),
        encoding: Some(
            match &other.transfer_encoding {
                ContentEncoding::SevenBit => "7bit",
                ContentEncoding::EightBit => "8bit",
                ContentEncoding::Binary => "binary",
                ContentEncoding::Base64 => "base64",
                ContentEncoding::QuotedPrintable => "quoted-printable",
                ContentEncoding::Other(o) => o.as_ref(),
            }
            .to_ascii_lowercase(),
        ),
    }
}

/// Fetched section contents, by section path.
pub trait Sections {
    fn header(&self) -> Option<&[u8]>;
    fn mime(&self, path: &[u32]) -> Option<&[u8]>;
    fn content(&self, path: &[u32]) -> Option<&[u8]>;
}

/// Reassemble a MIME message from the fetched parts.
pub fn rebuild(plan: &Plan, s: &dyn Sections, seed: u32) -> Option<Vec<u8>> {
    let header = s.header()?;
    let mut out = strip_fields(header, &["content-type", "content-transfer-encoding"]);
    out.extend_from_slice(b"X-Mail-Partial: attachments-omitted\r\n");
    match &plan.tree {
        None => out.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n\r\n"),
        Some(node) => {
            let (ct, body) = build(node, s, &format!("{seed:x}"))?;
            out.extend_from_slice(&ct);
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(&body);
        }
    }
    Some(out)
}

/// Returns (header lines, body) for a node.
/// `tag` makes each multipart's boundary unique within the message.
fn build(node: &Node, s: &dyn Sections, tag: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    match node {
        Node::Leaf { path } => {
            let mime = s.mime(path)?;
            // MIME headers end with a blank line; keep the fields, drop the separator.
            let fields = mime.strip_suffix(b"\r\n\r\n").or_else(|| mime.strip_suffix(b"\n\n")).unwrap_or(mime);
            let mut h = fields.to_vec();
            h.extend_from_slice(b"\r\n");
            Some((h, s.content(path)?.to_vec()))
        }
        Node::Multipart { subtype, children } => {
            let boundary = format!("=_partial_{tag}");
            let mut body = Vec::new();
            for (i, child) in children.iter().enumerate() {
                let (h, b) = build(child, s, &format!("{tag}_{i}"))?;
                body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                body.extend_from_slice(&h);
                body.extend_from_slice(b"\r\n");
                body.extend_from_slice(&b);
                body.extend_from_slice(b"\r\n");
            }
            body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
            let h = format!("Content-Type: multipart/{subtype}; boundary=\"{boundary}\"\r\n").into_bytes();
            Some((h, body))
        }
    }
}

/// Copy a header block without the named fields (including their folded continuation lines)
/// and without the terminating blank line.
fn strip_fields(header: &[u8], names: &[&str]) -> Vec<u8> {
    let text = String::from_utf8_lossy(header);
    let mut out = String::with_capacity(text.len());
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if !skipping {
                out.push_str(line);
            }
            continue;
        }
        let name = line.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
        skipping = names.contains(&name.as_str());
        if !skipping {
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\r\n");
            }
        }
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake {
        header: Vec<u8>,
        parts: HashMap<String, (Vec<u8>, Vec<u8>)>,
    }
    impl Sections for Fake {
        fn header(&self) -> Option<&[u8]> {
            Some(&self.header)
        }
        fn mime(&self, p: &[u32]) -> Option<&[u8]> {
            self.parts.get(&section(p)).map(|x| x.0.as_slice())
        }
        fn content(&self, p: &[u32]) -> Option<&[u8]> {
            self.parts.get(&section(p)).map(|x| x.1.as_slice())
        }
    }

    #[test]
    fn rebuilds_alternative_without_attachment() {
        // multipart/mixed( multipart/alternative(text, html), application/pdf )
        let plan = Plan {
            tree: Some(Node::Multipart {
                subtype: "mixed".into(),
                children: vec![Node::Multipart {
                    subtype: "alternative".into(),
                    children: vec![Node::Leaf { path: vec![1, 1] }, Node::Leaf { path: vec![1, 2] }],
                }],
            }),
            omitted: vec![OmittedPart { name: "a.pdf".into(), mime: "application/pdf".into(), size: 10, section: None, encoding: None }],
        };
        assert_eq!(
            plan.fetch_items(),
            "(UID BODY.PEEK[HEADER] BODY.PEEK[1.1.MIME] BODY.PEEK[1.1] BODY.PEEK[1.2.MIME] BODY.PEEK[1.2])"
        );
        let fake = Fake {
            header: b"From: a@b.c\r\nSubject: Hi\r\nContent-Type: multipart/mixed;\r\n\tboundary=\"old\"\r\nMIME-Version: 1.0\r\n\r\n".to_vec(),
            parts: HashMap::from([
                ("1.1".into(), (b"Content-Type: text/plain\r\n\r\n".to_vec(), b"plain body".to_vec())),
                ("1.2".into(), (b"Content-Type: text/html\r\n\r\n".to_vec(), b"<p>html body</p>".to_vec())),
            ]),
        };
        let raw = rebuild(&plan, &fake, 7).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.contains("boundary=\"old\""));
        assert!(text.contains("MIME-Version: 1.0"));

        let msg = mail_parser::MessageParser::default().parse(&raw).unwrap();
        assert_eq!(msg.subject(), Some("Hi"));
        assert_eq!(msg.body_html(0).unwrap(), "<p>html body</p>");
        assert_eq!(msg.body_text(0).unwrap(), "plain body");
        assert_eq!(msg.attachment_count(), 0);
    }
}
