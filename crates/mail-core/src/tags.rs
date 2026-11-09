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
    !k.is_empty()
        && !k.starts_with('\\')
        && !NOT_TAGS.contains(&k.as_str())
        && !k.starts_with(SNOOZE_PREFIX)
        && !k.starts_with(WOKE_PREFIX)
}

// ------------------------------------------------------------- snooze

/// Snooze is a reminder: the message stays where it is, with `$snooze-YYYYMMDDHHMM` (UTC) on
/// it. When that time comes, the first client to notice swaps it for `$woke-YYYYMMDDHHMM` (same
/// time), marks the message unread and brings it back to the Inbox if it left; the other
/// clients see the swap and leave it alone. The `$woke-` keyword makes every client sort the
/// message in the Inbox by that time (see `store::raise_woken`), and it goes when the message
/// leaves the Inbox.
pub const SNOOZE_PREFIX: &str = "$snooze-";
/// See [`SNOOZE_PREFIX`].
pub const WOKE_PREFIX: &str = "$woke-";

fn timed_keyword(prefix: &str, at: i64) -> String {
    let t = chrono::DateTime::from_timestamp(at, 0).unwrap_or_default();
    format!("{prefix}{}", t.format("%Y%m%d%H%M"))
}

fn keyword_time(prefix: &str, keyword: &str) -> Option<i64> {
    let k = normalize(keyword);
    let digits = k.strip_prefix(prefix)?;
    let t = chrono::NaiveDateTime::parse_from_str(digits, "%Y%m%d%H%M").ok()?;
    Some(t.and_utc().timestamp())
}

/// The keyword for a snooze until `until` (Unix seconds, rounded down to the minute).
pub fn snooze_keyword(until: i64) -> String {
    timed_keyword(SNOOZE_PREFIX, until)
}

/// The keyword for a snooze that went off at `at`.
pub fn woke_keyword(at: i64) -> String {
    timed_keyword(WOKE_PREFIX, at)
}

/// When a snooze keyword says to wake (Unix seconds); None for any other keyword.
pub fn snooze_time(keyword: &str) -> Option<i64> {
    keyword_time(SNOOZE_PREFIX, keyword)
}

/// When a woke keyword says the snooze went off; None for any other keyword.
pub fn woke_time(keyword: &str) -> Option<i64> {
    keyword_time(WOKE_PREFIX, keyword)
}

/// The pending snooze time among stored keywords (the latest, should there be several).
pub fn snoozed_until(stored: &str) -> Option<i64> {
    stored.split_whitespace().filter_map(snooze_time).max()
}

/// When the message's snooze went off, from stored keywords (the latest).
pub fn woken_at(stored: &str) -> Option<i64> {
    stored.split_whitespace().filter_map(woke_time).max()
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
    fn snooze_keywords() {
        let t = 1_794_236_400; // 2026-11-09 15:00 UTC
        assert_eq!(snooze_keyword(t), "$snooze-202611091500");
        assert_eq!(snooze_keyword(t + 59), "$snooze-202611091500");
        assert_eq!(snooze_time("$Snooze-202611091500"), Some(t));
        assert_eq!(snooze_time("$snooze-junk"), None);
        assert_eq!(snooze_time("$label1"), None);
        assert_eq!(snoozed_until("$label1 $snooze-202611091500 $forwarded"), Some(t));
        assert!(!is_tag("$snooze-202611091500"));
        assert_eq!(visible("$label1 $snooze-202611091500"), vec!["$label1"]);
        assert_eq!(woke_keyword(t), "$woke-202611091500");
        assert_eq!(woke_time("$woke-202611091500"), Some(t));
        assert_eq!(snooze_time("$woke-202611091500"), None);
        assert_eq!(woken_at("$snooze-202611091500"), None);
        assert_eq!(woken_at(" $woke-202611091500 "), Some(t));
        assert!(!is_tag("$woke-202611091500"));
    }

    #[test]
    fn keywords() {
        assert_eq!(join(&["$Label2".into(), "work".into(), "$label2".into()]), " $label2 work ");
        assert_eq!(visible(" $forwarded $label1 junk projectx "), vec!["$label1", "projectx"]);
        assert_eq!(keyword_for_name("Waiting On (Bob)"), "waiting_on__bob_");
        assert_eq!(keyword_for_name("Ünicode"), "_nicode");
        assert!(!is_tag("\\Seen") && !is_tag("$MDNSent") && is_tag("$label5"));
    }
}
