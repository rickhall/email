//! Microsoft Graph for what Exchange's IMAP can't keep: a Microsoft account's tags and snooze.
//!
//! Exchange stores no custom IMAP keywords, so on an account the user has allowed it for
//! (`Account::graph`):
//! - tags are Outlook **categories** on the message (by the tag's name), so Outlook on every
//!   device shows them, and the account's category list holds their colours;
//! - the app's own markers (`$snooze-…`, `$woke-…`) go in a property of the app's own on the
//!   message, invisible to other programs, with a second one ("marked") to find them by.
//!
//! Messages are found by their Message-ID (`internetMessageId`): every copy, in any folder.
//! Changes made here go out through the outbox (`actions::ServerOp::Graph`); changes made
//! elsewhere come in by `snapshot`, every few minutes (see `sync::graph_task`).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::oauth::MicrosoftSession;

const HOST: &str = "graph.microsoft.com";
/// The app's markers on a message, space-separated (a string extended property).
pub const MARKERS: &str = "String {8c6f0f3e-5b1d-4a57-9d2e-3f1a7c4b9e21} Name EmailAppKeywords";
/// "1" while a message has markers: what finds them (Graph filters properties by value only).
pub const MARKED: &str = "String {8c6f0f3e-5b1d-4a57-9d2e-3f1a7c4b9e21} Name EmailAppMarked";

/// Graph turned a change down for good (the message is gone, the permission was withdrawn):
/// the outbox rolls it back instead of retrying.
#[derive(Debug)]
pub struct GraphRejected(pub String);

impl std::fmt::Display for GraphRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for GraphRejected {}

/// Outlook's category colours (`preset0`…`preset24`), as shown in Outlook on the web.
const PRESETS: [(&str, u32); 25] = [
    ("preset0", 0xE74856),  // red
    ("preset1", 0xFF8C00),  // orange
    ("preset2", 0xAB6B3F),  // brown
    ("preset3", 0xFFF100),  // yellow
    ("preset4", 0x47D041),  // green
    ("preset5", 0x30C6CC),  // teal
    ("preset6", 0x73AA24),  // olive
    ("preset7", 0x4A9BFF),  // blue
    ("preset8", 0xA780FF),  // purple
    ("preset9", 0xEE5FB7),  // cranberry
    ("preset10", 0x949FA7), // steel
    ("preset11", 0x4C5961), // dark steel
    ("preset12", 0xABABAB), // gray
    ("preset13", 0x666666), // dark gray
    ("preset14", 0x474747), // black
    ("preset15", 0x910A19), // dark red
    ("preset16", 0xCE4B28), // dark orange
    ("preset17", 0x99684A), // dark brown
    ("preset18", 0xFFB700), // dark yellow
    ("preset19", 0x309B36), // dark green
    ("preset20", 0x2D9C9C), // dark teal
    ("preset21", 0x7D8A00), // dark olive
    ("preset22", 0x2858A8), // dark blue
    ("preset23", 0x6A3B9C), // dark purple
    ("preset24", 0xA40F6B), // dark cranberry
];

fn rgb(hex: &str) -> Option<(i32, i32, i32)> {
    let h = hex.trim().trim_start_matches('#');
    let v = u32::from_str_radix(h.get(..6)?, 16).ok()?;
    Some(((v >> 16) as i32, ((v >> 8) & 0xff) as i32, (v & 0xff) as i32))
}

/// Hue (0–1), lightness and saturation of an RGB colour.
fn hls((r, g, b): (i32, i32, i32)) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, l, 0.0);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, l, s)
}

