//! IMAP connection handling: TLS, authentication, folder listing and fetch-response parsing.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_imap::types::{Fetch, Flag, NameAttribute};
use futures::TryStreamExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, pki_types::ServerName};

use crate::mime;
use crate::model::{FolderRole, flags};
use crate::store::{HeaderRecord, ListedFolder};

pub type Session = async_imap::Session<Tracked<TlsStream<TcpStream>>>;

/// When data last moved on a connection, in either direction. Timeouts are based on this
/// (no progress for N seconds) rather than on total duration, so a large download on a slow
/// link isn't cut off while it's still arriving.
#[derive(Debug, Clone, Default)]
pub struct Activity(Arc<AtomicU64>);

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

impl Activity {
    pub fn touch(&self) {
        self.0.store(now_ms(), Ordering::Relaxed);
    }

    pub fn idle_for(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.0.load(Ordering::Relaxed)))
    }
}

/// A stream that records [`Activity`] on every read or write that moves bytes.
#[derive(Debug)]
pub struct Tracked<S> {
    inner: S,
    activity: Activity,
}

impl<S: AsyncRead + Unpin> AsyncRead for Tracked<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(r, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.activity.touch();
        }
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tracked<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, data);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            self.activity.touch();
        }
        r
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Run `fut`, failing if the connection goes `limit` without moving any data. The outer
/// `Result` is the stall; the inner value is whatever `fut` produced.
pub async fn stall_timeout<T>(
    activity: &Activity,
    limit: Duration,
    what: &str,
    fut: impl std::future::Future<Output = T>,
) -> Result<T> {
    activity.touch();
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            r = &mut fut => return Ok(r),
            _ = tick.tick() => {
                if activity.idle_for() > limit {
                    return Err(anyhow!("no data from the server for {}s while {what}; connection stalled", limit.as_secs()));
                }
            }
        }
    }
}

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
    /// RFC 8474: stable server-assigned EMAILID per message, shared by its copies.
    pub objectid: bool,
    /// RFC 6851 MOVE; without it a move is COPY + \Deleted + expunge.
    pub move_ext: bool,
    /// RFC 4315: UID EXPUNGE removes only the given messages.
    pub uidplus: bool,
}

pub struct Connection {
    pub session: Session,
    pub caps: Caps,
    /// Currently selected mailbox (raw name), to avoid redundant SELECTs.
    pub selected: Option<String>,
    pub activity: Activity,
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

