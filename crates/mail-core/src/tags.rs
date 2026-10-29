//! Message tags, compatible with Thunderbird.
//!
//! A tag is an IMAP keyword on the message (`STORE +FLAGS ($label2)`), so it lives on the server
//! and every client sees it. Thunderbird's five default tags use `$label1`..`$label5`; custom
//! tags use a keyword derived from the tag's name. Names and colours are not on the server: like
//! Thunderbird, we keep them locally (the `tags` table), seeded with Thunderbird's defaults so
//! existing tags look the same here. Keywords found on messages without a definition are still
//! shown, under their keyword.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TagDef {
    /// IMAP keyword, lowercase (e.g. `$label1`, `waitingon`).
    pub keyword: String,
    pub name: String,
    /// CSS colour, e.g. `#FF0000`.
    pub color: String,
    pub position: i64,
    /// False for keywords seen on messages but never defined here (shown with a generated colour).
    pub defined: bool,
}

/// Thunderbird's default tags (same keywords, names and colours).
pub const DEFAULTS: &[(&str, &str, &str)] = &[
    ("$label1", "Important", "#FF0000"),
    ("$label2", "Work", "#FF9900"),
    ("$label3", "Personal", "#009900"),
    ("$label4", "To Do", "#3333FF"),
    ("$label5", "Later", "#993399"),
];

/// Keywords that are bookkeeping, not tags (forwarded/MDN markers, junk training, phishing).
const NOT_TAGS: &[&str] = &[
    "$forwarded", "$mdnsent", "$junk", "$notjunk", "junk", "nonjunk", "notjunk", "junkrecorded",
    "$phishing", "$notphishing", "$submitpending", "$submitted", "$hasattachment", "$hasnoattachment",
    "$autojunk", "$autonotjunk", "$important", "$muted", "$followed", "$ismailinglist", "$isnotification",
    "$istrusted", "$seen", "nonjunk", "$label0",
];

/// Normalise a keyword for storage and comparison (IMAP keywords are case-insensitive).
pub fn normalize(k: &str) -> String {
    k.trim().to_ascii_lowercase()
}

/// Whether a message keyword should be shown as a tag.
pub fn is_tag(keyword: &str) -> bool {
    let k = normalize(keyword);
    !k.is_empty() && !k.starts_with('\\') && !NOT_TAGS.contains(&k.as_str())
}

/// Parse the stored space-separated keyword list.
pub fn parse(stored: &str) -> Vec<String> {
    stored.split_whitespace().map(normalize).collect()
}

/// Store keywords as a space-padded string (" a b ") so `LIKE '% a %'` finds whole keywords.
pub fn join(keywords: &[String]) -> String {
    let mut v: Vec<String> = keywords.iter().map(|k| normalize(k)).filter(|k| !k.is_empty()).collect();
    v.sort();
    v.dedup();
    if v.is_empty() { String::new() } else { format!(" {} ", v.join(" ")) }
}

/// Whether a message's stored keywords mark it as forwarded (`$Forwarded`, set by Thunderbird
/// and by us).
pub fn is_forwarded(stored: &str) -> bool {
    parse(stored).iter().any(|k| k == "$forwarded")
}

/// The visible tags among a message's stored keywords.
pub fn visible(stored: &str) -> Vec<String> {
    parse(stored).into_iter().filter(|k| is_tag(k)).collect()
}

/// Thunderbird's keyword for a new custom tag: the name lowercased, with characters that aren't
/// allowed in an IMAP flag atom replaced by `_`.
pub fn keyword_for_name(name: &str) -> String {
    let k: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_graphic() && !"(){%*\"\\]".contains(c) { c } else { '_' })
        .collect();
    if k.is_empty() { "_".into() } else { k }
}

/// A stable colour for keywords without a definition.
pub fn generated_color(keyword: &str) -> String {
    const PALETTE: &[&str] = &["#0E7C86", "#B0446A", "#5B6BC0", "#8A6D1F", "#3B7D3A", "#9C4DCC", "#C2571A", "#4A7A9C"];
    let h = keyword.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    PALETTE[h as usize % PALETTE.len()].into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords() {
        assert_eq!(join(&["$Label2".into(), "work".into(), "$label2".into()]), " $label2 work ");
        assert_eq!(visible(" $forwarded $label1 junk projectx "), vec!["$label1", "projectx"]);
        assert_eq!(keyword_for_name("Waiting On (Bob)"), "waiting_on__bob_");
        assert_eq!(keyword_for_name("Ünicode"), "_nicode");
        assert!(!is_tag("\\Seen") && !is_tag("$MDNSent") && is_tag("$label5"));
    }
}
