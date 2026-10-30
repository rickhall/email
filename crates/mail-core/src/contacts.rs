//! Address suggestions for the composer: everyone in the user's mail (senders, and the people
//! the user wrote to), ranked by how much and how recently they corresponded. Built from the
//! stored headers, kept in memory, and topped up as mail is sent.

use std::collections::HashMap;

use crate::model::Addr;

#[derive(Debug, Clone)]
pub struct Contact {
    pub name: Option<String>,
    pub addr: String,
    /// Messages the user sent to them.
    pub sent: u32,
    /// Messages from them.
    pub received: u32,
    /// Newest exchange (unix seconds).
    pub last: i64,
}

impl Contact {
    /// Writing to someone counts most; recent exchanges more than old ones.
    fn score(&self, now: i64) -> f64 {
        let age_days = ((now - self.last).max(0) / 86_400) as f64;
        let recency = if age_days < 30.0 {
            6.0
        } else if age_days < 365.0 {
            3.0
        } else {
            0.0
        };
        self.sent as f64 * 4.0 + (self.received as f64).sqrt() + recency
    }
}

/// Automated senders nobody writes to.
fn is_robot(addr: &str) -> bool {
    let local = addr.split('@').next().unwrap_or("");
    ["noreply", "no-reply", "no_reply", "donotreply", "do-not-reply", "do_not_reply", "mailer-daemon", "postmaster", "bounce", "notifications", "notification"]
        .iter()
        .any(|r| local.contains(r))
}

