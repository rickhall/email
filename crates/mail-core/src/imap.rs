//! IMAP connection handling: TLS, authentication, folder listing and fetch-response parsing.

use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow};
use async_imap::types::{Fetch, Flag, NameAttribute};
use futures::TryStreamExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, pki_types::ServerName};

use crate::mime;
use crate::model::{FolderRole, flags};
use crate::store::{HeaderRecord, ListedFolder};

pub type Session = async_imap::Session<TlsStream<TcpStream>>;

/// The server rejected our credentials (as opposed to a network or protocol failure).
#[derive(Debug)]
pub struct AuthFailed(pub String);

impl std::fmt::Display for AuthFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "The server rejected the password: {}", self.0)
    }
}

impl std::error::Error for AuthFailed {}

pub fn is_auth_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<AuthFailed>())
}

/// The server refused the sign-in for a temporary reason (too many simultaneous connections,
/// bandwidth limits), not because of the credentials. Retry later; don't ask for a password.
#[derive(Debug)]
pub struct ConnectionLimit(pub String);

impl std::fmt::Display for ConnectionLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Too many simultaneous connections to this account. Other mail apps or devices are using \
             the server's connection limit (Gmail allows 15 per account); retrying. Server said: {}",
            self.0
        )
    }
}

impl std::error::Error for ConnectionLimit {}

pub fn is_limit_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<ConnectionLimit>())
}

/// Classify a refused sign-in. Temporary refusals are recognized by RFC 5530 response codes and
/// Gmail's wording; everything else is treated as a credentials problem.
pub fn classify_sign_in_refusal(message: &str) -> anyhow::Error {
    let m = message.to_ascii_lowercase();
    let temporary = [
        "too many simultaneous",
        "too many connections",
        "maximum number of connections",
        "connection limit",
        "[limit]",
        "[unavailable]",
        "[inuse]",
        "[overquota]",
        "bandwidth",
        "try again later",
    ];
    if temporary.iter().any(|t| m.contains(t)) {
        ConnectionLimit(message.trim().to_string()).into()
    } else {
        AuthFailed(message.trim().to_string()).into()
    }
}

fn auth_error(e: async_imap::error::Error) -> anyhow::Error {
    match e {
        async_imap::error::Error::No(m) | async_imap::error::Error::Bad(m) => classify_sign_in_refusal(&m),
        other => anyhow!("sign-in failed: {other}"),
    }
}

/// Header fields fetched during header sync. Content-Type lets us guess attachments early.
pub const HEADER_FIELDS: &str =
    "FROM TO CC SUBJECT DATE MESSAGE-ID REFERENCES IN-REPLY-TO CONTENT-TYPE";

#[derive(Clone)]
pub enum Credentials {
    Password(String),
    /// OAuth2 access token, used with SASL XOAUTH2 (Gmail, Outlook).
    OAuth2 { access_token: String },
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Credentials::Password(_) => "Password(..)",
            Credentials::OAuth2 { .. } => "OAuth2(..)",
        })
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub credentials: Credentials,
}

#[derive(Debug, Clone, Default)]
pub struct Caps {
    pub gmail: bool,
    pub condstore: bool,
    pub qresync: bool,
    pub idle: bool,
    pub special_use: bool,
}

pub struct Connection {
    pub session: Session,
    pub caps: Caps,
    /// Currently selected mailbox (raw name), to avoid redundant SELECTs.
    pub selected: Option<String>,
}

fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
            Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
        })
        .clone()
}

struct XOAuth2 {
    user: String,
    token: String,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> String {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

pub async fn connect(cfg: &ServerConfig) -> Result<Connection> {
    let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port))
        .await
        .with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;
    tcp.set_nodelay(true)?;
    let server_name = ServerName::try_from(cfg.host.clone())?;
    let tls = TlsConnector::from(tls_config()).connect(server_name, tcp).await?;

    let mut client = async_imap::Client::new(tls);
    client.read_response().await?.ok_or_else(|| anyhow!("server closed before greeting"))?;