/// How unlike two colours look: hue first (red should find red, not a darker orange that's
/// nearer in RGB), then saturation and lightness; greys only match greys.
fn unlike(a: (i32, i32, i32), b: (i32, i32, i32)) -> f64 {
    let ((ha, la, sa), (hb, lb, sb)) = (hls(a), hls(b));
    if sa < 0.15 || sb < 0.15 {
        let both_grey = sa < 0.15 && sb < 0.15;
        return (sa - sb).powi(2) * 4.0 + (la - lb).powi(2) * 2.0 + if both_grey { 0.0 } else { 1.0 };
    }
    let dh = (ha - hb).abs();
    let dh = dh.min(1.0 - dh) * 2.0;
    dh * dh * 3.0 + (sa - sb).powi(2) + (la - lb).powi(2) * 2.0
}

/// The Outlook colour nearest a tag's (CSS `#rrggbb`).
pub fn preset_for(hex: &str) -> &'static str {
    let Some(c) = rgb(hex) else { return "preset12" };
    let split = |v: u32| ((v >> 16) as i32, ((v >> 8) & 0xff) as i32, (v & 0xff) as i32);
    PRESETS
        .iter()
        .min_by(|x, y| unlike(c, split(x.1)).total_cmp(&unlike(c, split(y.1))))
        .map(|p| p.0)
        .unwrap_or("preset12")
}

/// A category colour as a tag colour (`#rrggbb`); "none" and unknown ones grey.
pub fn color_for(preset: &str) -> String {
    let c = PRESETS.iter().find(|p| p.0.eq_ignore_ascii_case(preset)).map(|p| p.1).unwrap_or(0x949FA7);
    format!("#{c:06X}")
}

/// An OData string literal ('it''s').
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Percent-encode a query value (spaces as %20, which Graph wants).
fn encode(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
}

/// Message-IDs as Graph has them (`<…>`) and as stored here (without).
fn bare(id: &str) -> String {
    id.trim().trim_start_matches('<').trim_end_matches('>').to_string()
}

/// One copy of a message, as found by its Message-ID.
#[derive(Debug, Clone)]
struct Item {
    id: String,
    categories: Vec<String>,
    markers: Vec<String>,
}