    let activity = Activity::default();
    activity.touch();
    let mut client = async_imap::Client::new(Tracked { inner: tls, activity: activity.clone() });
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
        objectid: c.has_str("OBJECTID"),
        move_ext: c.has_str("MOVE"),
        uidplus: c.has_str("UIDPLUS"),
    };
    Ok(Connection { session, caps, selected: None, activity })
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
        let mut recs: Vec<HeaderRecord> = fetches.iter().filter_map(parse_header_fetch).collect();
        self.add_email_ids(&mut recs).await?;
        Ok(recs)
    }

    pub async fn fetch_headers_uid(&mut self, uid_range: &str) -> Result<Vec<HeaderRecord>> {
        let q = self.header_query();
        let fetches: Vec<Fetch> = self.session.uid_fetch(uid_range, &q).await?.try_collect().await?;
        let mut recs: Vec<HeaderRecord> = fetches.iter().filter_map(parse_header_fetch).collect();
        self.add_email_ids(&mut recs).await?;
        Ok(recs)
    }

    /// On OBJECTID servers, fill in each record's EMAILID (a second, small FETCH: async-imap
    /// parses EMAILID but doesn't expose it on `Fetch`, so the response is read directly).
    async fn add_email_ids(&mut self, recs: &mut [HeaderRecord]) -> Result<()> {
        if !self.caps.objectid || recs.is_empty() {
            return Ok(());
        }
        let uids: Vec<u32> = recs.iter().map(|r| r.uid).collect();
        let ids = self.fetch_email_ids(&uids).await?;
        for r in recs {
            r.email_id = ids.get(&r.uid).cloned();
        }
        Ok(())
    }

    pub async fn fetch_email_ids(&mut self, uids: &[u32]) -> Result<HashMap<u32, String>> {
        use async_imap::imap_proto::{AttributeValue, Response, Status};
        let tag = self.session.run_command(format!("UID FETCH {} (UID EMAILID)", uid_set(uids))).await?;
        let mut out = HashMap::new();
        loop {
            let resp = self.session.read_response().await?.ok_or_else(|| anyhow!("connection closed during FETCH"))?;
            match resp.parsed() {
                Response::Fetch(_, attrs) => {
                    let mut uid = None;
                    let mut email_id = None;
                    for a in attrs {
                        match a {
                            AttributeValue::Uid(u) => uid = Some(*u),
                            AttributeValue::EmailId(e) => email_id = Some(e.to_string()),
                            _ => {}
                        }
                    }
                    if let (Some(u), Some(e)) = (uid, email_id) {
                        out.insert(u, e);
                    }
                }
                Response::Done { tag: t, status, outcome } if *t == tag => {
                    if *status != Status::Ok {
                        return Err(anyhow!("FETCH EMAILID failed: {}", outcome.information.as_deref().unwrap_or("")));
                    }
                    return Ok(out);
                }
                _ => {}
            }
        }
    }

    /// (sequence number, UID) for a sequence range: a cheap way to compare the server's view
    /// of a folder with ours. Uses `UID SEARCH lo:hi` (a bare range in SEARCH means sequence
    /// numbers; the answer is UIDs): ~7 bytes per message, a quarter of `FETCH (UID)`. The range
    /// is contiguous and UIDs ascend with sequence numbers, so the n-th smallest UID is at
    /// sequence number lo + n.
    pub async fn fetch_seq_uids(&mut self, lo: u32, hi: u32) -> Result<Vec<(u32, u32)>> {
        let mut uids: Vec<u32> = self.session.uid_search(format!("{lo}:{hi}")).await?.into_iter().collect();
        uids.sort_unstable();
        Ok(uids.into_iter().enumerate().map(|(i, u)| (lo + i as u32, u)).collect())
    }

    /// Flags (and labels) changed since `modseq` (CONDSTORE), or all flags when `modseq` is None.
    pub async fn fetch_flags(&mut self, uid_range: &str, modseq: Option<u64>) -> Result<Vec<crate::store::FlagChange>> {
        let gm = if self.caps.gmail { " X-GM-LABELS" } else { "" };
        let q = match modseq {
            Some(m) => format!("(UID FLAGS{gm}) (CHANGEDSINCE {m})"),
            None => format!("(UID FLAGS{gm})"),
        };
        let fetches: Vec<Fetch> = self.session.uid_fetch(uid_range, &q).await?.try_collect().await?;
        Ok(fetches
            .iter()
            .filter_map(|f| {
                Some(crate::store::FlagChange { uid: f.uid?, flags: flag_bits(f), keywords: keywords(f), labels: gmail_labels(f) })
            })
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

    /// Download one attachment that the body-only download left on the server (in the selected
    /// folder), decoded. `section`/`encoding` come from the stored `OmittedPart`; when missing
    /// (stored before they were recorded) they're recovered from BODYSTRUCTURE by `ordinal`.
    pub async fn fetch_omitted_part(
        &mut self,
        uid: u32,
        ordinal: usize,
        section: Option<&str>,
        encoding: Option<&str>,
    ) -> Result<Vec<u8>> {
        let (section, encoding) = match section {
            Some(s) => (s.to_string(), encoding.map(str::to_string)),
            None => {
                let omitted = self
                    .fetch_plans(&[uid])
                    .await?
                    .into_iter()
                    .find(|(u, _)| *u == uid)
                    .and_then(|(_, p)| p)
                    .map(|p| p.omitted)
                    .unwrap_or_default();
                let part = omitted
                    .get(ordinal)
                    .ok_or_else(|| anyhow!("The attachment wasn't found in the message on the server."))?;
                (part.section.clone().unwrap_or_else(|| "1".into()), part.encoding.clone())
            }
        };
        let path: Vec<u32> = section.split('.').filter_map(|n| n.parse().ok()).collect();
        let fetches = self.fetch_items(&[uid], &format!("(UID BODY.PEEK[{section}])")).await?;
        let data = fetches
            .iter()
            .find(|f| f.uid == Some(uid))
            .and_then(|f| f.section(&imap_proto::SectionPath::Part(path.clone(), None)))
            .ok_or_else(|| anyhow!("The server returned no data for this attachment."))?;
        Ok(mime::decode_transfer(data, encoding.as_deref()))
    }

    /// Send one queued action command (see `actions::ServerOp`).
    pub async fn run_op(&mut self, op: &crate::actions::ServerOp) -> Result<()> {
        use crate::actions::{ServerOp, quote};
        async fn store(c: &mut Connection, uid: u32, query: String) -> Result<()> {
            let r: Vec<Fetch> = c.session.uid_store(uid.to_string(), query).await?.try_collect().await?;
            drop(r);
            Ok(())
        }
        match op {
            ServerOp::Labels { folder, uid, add, remove } => {
                self.ensure_selected(folder).await?;
                let list = |l: &[String]| l.iter().map(|x| quote(x)).collect::<Vec<_>>().join(" ");
                if !add.is_empty() {
                    store(self, *uid, format!("+X-GM-LABELS.SILENT ({})", list(add))).await?;
                }
                if !remove.is_empty() {
                    store(self, *uid, format!("-X-GM-LABELS.SILENT ({})", list(remove))).await?;
                }
            }
            ServerOp::Move { folder, uid, target } => {
                self.ensure_selected(folder).await?;
                if self.caps.move_ext {
                    self.session.uid_mv(uid.to_string(), target).await?;
                } else {
                    self.session.uid_copy(uid.to_string(), target).await?;
                    store(self, *uid, "+FLAGS.SILENT (\\Deleted)".into()).await?;
                    // Without UIDPLUS a plain EXPUNGE could remove other clients' \Deleted
                    // messages here; leave ours flagged for the server's own cleanup.
                    if self.caps.uidplus {
                        let r: Vec<_> = self.session.uid_expunge(uid.to_string()).await?.try_collect().await?;
                        drop(r);
                    }
                }
            }
            ServerOp::Flags { folder, uid, flag, on } => {
                self.ensure_selected(folder).await?;
                store(self, *uid, format!("{}FLAGS.SILENT ({flag})", if *on { "+" } else { "-" })).await?;
            }
            ServerOp::Expunge { folder, uid } => {
                self.ensure_selected(folder).await?;
                store(self, *uid, "+FLAGS.SILENT (\\Deleted)".into()).await?;
                if self.caps.uidplus {
                    let r: Vec<_> = self.session.uid_expunge(uid.to_string()).await?.try_collect().await?;
                    drop(r);
                } else {
                    // Only reached from Trash/Spam, where removing \Deleted messages is the point.
                    let r: Vec<_> = self.session.expunge().await?.try_collect().await?;
                    drop(r);
                }
            }
        }
        Ok(())
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

/// IMAP keywords on a message (Thunderbird tags such as `$label1`, junk markers, ...), lowercase.
fn keywords(f: &Fetch) -> Vec<String> {
    f.flags()
        .filter_map(|flag| match flag {
            Flag::Custom(k) if !k.starts_with('\\') => Some(crate::tags::normalize(&k)),
            _ => None,
        })
        .collect()
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
        email_id: None,
        gm_labels: gmail_labels(f),
        keywords: keywords(f),
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