    let mut session = match &cfg.credentials {
        Credentials::Password(pw) => client.login(&cfg.user, pw).await.map_err(|(e, _)| auth_error(e))?,
        Credentials::OAuth2 { access_token } => client
            .authenticate("XOAUTH2", XOAuth2 { user: cfg.user.clone(), token: access_token.clone() })
            .await
            .map_err(|(e, _)| auth_error(e))?,
    };

    let c = session.capabilities().await?;
    let caps = Caps {
        gmail: c.has_str("X-GM-EXT-1"),
        condstore: c.has_str("CONDSTORE"),
        qresync: c.has_str("QRESYNC"),
        idle: c.has_str("IDLE"),
        special_use: c.has_str("SPECIAL-USE"),
    };
    Ok(Connection { session, caps, selected: None })
}

impl Connection {
    /// SELECT (or EXAMINE-like read-write select) a mailbox, with CONDSTORE when available.
    pub async fn select(&mut self, raw_name: &str) -> Result<async_imap::types::Mailbox> {
        let mb = if self.caps.condstore {
            self.session.select_condstore(raw_name).await
        } else {
            self.session.select(raw_name).await
        }
        .with_context(|| format!("selecting {raw_name}"))?;
        self.selected = Some(raw_name.to_string());
        Ok(mb)
    }

    pub async fn ensure_selected(&mut self, raw_name: &str) -> Result<()> {
        if self.selected.as_deref() != Some(raw_name) {
            self.select(raw_name).await?;
        }
        Ok(())
    }

    pub async fn list_folders(&mut self) -> Result<Vec<ListedFolder>> {
        let names: Vec<_> = self.session.list(Some(""), Some("*")).await?.try_collect().await?;
        let mut out = Vec::with_capacity(names.len());
        for n in &names {
            let raw = n.name().to_string();
            let delim = n.delimiter().unwrap_or("/");
            let decoded = decode_mutf7(&raw);
            let path: Vec<String> = if delim.is_empty() {
                vec![decoded]
            } else {
                decoded.split(delim).map(str::to_string).collect()
            };
            let mut role = None;
            let mut selectable = true;
            for attr in n.attributes() {
                match attr {
                    NameAttribute::NoSelect => selectable = false,
                    NameAttribute::All => role = Some(FolderRole::All),
                    NameAttribute::Archive => role = Some(FolderRole::Archive),
                    NameAttribute::Drafts => role = Some(FolderRole::Drafts),
                    NameAttribute::Flagged => role = Some(FolderRole::Flagged),
                    NameAttribute::Junk => role = Some(FolderRole::Junk),
                    NameAttribute::Sent => role = Some(FolderRole::Sent),
                    NameAttribute::Trash => role = Some(FolderRole::Trash),
                    NameAttribute::Extension(ext) => {
                        let e = ext.to_ascii_lowercase();
                        if e == "\\important" {
                            role = Some(FolderRole::Important);
                        } else if e == "\\noselect" || e == "\\nonexistent" {
                            selectable = false;
                        }
                    }
                    _ => {}
                }
            }
            if raw.eq_ignore_ascii_case("INBOX") {
                role = Some(FolderRole::Inbox);
            }
            // Gmail marks its system folders with SPECIAL-USE; anything else is a user label,
            // even one called "Archive" or "Trash".
            if role.is_none() && !self.caps.gmail && path.len() == 1 {
                role = guess_role(&path);
            }
            out.push(ListedFolder { raw_name: raw, path, role, selectable });
        }
        // Gmail exposes "[Gmail]" as a non-selectable container (some accounts keep user labels
        // under it too); flatten it away so the sidebar shows "Sent" or "Work", not "[Gmail] / …".
        if self.caps.gmail {
            out.retain(|f| !(f.path.len() == 1 && f.path[0].starts_with('[') && !f.selectable));
        }
        for f in &mut out {
            if (self.caps.gmail || f.role.is_some()) && f.path.len() > 1 && f.path[0].starts_with('[') {
                f.path.remove(0);
            }
        }
        Ok(out)
    }

