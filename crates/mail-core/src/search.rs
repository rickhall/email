//! Quick-search query parsing: user syntax -> SQLite FTS5 MATCH expression.
//!
//! Syntax: words are ANDed and prefix-matched across sender, recipients and subject;
//! `"quoted phrases"`; `from:`, `to:`, `subject:` restrict a term to one field; `-term`
//! excludes. Anything FTS5 would treat as an operator is quoted, so arbitrary input is safe.

#[derive(Debug, PartialEq)]
struct Term {
    field: Option<&'static str>,
    text: String,
    negated: bool,
}

fn field_column(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "from" => "sender",
        "to" | "cc" => "recipients",
        "subject" | "subj" => "subject",
        _ => return None,
    })
}

fn parse(input: &str) -> Vec<Term> {
    let mut terms = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let negated = chars.peek() == Some(&'-');
        if negated {
            chars.next();
        }
        // Read a raw token up to whitespace, honouring quotes anywhere in it (from:"Ada L").
        let mut raw = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c == '"' {
                quoted = !quoted;
                raw.push(c);
                chars.next();
            } else if c.is_whitespace() && !quoted {
                break;
            } else {
                raw.push(c);
                chars.next();
            }
        }
        let (field, body) = match raw.split_once(':') {
            Some((f, rest)) => match field_column(f) {
                Some(col) => (Some(col), rest.to_string()),
                None => (None, raw.clone()),
            },
            None => (None, raw.clone()),
        };
        let text = body.replace('"', " ").split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().any(|c| c.is_alphanumeric()) {
            terms.push(Term { field, text, negated });
        }
    }
    terms
}

/// Split `tag:` terms (`tag:work`, `tag:"to do"`) out of a query: (rest of the query, tag values).
pub fn split_tags(query: &str) -> (String, Vec<String>) {
    let mut rest = String::new();
    let mut tags = Vec::new();
    let mut chars = query.char_indices().peekable();
    while let Some(&(i, _)) = chars.peek() {
        let tail = &query[i..];
        let at_word_start = i == 0 || query[..i].ends_with(char::is_whitespace);
        if at_word_start && tail.len() >= 4 && tail[..4].eq_ignore_ascii_case("tag:") {
            let after = &tail[4..];
            let (value, used) = if let Some(q) = after.strip_prefix('"') {
                match q.find('"') {
                    Some(end) => (q[..end].to_string(), 4 + 1 + end + 1),
                    None => (q.to_string(), tail.len()),
                }
            } else {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                (after[..end].to_string(), 4 + end)
            };
            if !value.trim().is_empty() {
                tags.push(value.trim().to_string());
            }
            for _ in 0..tail[..used].chars().count() {
                chars.next();
            }
            continue;
        }
        let (_, c) = chars.next().unwrap();
        rest.push(c);
    }
    (rest, tags)
}

/// Columns a plain (unprefixed) term may match.
pub const ALL_FIELDS: &[&str] = &["sender", "recipients", "subject"];

fn phrase(t: &Term, plain_fields: &[&str]) -> String {
    let p = format!("\"{}\"*", t.text.replace('"', "\"\""));
    match t.field {
        Some(col) => format!("{col} : {p}"),
        None if plain_fields.len() < ALL_FIELDS.len() => format!("{{{}}} : {p}", plain_fields.join(" ")),
        None => p,
    }
}

/// Build an FTS5 expression, or None if the query has no searchable (positive) terms.
/// `fields` limits plain terms to those columns (unknown names ignored; empty = all).
pub fn to_fts5(query: &str, fields: &[&str]) -> Option<String> {
    let mut plain: Vec<&str> = ALL_FIELDS.iter().copied().filter(|f| fields.contains(f)).collect();
    if plain.is_empty() {
        plain = ALL_FIELDS.to_vec();
    }
    let terms = parse(query);
    let positive: Vec<String> = terms.iter().filter(|t| !t.negated).map(|t| phrase(t, &plain)).collect();
    if positive.is_empty() {
        return None;
    }
    let mut expr = positive.join(" AND ");
    for t in terms.iter().filter(|t| t.negated) {
        expr = format!("({expr}) NOT {}", phrase(t, &plain));
    }
    Some(expr)
}

#[cfg(test)]
mod tests {
    fn to_fts5(q: &str) -> Option<String> {
        super::to_fts5(q, &[])
    }

    #[test]
    fn builds_expressions() {
        assert_eq!(to_fts5("invoice").unwrap(), r#""invoice"*"#);
        assert_eq!(to_fts5("  ada  bob ").unwrap(), r#""ada"* AND "bob"*"#);
        assert_eq!(to_fts5(r#""weekly report""#).unwrap(), r#""weekly report"*"#);
        assert_eq!(to_fts5(r#"from:"Ada L" subject:q3"#).unwrap(), r#"sender : "Ada L"* AND subject : "q3"*"#);
        assert_eq!(to_fts5("to:ada@example.com").unwrap(), r#"recipients : "ada@example.com"*"#);
        assert_eq!(to_fts5("report -draft").unwrap(), r#"("report"*) NOT "draft"*"#);
        // Unknown "field" is just text; operator words and stray punctuation are harmless.
        assert_eq!(to_fts5("re:meeting").unwrap(), r#""re:meeting"*"#);
        assert_eq!(to_fts5("AND OR NOT").unwrap(), r#""AND"* AND "OR"* AND "NOT"*"#);
        assert_eq!(to_fts5("-draft"), None);
        assert_eq!(to_fts5(" @ \" "), None);
    }

    #[test]
    fn tag_terms() {
        assert_eq!(super::split_tags("invoice tag:work"), ("invoice ".to_string(), vec!["work".to_string()]));
        assert_eq!(super::split_tags("tag:\"to do\" from:bob"), (" from:bob".to_string(), vec!["to do".to_string()]));
        assert_eq!(super::split_tags("hashtag:x"), ("hashtag:x".to_string(), vec![]));
    }

    #[test]
    fn limits_plain_terms_to_fields() {
        let f = |q, fields: &[&str]| super::to_fts5(q, fields).unwrap();
        assert_eq!(f("ada", &["sender"]), r#"{sender} : "ada"*"#);
        assert_eq!(f("ada", &["subject", "sender"]), r#"{sender subject} : "ada"*"#);
        assert_eq!(f("ada", &["sender", "recipients", "subject"]), r#""ada"*"#);
        assert_eq!(f("ada", &["bogus"]), r#""ada"*"#);
        // Explicit prefixes win over the field limit.
        assert_eq!(f("to:bob ada", &["subject"]), r#"recipients : "bob"* AND {subject} : "ada"*"#);
    }
}