/// Addresses in a stored recipient list ("Name <addr>, addr, Doe, Jane <addr>"). Names are
/// stored unquoted and may contain commas, so a piece without an address belongs to the next.
pub fn parse_list(list: &str) -> Vec<Addr> {
    // Split on commas outside <...>.
    let mut pieces: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut angle = false;
    for ch in list.chars() {
        match ch {
            '<' => angle = true,
            '>' => angle = false,
            ',' if !angle => {
                pieces.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    pieces.push(cur);
    let mut out = Vec::new();
    let mut name_part = String::new();
    for piece in pieces {
        if !piece.contains('@') {
            // part of a name with a comma in it ("Doe" of "Doe, John <…>")
            if !piece.trim().is_empty() {
                name_part.push_str(piece.trim());
                name_part.push_str(", ");
            }
            continue;
        }
        let whole = format!("{name_part}{}", piece.trim());
        name_part.clear();
        if let (Some(lt), Some(gt)) = (whole.find('<'), whole.rfind('>')) {
            let name = whole[..lt].trim().trim_matches('"').trim();
            out.push(Addr { name: (!name.is_empty()).then(|| name.to_string()), addr: whole[lt + 1..gt].trim().to_string() });
        } else {
            out.push(Addr { name: None, addr: whole.trim().to_string() });
        }
    }
    out
}

/// Everyone in the mail, from (from_name, from_addr, to_addrs, date) rows. `me`: the user's own
/// addresses (lowercase): their messages count as sent, and they're never suggested.
pub fn build<I>(rows: I, me: &[String]) -> Vec<Contact>
where
    I: IntoIterator<Item = (String, String, String, i64)>,
{
    let mut by: HashMap<String, Contact> = HashMap::new();
    let mut add = |name: Option<&str>, addr: &str, date: i64, sent: bool| {
        let key = addr.trim().to_ascii_lowercase();
        if key.is_empty() || !key.contains('@') || me.contains(&key) || is_robot(&key) {
            return;
        }
        let c = by.entry(key.clone()).or_insert_with(|| Contact { name: None, addr: key, sent: 0, received: 0, last: 0 });
        if sent {
            c.sent += 1;
        } else {
            c.received += 1;
        }
        // The name from the newest message that had one.
        if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty() && !n.contains('@')) {
            if date >= c.last || c.name.is_none() {
                c.name = Some(n.to_string());
            }
        }
        c.last = c.last.max(date);
    };
    for (from_name, from_addr, to_addrs, date) in rows {
        let from_me = me.contains(&from_addr.trim().to_ascii_lowercase());
        if from_me {
            for a in parse_list(&to_addrs) {
                add(a.name.as_deref(), &a.addr, date, true);
            }
        } else {
            add(Some(&from_name), &from_addr, date, false);
        }
    }
    by.into_values().collect()
}

/// Up to `limit` contacts matching what's typed: the start of the address, of its parts
/// (jane.doe → "doe"), or of any word of the name. Best first.
pub fn suggest(list: &[Contact], query: &str, limit: usize, now: i64) -> Vec<Addr> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    let matches = |c: &Contact| {
        if c.addr.starts_with(&q) {
            return Some(2.0);
        }
        let name = c.name.as_deref().unwrap_or("").to_lowercase();
        if name.starts_with(&q) {
            return Some(2.0);
        }
        let local = c.addr.split('@').next().unwrap_or("");
        let word = |s: &str| s.split([' ', '.', '_', '-', '\'']).any(|w| !w.is_empty() && w.starts_with(&q));
        (word(&name) || word(local) || (q.contains(' ') && name.contains(&q))).then_some(0.0)
    };
    let mut hits: Vec<(f64, &Contact)> = list.iter().filter_map(|c| matches(c).map(|bonus| (c.score(now) + bonus, c))).collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.addr.cmp(&b.1.addr)));
    hits.into_iter().take(limit).map(|(_, c)| Addr { name: c.name.clone(), addr: c.addr.clone() }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipient_lists() {
        let l = parse_list("Jane Doe <jane@example.net>, bob@example.org, \"Doe, John\" <john@example.com>, Kim <kim@example.org>");
        let got: Vec<(Option<&str>, &str)> = l.iter().map(|a| (a.name.as_deref(), a.addr.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (Some("Jane Doe"), "jane@example.net"),
                (None, "bob@example.org"),
                (Some("Doe, John"), "john@example.com"),
                (Some("Kim"), "kim@example.org"),
            ]
        );
        assert_eq!(parse_list("Doe, John <john@example.com>")[0].name.as_deref(), Some("Doe, John"));
    }

    #[test]
    fn ranks_people_written_to_first() {
        let me = vec!["me@example.com".to_string()];
        let day = 86_400;
        let now = 1_000 * day;
        let rows = vec![
            // a newsletter, often; a robot; someone the user writes to; an old acquaintance
            ("Daily Digest".into(), "digest@news.example".into(), String::new(), now - day),
            ("Daily Digest".into(), "digest@news.example".into(), String::new(), now - 2 * day),
            ("GitHub".into(), "notifications@github.example".into(), String::new(), now),
            ("Me".into(), "me@example.com".into(), "Jane Doe <jane.doe@example.net>, \"Dee, Ann\" <ann@example.org>".into(), now - 3 * day),
            ("Jane D.".into(), "Jane.Doe@example.net".into(), "me@example.com".into(), now - 900 * day),
            ("Dan Old".into(), "dan@example.org".into(), "me@example.com".into(), now - 800 * day),
        ];
        let list = build(rows, &me);
        assert!(list.iter().all(|c| c.addr != "me@example.com" && !c.addr.starts_with("notifications")));
        let names = |q: &str| suggest(&list, q, 5, now).into_iter().map(|a| a.addr).collect::<Vec<_>>();
        // Ann's name starts with "d" (bonus); Jane only matches on "doe"; the digest is just read
        assert_eq!(names("d"), vec!["ann@example.org", "jane.doe@example.net", "digest@news.example", "dan@example.org"]);
        assert_eq!(names("doe"), vec!["jane.doe@example.net"], "matches a part of the address");
        assert_eq!(names("ann"), vec!["ann@example.org"]);
        assert!(names("zzz").is_empty());
        // the newest name wins
        let jane = suggest(&list, "jane", 1, now);
        assert_eq!(jane[0].name.as_deref(), Some("Jane Doe"));
    }
}