    /// STATUS for counts without selecting.
    pub async fn status(&mut self, raw_name: &str) -> Result<(u32, u32)> {
        let mb = self.session.status(raw_name, "(MESSAGES UNSEEN)").await?;
        Ok((mb.exists, mb.unseen.unwrap_or(0)))
    }

    /// The fetch query for header sync.
    pub fn header_query(&self) -> String {
        let gm = if self.caps.gmail { " X-GM-MSGID X-GM-LABELS" } else { "" };
        format!("(UID FLAGS RFC822.SIZE INTERNALDATE{gm} BODY.PEEK[HEADER.FIELDS ({HEADER_FIELDS})])")
    }

    /// Fetch headers for a sequence-number range (newest-first chunking uses sequence numbers
    /// because they are dense, so each chunk is exactly N messages regardless of UID gaps).
    pub async fn fetch_headers_seq(&mut self, range: &str) -> Result<Vec<HeaderRecord>> {
        let q = self.header_query();
        let fetches: Vec<Fetch> = self.session.fetch(range, &q).await?.try_collect().await?;
        Ok(fetches.iter().filter_map(parse_header_fetch).collect())
    }

    pub async fn fetch_headers_uid(&mut self, uid_range: &str) -> Result<Vec<HeaderRecord>> {
        let q = self.header_query();
        let fetches: Vec<Fetch> = self.session.uid_fetch(uid_range, &q).await?.try_collect().await?;
        Ok(fetches.iter().filter_map(parse_header_fetch).collect())
    }

    /// Flags (and labels) changed since `modseq` (CONDSTORE), or all flags when `modseq` is None.
    pub async fn fetch_flags(
        &mut self,
        uid_range: &str,
        modseq: Option<u64>,
    ) -> Result<Vec<(u32, i64, Option<Vec<String>>)>> {
        let gm = if self.caps.gmail { " X-GM-LABELS" } else { "" };
        let q = match modseq {
            Some(m) => format!("(UID FLAGS{gm}) (CHANGEDSINCE {m})"),
            None => format!("(UID FLAGS{gm})"),
        };
        let fetches: Vec<Fetch> = self.session.uid_fetch(uid_range, &q).await?.try_collect().await?;
        Ok(fetches
            .iter()
            .filter_map(|f| Some((f.uid?, flag_bits(f), gmail_labels(f))))
            .collect())
    }

    pub async fn uid_search_all(&mut self) -> Result<Vec<u32>> {
        Ok(self.session.uid_search("ALL").await?.into_iter().collect())
    }

    /// Sequence number of `uid`, if it still exists in the selected mailbox.
    pub async fn seq_of_uid(&mut self, uid: u32) -> Result<Option<u32>> {
        let fetches: Vec<Fetch> =
            self.session.uid_fetch(uid.to_string(), "(UID)").await?.try_collect().await?;
        Ok(fetches.iter().find(|f| f.uid == Some(uid)).map(|f| f.message))
    }

    /// Which parts each message needs for a body-only download (`None`: no attachments).
    pub async fn fetch_plans(&mut self, uids: &[u32]) -> Result<Vec<(u32, Option<crate::partial::Plan>)>> {
        let fetches: Vec<Fetch> =
            self.session.uid_fetch(uid_set(uids), "(UID BODYSTRUCTURE)").await?.try_collect().await?;
        Ok(fetches
            .iter()
            .filter_map(|f| Some((f.uid?, f.bodystructure().and_then(crate::partial::plan))))
            .collect())
    }

    /// Fetch the given section items for messages sharing one plan shape.
    pub async fn fetch_items(&mut self, uids: &[u32], items: &str) -> Result<Vec<Fetch>> {
        Ok(self.session.uid_fetch(uid_set(uids), items).await?.try_collect().await?)
    }

    /// Download full raw messages. Returns (uid, raw bytes).
    pub async fn fetch_bodies(&mut self, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>> {
        if uids.is_empty() {
            return Ok(vec![]);
        }
        let set = uid_set(uids);
        let fetches: Vec<Fetch> =
            self.session.uid_fetch(&set, "(UID BODY.PEEK[])").await?.try_collect().await?;
        Ok(fetches
            .iter()
            .filter_map(|f| Some((f.uid?, f.body()?.to_vec())))
            .collect())
    }

    pub async fn logout(mut self) {
        let _ = self.session.logout().await;
    }
}

/// Exposes a FETCH response's body sections to `partial::rebuild`.
pub struct FetchSections<'a>(pub &'a Fetch);