fn markers_of(v: &Value) -> Vec<String> {
    v["singleValueExtendedProperties"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["id"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(MARKERS)))
        .filter_map(|p| p["value"].as_str())
        .flat_map(|s| s.split_whitespace().map(crate::tags::normalize))
        .collect()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

/// What Graph holds for an account's messages, by Message-ID (without <>).
#[derive(Debug, Default)]
pub struct Snapshot {
    /// The account's categories: (name, colour as `#rrggbb`).
    pub categories: Vec<(String, String)>,
    /// Category names on each message (those in the account's list).
    pub tagged: HashMap<String, Vec<String>>,
    /// The app's markers on each message.
    pub markers: HashMap<String, Vec<String>>,
}

/// A change to one message's tags or markers.
pub enum Change<'a> {
    /// A tag, as the category `name` (made in the account's list, in `color`, if missing).
    Category { name: &'a str, color: &'a str, on: bool },
    /// One of the app's markers (`$snooze-…`).
    Marker { keyword: &'a str, on: bool },
}

pub struct Graph {
    session: Arc<MicrosoftSession>,
}

impl Graph {
    pub fn new(session: Arc<MicrosoftSession>) -> Graph {
        Graph { session }
    }

    /// One request; JSON back (Null for an empty answer). A rejected token is renewed and the
    /// request tried once more.
    async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        for attempt in 0..2 {
            let token = self.session.access_token().await?;
            let auth = format!("Bearer {token}");
            let mut headers = vec![("Authorization", auth.as_str())];
            if !body.is_empty() {
                headers.push(("Content-Type", "application/json"));
            }
            let (status, text) = crate::oauth::https(HOST, method, path, &headers, &body).await?;
            if status == 401 && attempt == 0 {
                self.session.forget(&token).await;
                continue;
            }
            if (200..300).contains(&status) {
                return Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? });
            }
            let v: Value = serde_json::from_str(&text).unwrap_or_default();
            let why = v["error"]["message"].as_str().unwrap_or(text.trim()).chars().take(300).collect::<String>();
            if matches!(status, 400 | 403 | 404) {
                return Err(GraphRejected(format!("Microsoft Graph refused ({status}): {why}")).into());
            }
            bail!("Microsoft Graph: HTTP {status}: {why}");
        }
        unreachable!()
    }

    /// GET a list, following its pages.
    async fn list(&self, first: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut next = Some(first.to_string());
        while let Some(path) = next.take() {
            let v = self.call("GET", &path, None).await?;
            out.extend(v["value"].as_array().cloned().unwrap_or_default());
            next = v["@odata.nextLink"].as_str().and_then(|u| u.strip_prefix("https://graph.microsoft.com")).map(str::to_string);
        }
        Ok(out)
    }

    /// The account's categories: (name, colour).
    pub async fn categories(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .list("/v1.0/me/outlook/masterCategories")
            .await?
            .iter()
            .filter_map(|c| Some((c["displayName"].as_str()?.to_string(), color_for(c["color"].as_str().unwrap_or("none")))))
            .collect())
    }

    /// Make sure the account's list has a category `name` (new ones in the colour nearest
    /// `color`). Without permission for the list, the message still gets its category.
    async fn ensure_category(&self, name: &str, color: &str) -> Result<()> {
        let have = match self.categories().await {
            Ok(c) => c,
            Err(e) => {
                tracing::info!("reading Outlook categories: {e:#}");
                return Ok(());
            }
        };
        if have.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            return Ok(());
        }
        let body = json!({ "displayName": name, "color": preset_for(color) });
        if let Err(e) = self.call("POST", "/v1.0/me/outlook/masterCategories", Some(&body)).await {
            tracing::info!("adding the Outlook category {name:?}: {e:#}");
        }
        Ok(())
    }

    /// Every copy of the message with this Message-ID.
    async fn find(&self, message_id: &str) -> Result<Vec<Item>> {
        let filter = format!("internetMessageId eq {}", literal(&format!("<{}>", bare(message_id))));
        let expand = format!("singleValueExtendedProperties($filter=id eq {})", literal(MARKERS));
        let path = format!(
            "/v1.0/me/messages?$filter={}&$select=id,categories&$expand={}&$top=50",
            encode(&filter),
            encode(&expand)
        );
        Ok(self
            .list(&path)
            .await?
            .iter()
            .filter_map(|v| {
                Some(Item { id: v["id"].as_str()?.to_string(), categories: strings(&v["categories"]), markers: markers_of(v) })
            })
            .collect())
    }

    /// Apply `change` to every copy of the message.
    pub async fn apply(&self, message_id: &str, change: Change<'_>) -> Result<()> {
        if let Change::Category { name, color, on: true } = change {
            self.ensure_category(name, color).await?;
        }
        let items = self.find(message_id).await?;
        if items.is_empty() {
            return Err(GraphRejected(format!("the message <{}> wasn't found in the mailbox", bare(message_id))).into());
        }
        for item in items {
            let body = match &change {
                Change::Category { name, on, .. } => {
                    let mut cats: Vec<String> = item.categories.iter().filter(|c| !c.eq_ignore_ascii_case(name)).cloned().collect();
                    if *on {
                        cats.push(name.to_string());
                    }
                    if cats.len() == item.categories.len() && cats.iter().zip(&item.categories).all(|(a, b)| a == b) {
                        continue;
                    }
                    json!({ "categories": cats })
                }
                Change::Marker { keyword, on } => {
                    let k = crate::tags::normalize(keyword);
                    let mut markers: Vec<String> = item.markers.iter().filter(|m| **m != k).cloned().collect();
                    if *on {
                        markers.push(k);
                    }
                    json!({ "singleValueExtendedProperties": [
                        { "id": MARKERS, "value": markers.join(" ") },
                        { "id": MARKED, "value": if markers.is_empty() { "0" } else { "1" } },
                    ]})
                }
            };
            let path = format!("/v1.0/me/messages/{}", encode(&item.id));
            match self.call("PATCH", &path, Some(&body)).await {
                Ok(_) => {}
                // A copy that went away meanwhile (moved, deleted): the others still count.
                Err(e) if e.is::<GraphRejected>() => tracing::info!("a copy of <{}>: {e:#}", bare(message_id)),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Everything the account's tags and markers are, as Graph has them now.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        let mut snap = Snapshot { categories: self.categories().await?, ..Default::default() };
        for (name, _) in &snap.categories {
            let filter = format!("categories/any(c:c eq {})", literal(name));
            let path = format!("/v1.0/me/messages?$filter={}&$select=internetMessageId&$top=500", encode(&filter));
            for v in self.list(&path).await? {
                if let Some(mid) = v["internetMessageId"].as_str() {
                    snap.tagged.entry(bare(mid)).or_default().push(name.clone());
                }
            }
        }
        let filter = format!("singleValueExtendedProperties/any(ep:ep/id eq {} and ep/value eq '1')", literal(MARKED));
        let expand = format!("singleValueExtendedProperties($filter=id eq {})", literal(MARKERS));
        let path = format!(
            "/v1.0/me/messages?$filter={}&$select=internetMessageId&$expand={}&$top=500",
            encode(&filter),
            encode(&expand)
        );
        for v in self.list(&path).await? {
            if let Some(mid) = v["internetMessageId"].as_str() {
                let m = markers_of(&v);
                if !m.is_empty() {
                    snap.markers.entry(bare(mid)).or_default().extend(m);
                }
            }
        }
        Ok(snap)
    }

    /// Check that Graph answers for this account (after the user allowed it).
    pub async fn check(&self) -> Result<()> {
        self.call("GET", "/v1.0/me/mailFolders/inbox?$select=id", None).await.map(|_| ()).map_err(|e| anyhow!("{e:#}"))
    }
}

/// The keywords a message should have, from Graph: its categories as tags (`tag_for` maps a
/// category name to its tag's keyword) and its markers.
pub fn keywords_from(snap: &Snapshot, message_id: &str, tag_for: &HashMap<String, String>) -> Vec<String> {
    let mut out: Vec<String> = snap
        .tagged
        .get(message_id)
        .into_iter()
        .flatten()
        .filter_map(|name| tag_for.get(&name.to_lowercase()).cloned())
        .collect();
    out.extend(snap.markers.get(message_id).into_iter().flatten().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thunderbird_colours_find_their_outlook_ones() {
        // Important, Work, Personal, To Do, Later
        assert_eq!(preset_for("#FF0000"), "preset0");
        assert_eq!(preset_for("#FF9900"), "preset1");
        assert_eq!(preset_for("#009900"), "preset19");
        assert_eq!(preset_for("#3333FF"), "preset7");
        assert_eq!(preset_for("#993399"), "preset23");
        assert_eq!(preset_for("#808080"), "preset13");
        assert_eq!(color_for("preset1"), "#FF8C00");
        assert_eq!(color_for("none"), "#949FA7");
    }

    #[test]
    fn queries_are_quoted_and_encoded() {
        assert_eq!(literal("it's"), "'it''s'");
        assert_eq!(encode("categories/any(c:c eq 'To Do')"), "categories%2Fany%28c%3Ac%20eq%20%27To%20Do%27%29");
        assert_eq!(bare("<abc@example.com>"), "abc@example.com");
    }

    #[test]
    fn keywords_from_categories_and_markers() {
        let mut snap = Snapshot::default();
        snap.tagged.insert("a@example.com".into(), vec!["Work".into(), "Unknown".into()]);
        snap.markers.insert("a@example.com".into(), vec!["$snooze-202611140900".into()]);
        let tags = HashMap::from([("work".to_string(), "$label2".to_string())]);
        assert_eq!(keywords_from(&snap, "a@example.com", &tags), ["$label2", "$snooze-202611140900"]);
        assert!(keywords_from(&snap, "b@example.com", &tags).is_empty());
    }
}