impl crate::partial::Sections for FetchSections<'_> {
    fn header(&self) -> Option<&[u8]> {
        self.0.section(&imap_proto::SectionPath::Full(imap_proto::MessageSection::Header))
    }
    fn mime(&self, path: &[u32]) -> Option<&[u8]> {
        self.0.section(&imap_proto::SectionPath::Part(path.to_vec(), Some(imap_proto::MessageSection::Mime)))
    }
    fn content(&self, path: &[u32]) -> Option<&[u8]> {
        self.0.section(&imap_proto::SectionPath::Part(path.to_vec(), None))
    }
}

fn guess_role(path: &[String]) -> Option<FolderRole> {
    let last = path.last()?.to_ascii_lowercase();
    Some(match last.as_str() {
        "sent" | "sent items" | "sent messages" | "sent mail" => FolderRole::Sent,
        "drafts" | "draft" => FolderRole::Drafts,
        "trash" | "deleted items" | "deleted messages" | "bin" => FolderRole::Trash,
        "junk" | "spam" | "junk email" | "junk e-mail" | "bulk mail" => FolderRole::Junk,
        "archive" | "archives" => FolderRole::Archive,
        _ => return None,
    })
}

fn flag_bits(f: &Fetch) -> i64 {
    let mut bits = 0;
    for flag in f.flags() {
        bits |= match flag {
            Flag::Seen => flags::SEEN,
            Flag::Flagged => flags::FLAGGED,
            Flag::Answered => flags::ANSWERED,
            Flag::Draft => flags::DRAFT,
            Flag::Deleted => flags::DELETED,
            _ => 0,
        };
    }
    bits
}

fn gmail_labels(f: &Fetch) -> Option<Vec<String>> {
    f.gmail_labels().map(|l| l.iter().map(|s| normalize_label(s)).collect())
}

/// Gmail sends system labels quoted (`"\\Inbox"`) and the parser leaves quoted strings
/// escaped, so undo the escaping to get `\Inbox`.
pub fn normalize_label(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.clone().next() {
                Some(n @ ('\\' | '"')) => {
                    out.push(n);
                    chars.next();
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_header_fetch(f: &Fetch) -> Option<HeaderRecord> {
    let uid = f.uid?;
    let header = f.header().unwrap_or_default();
    let parsed = mime::parse_header_block(header);
    let date = f
        .internal_date()
        .map(|d| d.timestamp())
        .or(parsed.date)
        .unwrap_or(0);
    Some(HeaderRecord {
        uid,
        gm_msgid: f.gmail_msg_id().copied(),
        gm_labels: gmail_labels(f),
        message_id: parsed.message_id,
        in_reply_to: parsed.in_reply_to,
        refs: parsed.refs,
        subject: parsed.subject,
        from_name: parsed.from_name,
        from_addr: parsed.from_addr,
        to_addrs: parsed.to_addrs,
        date,
        size: f.size.unwrap_or(0),
        flags: flag_bits(f),
        has_attachments: parsed.likely_attachments,
    })
}

/// Compress a list of UIDs into an IMAP sequence set, e.g. `1:4,7,9:10`.
pub fn uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = String::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}:{end}"));
        }
        i += 1;
    }
    out
}

/// Decode IMAP modified UTF-7 (RFC 3501 §5.1.3) mailbox names.
pub fn decode_mutf7(s: &str) -> String {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD_NO_PAD;
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos + 1..];
        let Some(end) = rest.find('-') else {
            out.push('&');
            continue;
        };
        let chunk = &rest[..end];
        rest = &rest[end + 1..];
        if chunk.is_empty() {
            out.push('&');
            continue;
        }
        match engine.decode(chunk.replace(',', "/")) {
            Ok(bytes) => {
                let units: Vec<u16> =
                    bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                out.push_str(&String::from_utf16_lossy(&units));
            }
            Err(_) => {
                out.push('&');
                out.push_str(chunk);
                out.push('-');
            }
        }
    }
    out.push_str(rest);
    out
}

/// Server settings for any address: well-known domains first, then the domain's MX records,
/// which reveal Google Workspace and Microsoft 365 custom domains.
pub async fn discover_server(email: &str) -> Option<(&'static str, u16, crate::model::Provider)> {
    use crate::model::Provider;
    if let Some(known) = server_for(email) {
        return Some(known);
    }
    let domain = email.rsplit('@').next()?.trim().to_ascii_lowercase();
    let resolver = hickory_resolver::TokioResolver::builder_tokio().ok()?.build().ok()?;
    let lookup = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        resolver.mx_lookup(format!("{domain}.")),
    )
    .await
    .ok()?
    .ok()?;
    for record in lookup.answers() {
        let hickory_resolver::proto::rr::RData::MX(mx) = &record.data else { continue };
        let host = mx.exchange.to_ascii().to_ascii_lowercase();
        let host = host.trim_end_matches('.');
        if host.ends_with("google.com") || host.ends_with("googlemail.com") {
            return Some(("imap.gmail.com", 993, Provider::Gmail));
        }
        if host.ends_with("protection.outlook.com") || host.ends_with("outlook.com") {
            return Some(("outlook.office365.com", 993, Provider::Outlook));
        }
    }
    None
}

/// Well-known server settings by email domain.
pub fn server_for(email: &str) -> Option<(&'static str, u16, crate::model::Provider)> {
    use crate::model::Provider;
    let domain = email.rsplit('@').next()?.to_ascii_lowercase();
    Some(match domain.as_str() {
        "gmail.com" | "googlemail.com" => ("imap.gmail.com", 993, Provider::Gmail),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" => {
            ("outlook.office365.com", 993, Provider::Outlook)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutf7() {
        assert_eq!(decode_mutf7("INBOX"), "INBOX");
        assert_eq!(decode_mutf7("&ZeVnLIqe-"), "日本語");
        assert_eq!(decode_mutf7("Tom &- Jerry"), "Tom & Jerry");
        assert_eq!(decode_mutf7("Entw&APw-rfe"), "Entwürfe");
    }

    #[test]
    fn sign_in_refusals() {
        let limit = |m| is_limit_error(&classify_sign_in_refusal(m));
        let auth = |m| is_auth_error(&classify_sign_in_refusal(m));
        assert!(limit("[ALERT] Too many simultaneous connections. (Failure)"));
        assert!(limit("[UNAVAILABLE] Server busy, try again later"));
        assert!(limit("[OVERQUOTA] Account exceeded bandwidth limits"));
        assert!(auth("[AUTHENTICATIONFAILED] Invalid credentials (Failure)"));
        assert!(auth("[ALERT] Application-specific password required"));
        assert!(auth("LOGIN failed."));
        assert!(!limit("[AUTHENTICATIONFAILED] Invalid credentials (Failure)"));
    }

    #[test]
    fn gmail_label_escapes() {
        assert_eq!(normalize_label(r"\\Inbox"), r"\Inbox");
        assert_eq!(normalize_label(r"\Inbox"), r"\Inbox");
        assert_eq!(normalize_label(r#"Work/Q\"1\""#), r#"Work/Q"1""#);
        assert_eq!(normalize_label("Plain"), "Plain");
    }

    #[test]
    fn uid_sets() {
        assert_eq!(uid_set(&[1, 2, 3, 4, 7, 9, 10]), "1:4,7,9:10");
        assert_eq!(uid_set(&[5]), "5");
    }
}

#[cfg(test)]
mod discovery_tests {
    /// Hits the network; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn workspace_domain_via_mx() {
        let found = super::discover_server(&std::env::var("DISCOVER_EMAIL").unwrap_or("x@google.com".into())).await;
        assert_eq!(found.map(|f| f.0), Some("imap.gmail.com"));
    }
}
