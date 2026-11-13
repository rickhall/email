//! Local SQLite store.
//!
//! Two databases: `mail.db` holds accounts, folders and message metadata (small, hot),
//! `bodies.db` holds raw RFC 822 messages (large, cold) so header queries stay fast.
//!
//! A message row is unique per account by Gmail's `X-GM-MSGID` when available, so a Gmail
//! message carrying several labels is stored (and its body downloaded) once. `entries` maps
//! messages into folders; `uid` is null when membership was derived from Gmail labels rather
//! than observed in that folder directly. For Gmail, labels are the source of truth for
//! membership in label-backed folders.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::*;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id           INTEGER PRIMARY KEY,
    email        TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    provider     TEXT NOT NULL,
    imap_host    TEXT NOT NULL,
    imap_port    INTEGER NOT NULL,
    smtp_host    TEXT,
    smtp_port    INTEGER,
    keep_attachments INTEGER NOT NULL DEFAULT 0, -- download whole messages (offline attachments)
    auth         TEXT NOT NULL DEFAULT 'password', -- how it signs in: password | microsoft (OAuth)
    keywords     INTEGER,               -- server keeps custom keywords (\* in PERMANENTFLAGS); NULL: not seen yet
    position     INTEGER,               -- the user's order (dragged in the sidebar); NULL: after the others, by id
    mark_letter  TEXT,                  -- its marker's letter, set by the user; NULL: automatic
    mark_color   TEXT                   -- its marker's colour (a name from the app's palette); NULL: automatic
);

CREATE TABLE IF NOT EXISTS folders (
    id             INTEGER PRIMARY KEY,
    account_id     INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    raw_name       TEXT NOT NULL,
    path           TEXT NOT NULL,          -- JSON array of decoded segments
    role           TEXT,
    selectable     INTEGER NOT NULL,
    total          INTEGER NOT NULL DEFAULT 0,
    unread         INTEGER NOT NULL DEFAULT 0,
    uidvalidity    INTEGER NOT NULL DEFAULT 0,
    highest_modseq INTEGER NOT NULL DEFAULT 0,
    top_uid        INTEGER NOT NULL DEFAULT 0, -- highest UID whose header is stored
    low_uid        INTEGER NOT NULL DEFAULT 0, -- lowest UID whose header is stored (0 = none)
    complete       INTEGER NOT NULL DEFAULT 0, -- header backfill reached the oldest message
    present        INTEGER NOT NULL DEFAULT 1, -- seen in last LIST
    UNIQUE(account_id, raw_name)
);

CREATE TABLE IF NOT EXISTS messages (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    gm_msgid        INTEGER,
    message_id      TEXT,
    in_reply_to     TEXT,
    refs            TEXT,
    subject         TEXT NOT NULL,
    from_name       TEXT NOT NULL,
    from_addr       TEXT NOT NULL,
    to_addrs        TEXT NOT NULL,
    date            INTEGER NOT NULL,
    size            INTEGER NOT NULL DEFAULT 0,
    flags           INTEGER NOT NULL DEFAULT 0,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    has_body        INTEGER NOT NULL DEFAULT 0,
    snippet         TEXT,
    omitted         TEXT,    -- JSON [OmittedPart] when the body was stored without attachments
    allow_remote    INTEGER NOT NULL DEFAULT 0, -- user chose "Load images" for this message
    thread_id       INTEGER, -- conversation; the id of one of its messages
    identity        TEXT,    -- non-Gmail: EMAILID, or Message-ID + size (copies are one message)
    keywords        TEXT NOT NULL DEFAULT '', -- IMAP keywords (tags), space-separated
    local_copy      INTEGER NOT NULL DEFAULT 0, -- our own sent mail, shown before the server has it
    pending_send    INTEGER, -- its outgoing row while queued
    draft_id        INTEGER, -- a local draft's row, while it's one
    -- whole body holding true attachments: 1 kept (Remove takes them); 2 not meant to be kept
    -- (a whole-message fallback in an account without "keep"), stripped at the next start;
    -- NULL not known yet
    attachments_stored INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS messages_gm ON messages(account_id, gm_msgid) WHERE gm_msgid IS NOT NULL;
CREATE INDEX IF NOT EXISTS messages_nobody ON messages(account_id, has_body, date DESC);
CREATE INDEX IF NOT EXISTS messages_thread ON messages(thread_id, date);
CREATE INDEX IF NOT EXISTS messages_msgid ON messages(account_id, message_id);
CREATE UNIQUE INDEX IF NOT EXISTS messages_identity ON messages(account_id, identity) WHERE identity IS NOT NULL;
-- Our own local copies (sent mail, drafts): few, looked over without a scan.
CREATE INDEX IF NOT EXISTS messages_local ON messages(account_id) WHERE local_copy = 1;
-- Messages stored without their attachments (counted for download progress without a scan).
CREATE INDEX IF NOT EXISTS messages_omitted ON messages(account_id) WHERE omitted IS NOT NULL;

-- Message-IDs each message refers to (In-Reply-To + References), for threading.
CREATE TABLE IF NOT EXISTS msgrefs (
    account_id INTEGER NOT NULL,
    ref_msgid  TEXT NOT NULL,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS msgrefs_ref ON msgrefs(account_id, ref_msgid);
CREATE INDEX IF NOT EXISTS msgrefs_msg ON msgrefs(message_id);

-- Server operations for user actions (archive, delete, move, flags), applied locally at once
-- and sent from here: survives restarts and offline periods. `snapshot` restores the local
-- state on undo or permanent failure; ops with `started = 0` can still be cancelled.
-- Messages being written: the composer's state, saved as it changes. Shown in Drafts as a
-- local copy (messages.draft_id) until sent or discarded.
CREATE TABLE IF NOT EXISTS drafts (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    state      TEXT NOT NULL,      -- the composer's own JSON
    message_id TEXT NOT NULL,      -- stable across saves
    updated    INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Outgoing mail: built messages waiting for their send time (the undo window), then SMTP.
CREATE TABLE IF NOT EXISTS outgoing (
    id            INTEGER PRIMARY KEY,
    account_id    INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    raw           BLOB NOT NULL,
    envelope_from TEXT NOT NULL,
    recipients    TEXT NOT NULL,     -- JSON array of addresses (To, Cc, Bcc)
    not_before    INTEGER NOT NULL,  -- ms
    started       INTEGER NOT NULL DEFAULT 0,
    attempts      INTEGER NOT NULL DEFAULT 0,
    failed        TEXT,              -- set when the server refused it for good
    replied       INTEGER,           -- message to mark replied once sent
    forwarded     INTEGER,           -- message to mark forwarded once sent
    draft         TEXT,              -- the composer's state, to reopen after undo or failure
    created       INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS pending_ops (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    ops        TEXT NOT NULL,
    snapshot   TEXT NOT NULL,
    folders    TEXT NOT NULL,
    not_before INTEGER NOT NULL,
    started    INTEGER NOT NULL DEFAULT 0,
    attempts   INTEGER NOT NULL DEFAULT 0
);

-- Tag names and colours (keywords are on the server; definitions are local, like Thunderbird's).
CREATE TABLE IF NOT EXISTS tags (
    keyword  TEXT PRIMARY KEY,
    name     TEXT NOT NULL,
    color    TEXT NOT NULL,
    position INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS entries (
    folder_id  INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    uid        INTEGER,
    date       INTEGER NOT NULL,
    thread_id  INTEGER, -- copy of messages.thread_id (kept exact by triggers)
    PRIMARY KEY (folder_id, message_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS entries_list ON entries(folder_id, date DESC);
CREATE UNIQUE INDEX IF NOT EXISTS entries_uid ON entries(folder_id, uid) WHERE uid IS NOT NULL;
CREATE INDEX IF NOT EXISTS entries_msg ON entries(message_id);
-- Copies placed locally (a move, a Gmail label) that are waiting for their UID.
CREATE INDEX IF NOT EXISTS entries_waiting ON entries(folder_id) WHERE uid IS NULL;
-- entries.thread_id mirrors messages.thread_id so a folder's conversations can be listed from
-- one covering index.
CREATE INDEX IF NOT EXISTS entries_thread ON entries(folder_id, thread_id, date);
CREATE TRIGGER IF NOT EXISTS entries_thread_ins AFTER INSERT ON entries BEGIN
    UPDATE entries SET thread_id=(SELECT thread_id FROM messages WHERE id=NEW.message_id)
    WHERE folder_id=NEW.folder_id AND message_id=NEW.message_id;
END;
CREATE TRIGGER IF NOT EXISTS messages_thread_upd AFTER UPDATE OF thread_id ON messages BEGIN
    UPDATE entries SET thread_id=NEW.thread_id WHERE message_id=NEW.id;
END;

-- Quick search over sender, recipients and subject. Contentless: it stores only the index, rows
-- are joined back to `messages` by rowid. Triggers keep it current.
-- addresses the user removed from the suggestions (lowercase)
CREATE TABLE IF NOT EXISTS forgotten_addresses (
    addr TEXT PRIMARY KEY
);

CREATE VIRTUAL TABLE IF NOT EXISTS msg_fts USING fts5(
    subject, sender, recipients,
    content='', contentless_delete=1,
    tokenize='unicode61 remove_diacritics 2', prefix='2 3');
CREATE TRIGGER IF NOT EXISTS msg_fts_ins AFTER INSERT ON messages BEGIN
    INSERT INTO msg_fts(rowid, subject, sender, recipients)
    VALUES (NEW.id, NEW.subject, NEW.from_name || ' ' || NEW.from_addr, NEW.to_addrs);
END;
CREATE TRIGGER IF NOT EXISTS msg_fts_del AFTER DELETE ON messages BEGIN
    DELETE FROM msg_fts WHERE rowid = OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS msg_fts_upd AFTER UPDATE OF subject, from_name, from_addr, to_addrs ON messages BEGIN
    DELETE FROM msg_fts WHERE rowid = OLD.id;
    INSERT INTO msg_fts(rowid, subject, sender, recipients)
    VALUES (NEW.id, NEW.subject, NEW.from_name || ' ' || NEW.from_addr, NEW.to_addrs);
END;
"#;

/// Schema versions (`PRAGMA user_version`). `SCHEMA` is always the current schema, whole:
/// a new database gets it directly. A database at `BASE_VERSION` or later is brought up to
/// date by the steps in `MIGRATIONS` it hasn't had (step i takes BASE_VERSION + i to the next).
/// One older than the base isn't upgraded: it's deleted, with its bodies, and its accounts sync
/// from scratch; one newer than this build is refused.
///
/// While the app has one user, the steps are folded back into `SCHEMA` now and then and the
/// base moved up, once every machine has run them (the base is 4 since 2026-11-03). With more
/// users this needs a stricter policy.
const BASE_VERSION: i64 = 4;
/// Schema changes since the base, oldest first. Add one per change (and change `SCHEMA` too).
const MIGRATIONS: &[fn(&Connection) -> Result<()>] = &[
    // 5: an account can keep its attachments for offline use
    |db| Ok(db.execute_batch("ALTER TABLE accounts ADD COLUMN keep_attachments INTEGER NOT NULL DEFAULT 0;")?),
    // 6: which stored bodies hold true attachments (filled in by `mark_stored_attachments`)
    |db| Ok(db.execute_batch("ALTER TABLE messages ADD COLUMN attachments_stored INTEGER;")?),
    // 7: attachments stored by the whole-message fallback in accounts that don't keep them go
    // (`strip_unwanted_attachments`, once)
    |db| {
        Ok(db.execute_batch(
            "UPDATE messages SET attachments_stored=2 WHERE attachments_stored=1
               AND account_id IN (SELECT id FROM accounts WHERE keep_attachments=0);",
        )?)
    },
    // 8: accounts can sign in with Microsoft (OAuth) instead of a password
    |db| Ok(db.execute_batch("ALTER TABLE accounts ADD COLUMN auth TEXT NOT NULL DEFAULT 'password';")?),
    // 9: whether the server keeps custom keywords (tags, snooze)
    |db| Ok(db.execute_batch("ALTER TABLE accounts ADD COLUMN keywords INTEGER;")?),
    // 10: the accounts' order and markers, set by the user
    |db| {
        Ok(db.execute_batch(
            "ALTER TABLE accounts ADD COLUMN position INTEGER;
             ALTER TABLE accounts ADD COLUMN mark_letter TEXT;
             ALTER TABLE accounts ADD COLUMN mark_color TEXT;",
        )?)
    },
    // 11: subjects with HTML character references in the header decoded (`clean_subject`);
    // addresses removed from the suggestions
    |db| {
        db.execute_batch("CREATE TABLE IF NOT EXISTS forgotten_addresses (addr TEXT PRIMARY KEY);")?;
        let rows: Vec<(i64, String)> = db
            .prepare("SELECT id, subject FROM messages WHERE subject LIKE '%&%;%'")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut update = db.prepare("UPDATE messages SET subject=?2 WHERE id=?1")?;
        for (id, subject) in rows {
            let clean = crate::mime::clean_subject(&subject);
            if clean != subject {
                update.execute(params![id, clean])?;
            }
        }
        Ok(())
    },
];
const SCHEMA_VERSION: i64 = BASE_VERSION + MIGRATIONS.len() as i64;

const BODY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS bodies (
    message_id INTEGER PRIMARY KEY,
    raw        BLOB NOT NULL
);
-- Bodies of messages that disappeared from every folder, kept briefly by identity: on servers
-- without Gmail labels a move is an expunge plus a new copy elsewhere, and if the expunge is
-- seen first the new copy picks the body up from here instead of downloading it again.
CREATE TABLE IF NOT EXISTS stash (
    account_id      INTEGER NOT NULL,
    identity        TEXT NOT NULL,
    raw             BLOB NOT NULL,
    snippet         TEXT,
    has_attachments INTEGER NOT NULL,
    omitted         TEXT,
    allow_remote    INTEGER NOT NULL,
    created         INTEGER NOT NULL,
    PRIMARY KEY (account_id, identity)
);
"#;

/// Attachments fetched on demand (omitted from the stored body), so opening one again is
/// instant and works offline. Keyed by the attachment's index in the message.
const ATTACHMENT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS attachment_cache (
    message_id INTEGER NOT NULL,
    idx        INTEGER NOT NULL,
    data       BLOB NOT NULL,
    PRIMARY KEY (message_id, idx)
);";

/// How long a stashed body waits for its message to reappear in another folder.
const STASH_TTL_SECS: i64 = 7 * 24 * 3600;

/// A message's flags as fetched (incremental flag sync).
#[derive(Debug, Clone, Default)]
pub struct FlagChange {
    pub uid: u32,
    pub flags: i64,
    pub keywords: Vec<String>,
    /// Gmail labels, when the server is Gmail.
    pub labels: Option<Vec<String>>,
}

/// Local state of some messages before an action (undo restores it).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub entries: Vec<SnapEntry>,
    pub flags: Vec<(i64, i64)>,
    /// (message, stored keywords)
    pub keywords: Vec<(i64, String)>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapEntry {
    pub folder_id: i64,
    pub message_id: i64,
    pub uid: Option<u32>,
    pub date: i64,
}

/// The local effect of an action.
#[derive(Debug, Clone, Default)]
pub struct LocalChange {
    pub message_ids: Vec<i64>,
    /// (folder, message) entries to drop / add.
    pub remove: Vec<(i64, i64)>,
    pub add: Vec<(i64, i64)>,
    /// (message, flag bits to set, flag bits to clear).
    pub flags: Vec<(i64, i64, i64)>,
    /// (message, keyword, add?)
    pub tags: Vec<(i64, String, bool)>,
}

#[derive(Debug, Clone)]
pub struct PendingOp {
    pub id: i64,
    pub ops: String,
    pub snapshot: String,
    pub folders: String,
    pub attempts: i64,
}

/// Header data for one message as fetched from a folder.
#[derive(Debug, Clone)]
pub struct HeaderRecord {
    pub uid: u32,
    pub gm_msgid: Option<u64>,
    /// RFC 8474 EMAILID (servers with OBJECTID), identical for every copy of a message.
    pub email_id: Option<String>,
    pub gm_labels: Option<Vec<String>>,
    /// IMAP keywords (tags), lowercase.
    pub keywords: Vec<String>,
    pub message_id: Option<String>,
    /// The Message-ID the sender gave it, where the server replaced it (Exchange keeps it in
    /// X-Microsoft-Original-Message-ID): how our own sent copy is recognised there.
    pub original_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub refs: Option<String>,
    pub subject: String,
    pub from_name: String,
    pub from_addr: String,
    pub to_addrs: String,
    pub date: i64,
    pub size: u32,
    pub flags: i64,
    pub has_attachments: bool,
}

/// Sync bookkeeping for one folder.
#[derive(Debug, Clone, Default)]
pub struct FolderSyncState {
    pub uidvalidity: u32,
    pub highest_modseq: u64,
    pub top_uid: u32,
    pub low_uid: u32,
    pub complete: bool,
}

/// Where to fetch a message body from.
#[derive(Debug, Clone)]
pub struct BodyLocation {
    pub message_id: i64,
    pub folder_id: i64,
    pub raw_name: String,
    pub uid: u32,
    pub size: u32,
}

/// A folder as returned by IMAP LIST.
#[derive(Debug, Clone)]
pub struct ListedFolder {
    pub raw_name: String,
    pub path: Vec<String>,
    pub role: Option<FolderRole>,
    pub selectable: bool,
}

/// Message-IDs mentioned in In-Reply-To / References, without angle brackets.
/// Cross-folder identity of a message on servers without Gmail ids: the server's EMAILID
/// (RFC 8474) when it has one, else Message-ID plus size (copies of a message are byte-for-byte
/// identical). Gmail messages use `gm_msgid` instead; messages with neither stay per-folder.
pub fn message_identity(rec: &HeaderRecord) -> Option<String> {
    if rec.gm_msgid.is_some() {
        return None;
    }
    if let Some(e) = &rec.email_id {
        return Some(format!("e:{e}"));
    }
    rec.message_id.as_ref().filter(|m| !m.is_empty()).map(|m| format!("m:{m}:{}", rec.size))
}

/// Stored id lists are clean (see `mime::parse_msg_ids`): space-separated ids. Brackets are
/// tolerated in case a caller passes header-style values.
fn referenced_ids(in_reply_to: Option<&str>, refs: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in [refs, in_reply_to].into_iter().flatten() {
        for id in s.split_whitespace().map(|t| t.trim_matches(['<', '>'])).filter(|t| !t.is_empty()) {
            if !out.iter().any(|o| o == id) {
                out.push(id.to_string());
            }
        }
    }
    out
}

/// Put a newly inserted message into a conversation (JWZ-style, without subject matching):
/// it joins every thread that contains a message it references, a message referencing it,
/// a message sharing one of its references (siblings under a missing parent), or a copy
/// with the same Message-ID. Threads it connects are merged into the oldest one.
fn assign_thread(
    tx: &rusqlite::Transaction,
    account_id: i64,
    id: i64,
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
    refs: Option<&str>,
) -> Result<()> {
    let own = message_id.map(|m| m.trim().trim_matches(['<', '>'])).filter(|m| !m.is_empty());
    let mut referenced = referenced_ids(in_reply_to, refs);
    referenced.retain(|r| Some(r.as_str()) != own);

    let mut threads = std::collections::BTreeSet::new();
    {
        let mut by_msgid = tx.prepare_cached(
            "SELECT thread_id FROM messages WHERE account_id=?1 AND message_id=?2 AND thread_id IS NOT NULL",
        )?;
        let mut by_ref = tx.prepare_cached(
            "SELECT m.thread_id FROM msgrefs r JOIN messages m ON m.id=r.message_id
             WHERE r.account_id=?1 AND r.ref_msgid=?2 AND m.thread_id IS NOT NULL",
        )?;
        for key in referenced.iter().map(String::as_str).chain(own) {
            for t in by_msgid.query_map(params![account_id, key], |r| r.get::<_, i64>(0))? {
                threads.insert(t?);
            }
            for t in by_ref.query_map(params![account_id, key], |r| r.get::<_, i64>(0))? {
                threads.insert(t?);
            }
        }
    }
    let thread = threads.first().copied().unwrap_or(id);
    {
        let mut merge = tx.prepare_cached("UPDATE messages SET thread_id=?1 WHERE thread_id=?2")?;
        for other in threads.iter().skip(1) {
            merge.execute(params![thread, other])?;
        }
    }
    tx.execute("UPDATE messages SET thread_id=?2 WHERE id=?1", params![id, thread])?;
    let mut ins = tx.prepare_cached("INSERT INTO msgrefs (account_id, ref_msgid, message_id) VALUES (?1,?2,?3)")?;
    for r in &referenced {
        ins.execute(params![account_id, r, id])?;
    }
    Ok(())
}

/// A message whose snooze went off sorts in the Inbox by that time (its `$woke-` keyword, on
/// every client); otherwise, and everywhere else, by its own date.
fn settle_inbox_date(db: &Connection, message_id: i64) -> Result<()> {
    let row: Option<(i64, String)> = db
        .prepare_cached("SELECT date, keywords FROM messages WHERE id=?1")?
        .query_row([message_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((date, keywords)) = row else { return Ok(()) };
    let date = crate::tags::woken_at(&keywords).map_or(date, |t| date.max(t));
    db.prepare_cached(
        "UPDATE entries SET date=?2 WHERE message_id=?1 AND date<>?2
           AND folder_id IN (SELECT id FROM folders WHERE role='inbox')",
    )?
    .execute(params![message_id, date])?;
    Ok(())
}

/// Make a message's membership in label-backed folders match its Gmail labels exactly.
/// `current` (the folder being synced) is left alone; folders not in `label_folders`
/// (All Mail, Trash, Spam) are never touched. A message in Trash or Spam keeps its labels
/// (restoring it brings them back) but Gmail shows it under none of them: it's in no label
/// folder while it's there.
fn apply_gmail_labels(
    tx: &rusqlite::Transaction,
    message_id: i64,
    current: i64,
    date: i64,
    labels: &[String],
    label_folders: &std::collections::HashMap<String, i64>,
    changed: &mut std::collections::BTreeSet<i64>,
) -> Result<()> {
    let managed: std::collections::HashSet<i64> = label_folders.values().copied().collect();
    let binned: bool = tx
        .prepare_cached("SELECT 1 FROM folders WHERE id=?1 AND role IN ('trash','junk')")?
        .exists([current])?;
    let wanted: std::collections::HashSet<i64> = if binned {
        Default::default()
    } else {
        labels.iter().filter_map(|l| label_folders.get(l).copied()).filter(|&f| f != current).collect()
    };
    let existing: Vec<i64> = {
        let mut stmt = tx.prepare_cached("SELECT folder_id FROM entries WHERE message_id=?1")?;
        stmt.query_map([message_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let mut del = tx.prepare_cached("DELETE FROM entries WHERE folder_id=?1 AND message_id=?2")?;
    for fid in &existing {
        if *fid != current && managed.contains(fid) && !wanted.contains(fid) {
            del.execute(params![fid, message_id])?;
            changed.insert(*fid);
        }
    }
    let mut ins = tx.prepare_cached(
        "INSERT INTO entries (folder_id, message_id, uid, date) VALUES (?1,?2,NULL,?3)
         ON CONFLICT(folder_id, message_id) DO NOTHING",
    )?;
    for fid in wanted {
        if !existing.contains(&fid) {
            ins.execute(params![fid, message_id, date])?;
            changed.insert(fid);
        }
    }
    Ok(())
}

/// Conversation scope filter over `messages m`; params ?2 = current folder, ?3 = all folders,
/// ?4 = include Trash/Spam. See [`ThreadScope`].
const SCOPE_FILTER: &str = "(
    EXISTS (SELECT 1 FROM entries e WHERE e.message_id = m.id AND e.folder_id = ?2)
    OR (?3 AND EXISTS (SELECT 1 FROM entries e JOIN folders f ON f.id = e.folder_id
            WHERE e.message_id = m.id AND coalesce(f.role, '') NOT IN ('trash', 'junk')))
    OR (?4 AND EXISTS (SELECT 1 FROM entries e JOIN folders f ON f.id = e.folder_id
            WHERE e.message_id = m.id AND f.role IN ('trash', 'junk'))))";

/// Writers (sync) and readers (UI) use separate connections so that WAL lets the UI read
/// while a large header batch is being written.
/// A conversation member as `list_threads` reads it: id, subject, from name, from address,
/// flags, has attachments, snippet, Message-ID, keywords, send state (None / Some(failed)).
type Member = (i64, String, String, String, i64, bool, Option<String>, Option<String>, String, Option<bool>);

/// A message to queue for sending (see `Store::enqueue_outgoing`).
pub struct NewOutgoing {
    pub account_id: i64,
    pub raw: Vec<u8>,
    pub envelope_from: String,
    pub recipients: Vec<String>,
    pub not_before_ms: i64,
    pub replied: Option<i64>,
    pub forwarded: Option<i64>,
    pub draft: Option<String>,
}

/// A queued message claimed for sending.
#[derive(Debug)]
pub struct OutgoingRow {
    pub id: i64,
    pub raw: Vec<u8>,
    pub envelope_from: String,
    pub recipients: Vec<String>,
    pub attempts: i64,
    pub replied: Option<i64>,
    pub forwarded: Option<i64>,
}

pub struct Store {
    db: Mutex<Connection>,
    read: Mutex<Connection>,
    bodies: Mutex<Connection>,
    bodies_read: Mutex<Connection>,
    dir: std::path::PathBuf,
    /// Bumped by every local change (an action applied or undone). A sync step that read the
    /// server before the change must not write its results over it: see [`SyncSuperseded`].
    local_epoch: AtomicU64,
}

/// A sync step's server data predates a local change (the user acted while it ran), so writing
/// it would undo that change on screen until the next sync. Nothing was written; the folder is
/// synced again once the action has reached the server.
#[derive(Debug)]
pub struct SyncSuperseded;

impl std::fmt::Display for SyncSuperseded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("local change during sync step")
    }
}

impl std::error::Error for SyncSuperseded {}

fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;
         PRAGMA temp_store=MEMORY; PRAGMA cache_size=-65536;",
    )?;
    Ok(conn)
}

/// The mail database was last opened by a newer version of the app (its schema is ahead of this
/// build's): refused, untouched. The app tells the user to update.
#[derive(Debug)]
pub struct NewerDatabase {
    pub dir: std::path::PathBuf,
    pub version: i64,
    pub supported: i64,
}

impl std::fmt::Display for NewerDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The mail database in {} is from a newer version of the app (schema {}; this version knows up to {}).",
            self.dir.display(),
            self.version,
            self.supported
        )
    }
}

impl std::error::Error for NewerDatabase {}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        let mut db = open(&dir.join("mail.db"))?;
        let fresh = !db.prepare("SELECT 1 FROM sqlite_master")?.exists([])?;
        let mut version: i64 = if fresh { SCHEMA_VERSION } else { db.query_row("PRAGMA user_version", [], |r| r.get(0))? };
        if version > SCHEMA_VERSION {
            return Err(NewerDatabase { dir: dir.to_path_buf(), version, supported: SCHEMA_VERSION }.into());
        }
        if version < BASE_VERSION {
            tracing::warn!(version, "mail database older than the base schema; starting over");
            drop(db);
            for f in ["mail.db", "mail.db-wal", "mail.db-shm", "bodies.db", "bodies.db-wal", "bodies.db-shm"] {
                let p = dir.join(f);
                if p.exists() {
                    std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
                }
            }
            db = open(&dir.join("mail.db"))?;
            version = SCHEMA_VERSION;
        }
        while version < SCHEMA_VERSION {
            let tx = db.transaction()?;
            MIGRATIONS[(version - BASE_VERSION) as usize](&tx)?;
            version += 1;
            tx.execute_batch(&format!("PRAGMA user_version={version};"))?;
            tx.commit()?;
            tracing::info!(version, "mail database upgraded");
        }
        db.execute_batch(SCHEMA)?;
        db.execute_batch(&format!("PRAGMA user_version={SCHEMA_VERSION};"))?;
        let bodies = open(&dir.join("bodies.db"))?;
        bodies.execute_batch(BODY_SCHEMA)?;
        bodies.execute_batch(ATTACHMENT_SCHEMA)?;
        bodies.execute(
            "DELETE FROM stash WHERE created < unixepoch() - ?1",
            [STASH_TTL_SECS],
        )?;
        let have_tags: bool = db.prepare("SELECT 1 FROM tags LIMIT 1")?.exists([])?;
        if !have_tags {
            for (i, (k, n, c)) in crate::tags::DEFAULTS.iter().enumerate() {
                db.execute("INSERT INTO tags (keyword, name, color, position) VALUES (?1,?2,?3,?4)", params![k, n, c, i as i64])?;
            }
        }
        let store = Store {
            db: Mutex::new(db),
            read: Mutex::new(open(&dir.join("mail.db"))?),
            bodies: Mutex::new(bodies),
            bodies_read: Mutex::new(open(&dir.join("bodies.db"))?),
            dir: dir.to_path_buf(),
            local_epoch: AtomicU64::new(0),
        };
        store.mark_stored_attachments()?;
        store.strip_unwanted_attachments()?;
        store.drop_matched_local_copies(None)?;
        store.drop_resolved_stand_ins(None)?;
        Ok(store)
    }

    /// Per account: messages, their size on the server, and the space their bodies and
    /// separately fetched attachments take here. Reads every stored body's length, which takes
    /// seconds on a cold disk, so on connections of its own (the app's reads aren't held up).
    pub fn account_storage(&self) -> Result<Vec<AccountStorage>> {
        let db = Connection::open_with_flags(self.dir.join("mail.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.execute("ATTACH DATABASE ?1 AS b", [self.dir.join("bodies.db").to_string_lossy()])?;
        let mut stmt = db.prepare(
            "SELECT m.account_id, count(*), coalesce(sum(m.size), 0),
                    coalesce(sum(length(x.raw)), 0)
                      + coalesce((SELECT sum(length(c.data)) FROM b.attachment_cache c
                                  JOIN messages n ON n.id = c.message_id WHERE n.account_id = m.account_id), 0)
             FROM messages m LEFT JOIN b.bodies x ON x.message_id = m.id
             GROUP BY m.account_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AccountStorage {
                account_id: r.get(0)?,
                messages: r.get::<_, i64>(1)? as u64,
                server_bytes: r.get::<_, i64>(2)? as u64,
                local_bytes: r.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The writer, for a sync step that started at local epoch `epoch` (None: unconditional).
    /// Checked under the writer lock, which local changes also hold, so no change slips between.
    fn db_for_sync(&self, epoch: Option<u64>) -> Result<std::sync::MutexGuard<'_, Connection>> {
        let db = self.db();
        if epoch.is_some_and(|e| e != self.local_epoch()) {
            return Err(SyncSuperseded.into());
        }
        Ok(db)
    }

    /// See [`SyncSuperseded`]; sync steps capture this when they start.
    pub fn local_epoch(&self) -> u64 {
        self.local_epoch.load(Ordering::SeqCst)
    }

    fn rdb(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.read.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- accounts -------------------------------------------------------------------------

    pub fn insert_account(
        &self,
        email: &str,
        display_name: &str,
        provider: Provider,
        host: &str,
        port: u16,
    ) -> Result<Account> {
        let db = self.db();
        db.execute(
            "INSERT INTO accounts (email, display_name, provider, imap_host, imap_port)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(email) DO UPDATE SET display_name=excluded.display_name,
                provider=excluded.provider, imap_host=excluded.imap_host, imap_port=excluded.imap_port",
            params![email, display_name, provider.as_str(), host, port],
        )?;
        let id: i64 = db.query_row("SELECT id FROM accounts WHERE email=?1", [email], |r| r.get(0))?;
        let (smtp_host, smtp_port, auth, keywords, mark_letter, mark_color) = db.query_row(
            "SELECT smtp_host, smtp_port, auth, keywords, mark_letter, mark_color FROM accounts WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, AuthKind::parse(&r.get::<_, String>(2)?), r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;
        Ok(Account {
            id,
            email: email.into(),
            display_name: display_name.into(),
            provider,
            imap_host: host.into(),
            imap_port: port,
            smtp_host,
            smtp_port,
            auth,
            keywords,
            mark_letter,
            mark_color,
        })
    }

    /// Record whether the account's server keeps custom keywords; true if that changed.
    pub fn set_keywords_supported(&self, account_id: i64, yes: bool) -> Result<bool> {
        Ok(self.db().execute(
            "UPDATE accounts SET keywords=?2 WHERE id=?1 AND keywords IS NOT ?2",
            params![account_id, yes],
        )? > 0)
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, email, display_name, provider, imap_host, imap_port, smtp_host, smtp_port, auth, keywords,
                    mark_letter, mark_color
             FROM accounts ORDER BY position IS NULL, position, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Account {
                id: r.get(0)?,
                email: r.get(1)?,
                display_name: r.get(2)?,
                provider: Provider::parse(&r.get::<_, String>(3)?),
                imap_host: r.get(4)?,
                imap_port: r.get(5)?,
                smtp_host: r.get(6)?,
                smtp_port: r.get(7)?,
                auth: AuthKind::parse(&r.get::<_, String>(8)?),
                keywords: r.get(9)?,
                mark_letter: r.get(10)?,
                mark_color: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_account(&self, account_id: i64) -> Result<()> {
        let db = self.db();
        let ids: Vec<i64> = {
            let mut stmt = db.prepare("SELECT id FROM messages WHERE account_id=?1")?;
            stmt.query_map([account_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        db.execute("DELETE FROM accounts WHERE id=?1", [account_id])?;
        drop(db);
        let bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        let tx = bodies.unchecked_transaction()?;
        tx.execute("DELETE FROM stash WHERE account_id=?1", [account_id])?;
        for id in ids {
            tx.execute("DELETE FROM bodies WHERE message_id=?1", [id])?;
            tx.execute("DELETE FROM attachment_cache WHERE message_id=?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- folders --------------------------------------------------------------------------

    /// Replace the folder list with the result of LIST, keeping sync state of existing folders.
    pub fn sync_folder_list(&self, account_id: i64, listed: &[ListedFolder]) -> Result<()> {
        // Every IMAP server lists INBOX. A listing without it is a server glitch, not a mass
        // deletion: update what's listed but remove nothing.
        let trustworthy = listed.iter().any(|f| f.raw_name.eq_ignore_ascii_case("INBOX"));
        let mut db = self.db();
        let tx = db.transaction()?;
        if trustworthy {
            tx.execute("UPDATE folders SET present=0 WHERE account_id=?1", [account_id])?;
        } else {
            tracing::warn!(account_id, n = listed.len(), "folder list without INBOX; not removing any folders");
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO folders (account_id, raw_name, path, role, selectable, present)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)
                 ON CONFLICT(account_id, raw_name) DO UPDATE SET path=excluded.path,
                    role=excluded.role, selectable=excluded.selectable, present=1",
            )?;
            for f in listed {
                stmt.execute(params![
                    account_id,
                    f.raw_name,
                    serde_json::to_string(&f.path)?,
                    f.role.map(|r| r.as_str()),
                    f.selectable,
                ])?;
            }
        }
        // Folders deleted on the server (by another client): their entries go with them
        // (ON DELETE CASCADE); messages that were only there are then dropped below.
        let candidates: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT e.message_id FROM entries e JOIN folders f ON f.id = e.folder_id
                 WHERE f.account_id=?1 AND f.present=0",
            )?;
            stmt.query_map([account_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let gone: Vec<String> = {
            let mut stmt = tx.prepare("SELECT raw_name FROM folders WHERE account_id=?1 AND present=0")?;
            stmt.query_map([account_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        tx.execute("DELETE FROM folders WHERE account_id=?1 AND present=0", [account_id])?;
        tx.commit()?;
        drop(db);
        if !gone.is_empty() {
            let dropped = self.drop_orphans(&candidates)?;
            tracing::info!(?gone, messages_dropped = dropped, "folders removed on the server");
        }
        Ok(())
    }

    pub fn folders(&self, account_id: i64) -> Result<Vec<Folder>> {
        let db = self.rdb();
        let mut stmt = db.prepare(
            "SELECT f.id, f.account_id, f.raw_name, f.path, f.role, f.selectable, f.total, f.unread,
                    (SELECT count(*) FROM entries e WHERE e.folder_id = f.id)
             FROM folders f WHERE f.account_id=?1 ORDER BY f.raw_name",
        )?;
        let rows = stmt.query_map([account_id], |r| {
            let path: Vec<String> = serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default();
            let role = r.get::<_, Option<String>>(4)?.and_then(|s| FolderRole::parse(&s));
            let display_name = match role {
                Some(role) => role.friendly_name().to_string(),
                None => path.last().cloned().unwrap_or_default(),
            };
            Ok(Folder {
                id: r.get(0)?,
                account_id: r.get(1)?,
                raw_name: r.get(2)?,
                path,
                display_name,
                role,
                selectable: r.get(5)?,
                total: r.get(6)?,
                unread: r.get(7)?,
                synced_count: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn folder(&self, folder_id: i64) -> Result<Option<(i64, String)>> {
        Ok(self
            .db()
            .query_row("SELECT account_id, raw_name FROM folders WHERE id=?1", [folder_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?)
    }

    pub fn set_folder_counts(&self, folder_id: i64, total: u32, unread: u32) -> Result<()> {
        self.db().execute(
            "UPDATE folders SET total=?2, unread=?3 WHERE id=?1",
            params![folder_id, total, unread],
        )?;
        Ok(())
    }

    pub fn folder_sync_state(&self, folder_id: i64) -> Result<FolderSyncState> {
        Ok(self.db().query_row(
            "SELECT uidvalidity, highest_modseq, top_uid, low_uid, complete FROM folders WHERE id=?1",
            [folder_id],
            |r| {
                Ok(FolderSyncState {
                    uidvalidity: r.get(0)?,
                    highest_modseq: r.get::<_, i64>(1)? as u64,
                    top_uid: r.get(2)?,
                    low_uid: r.get(3)?,
                    complete: r.get(4)?,
                })
            },
        )?)
    }

    /// `top_uid` only ever grows here: the IDLE fast path may have advanced it concurrently.
    pub fn set_folder_sync_state(&self, folder_id: i64, s: &FolderSyncState) -> Result<()> {
        self.db().execute(
            "UPDATE folders SET uidvalidity=?2, highest_modseq=?3,
                top_uid=CASE WHEN uidvalidity=?2 THEN max(top_uid, ?4) ELSE ?4 END,
                low_uid=?5, complete=?6
             WHERE id=?1",
            params![folder_id, s.uidvalidity, s.highest_modseq as i64, s.top_uid, s.low_uid, s.complete],
        )?;
        Ok(())
    }

    pub fn bump_top_uid(&self, folder_id: i64, uid: u32) -> Result<()> {
        self.db().execute("UPDATE folders SET top_uid=max(top_uid, ?2) WHERE id=?1", params![folder_id, uid])?;
        Ok(())
    }

    /// UIDVALIDITY changed: every UID we know for this folder is meaningless.
    pub fn reset_folder(&self, folder_id: i64) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM entries WHERE folder_id=?1", [folder_id])?;
        db.execute(
            "UPDATE folders SET uidvalidity=0, highest_modseq=0, top_uid=0, low_uid=0, complete=0 WHERE id=?1",
            [folder_id],
        )?;
        Ok(())
    }

    pub fn local_uid_count(&self, folder_id: i64) -> Result<u32> {
        Ok(self.db().query_row(
            "SELECT count(*) FROM entries WHERE folder_id=?1 AND uid IS NOT NULL",
            [folder_id],
            |r| r.get(0),
        )?)
    }

    /// (messages, unread) stored for `folder_id` with a server UID: what a STATUS of it should say.
    pub fn local_counts(&self, folder_id: i64) -> Result<(u32, u32)> {
        Ok(self.db().query_row(
            "SELECT count(*), coalesce(sum((m.flags & 1) = 0), 0)
             FROM entries e JOIN messages m ON m.id = e.message_id
             WHERE e.folder_id=?1 AND e.uid IS NOT NULL",
            [folder_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn local_uids(&self, folder_id: i64) -> Result<Vec<u32>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT uid FROM entries WHERE folder_id=?1 AND uid IS NOT NULL")?;
        let rows = stmt.query_map([folder_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- messages -------------------------------------------------------------------------

    /// Store a batch of headers fetched from `folder_id`. With Gmail labels, also records
    /// membership in every labelled folder (`label_folders` maps label -> folder id).
    /// Returns the set of folder ids whose contents changed.
    pub fn upsert_headers(
        &self,
        account_id: i64,
        folder_id: i64,
        records: &[HeaderRecord],
        label_folders: &std::collections::HashMap<String, i64>,
    ) -> Result<Vec<i64>> {
        self.upsert_headers_since(None, account_id, folder_id, records, label_folders)
    }

    /// [`Self::upsert_headers`] for a sync step that started at local epoch `epoch`: fails with
    /// [`SyncSuperseded`], writing nothing, if a local change happened since.
    pub fn upsert_headers_since(
        &self,
        epoch: Option<u64>,
        account_id: i64,
        folder_id: i64,
        records: &[HeaderRecord],
        label_folders: &std::collections::HashMap<String, i64>,
    ) -> Result<Vec<i64>> {
        let mut changed = std::collections::BTreeSet::from([folder_id]);
        // New rows whose body may be waiting in the stash (moved between folders).
        let mut restore: Vec<(i64, String)> = Vec::new();
        let mut db = self.db_for_sync(epoch)?;
        let tx = db.transaction()?;
        {
            let mut by_gm = tx.prepare_cached(
                "SELECT id FROM messages WHERE account_id=?1 AND gm_msgid=?2",
            )?;
            let mut by_identity =
                tx.prepare_cached("SELECT id FROM messages WHERE account_id=?1 AND identity=?2")?;
            let mut uid_here = tx.prepare_cached(
                "SELECT uid FROM entries WHERE folder_id=?1 AND message_id=?2 AND uid IS NOT NULL",
            )?;
            let mut by_uid =
                tx.prepare_cached("SELECT message_id FROM entries WHERE folder_id=?1 AND uid=?2")?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO messages (account_id, gm_msgid, message_id, in_reply_to, refs, subject,
                    from_name, from_addr, to_addrs, date, size, flags, has_attachments, identity, keywords)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            )?;
            let mut update_flags = tx.prepare_cached("UPDATE messages SET flags=?2, keywords=?3 WHERE id=?1")?;
            // Our local copy of a message we sent: the server's copy takes it over (same row, so
            // no duplicate and the conversation keeps its place).
            let mut by_local = tx.prepare_cached(
                "SELECT id FROM messages WHERE account_id=?1 AND local_copy=1 AND gm_msgid IS NULL AND message_id=?2",
            )?;
            let mut adopt = tx.prepare_cached(
                "UPDATE messages SET local_copy=0, pending_send=NULL, gm_msgid=coalesce(gm_msgid, ?2),
                    identity=coalesce(identity, ?3)
                 WHERE id=?1 AND local_copy=1",
            )?;
            // A message we moved (or restored) here locally waits as an entry without a UID. Its
            // server copy takes that row over by Message-ID alone: some servers (Exchange) report
            // a different size after a move, so the identity (Message-ID + size) wouldn't match.
            let mut stand_in = tx.prepare_cached(
                "SELECT e.message_id FROM entries e JOIN messages m ON m.id = e.message_id
                 WHERE e.folder_id=?1 AND e.uid IS NULL AND m.account_id=?2 AND m.message_id=?3
                   AND m.gm_msgid IS NULL
                 LIMIT 1",
            )?;
            let mut reidentify =
                tx.prepare_cached("UPDATE messages SET identity=coalesce(?2, identity), size=?3 WHERE id=?1")?;
            let mut entry = tx.prepare_cached(
                "INSERT INTO entries (folder_id, message_id, uid, date) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(folder_id, message_id) DO UPDATE SET uid=coalesce(excluded.uid, entries.uid)",
            )?;
            for rec in records {
                let mut identity = message_identity(rec);
                let existing: Option<i64> = match rec.gm_msgid {
                    Some(gm) => match by_gm.query_row(params![account_id, gm as i64], |r| r.get(0)).optional()? {
                        Some(id) => Some(id),
                        None => match &rec.message_id {
                            Some(mid) => by_local.query_row(params![account_id, mid], |r| r.get(0)).optional()?,
                            None => None,
                        },
                    },
                    None => match by_uid.query_row(params![folder_id, rec.uid], |r| r.get(0)).optional()? {
                        Some(id) => Some(id),
                        None => match &identity {
                            Some(key) => {
                                let found: Option<i64> =
                                    by_identity.query_row(params![account_id, key], |r| r.get(0)).optional()?;
                                // Two copies in the *same* folder (a duplicate delivery) can't share a
                                // row: each needs its own UID entry. Keep this one separate.
                                let dup_here = match found {
                                    Some(id) => uid_here
                                        .query_row(params![folder_id, id], |r| r.get::<_, u32>(0))
                                        .optional()?
                                        .is_some_and(|u| u != rec.uid),
                                    None => false,
                                };
                                if dup_here {
                                    identity = None;
                                    None
                                } else {
                                    found
                                }
                            }
                            None => None,
                        },
                    },
                };
                // Not known yet: maybe a copy we moved here that's waiting for its UID.
                let existing = match existing {
                    Some(id) => Some(id),
                    None if rec.gm_msgid.is_none() => match rec.message_id.as_deref().filter(|m| !m.is_empty()) {
                        Some(mid) => {
                            let found: Option<i64> =
                                stand_in.query_row(params![folder_id, account_id, mid], |r| r.get(0)).optional()?;
                            if let Some(id) = found {
                                reidentify.execute(params![id, identity, rec.size])?;
                            }
                            found
                        }
                        None => None,
                    },
                    None => None,
                };
                // Not known yet: maybe it's our local copy of a message we sent (on any server), by
                // its Message-ID or, where the server replaced that, the original.
                let existing = match existing {
                    Some(id) => Some(id),
                    None => {
                        let mut found = None;
                        for mid in [&rec.message_id, &rec.original_message_id].into_iter().flatten() {
                            found = by_local.query_row(params![account_id, mid], |r| r.get(0)).optional()?;
                            if found.is_some() {
                                break;
                            }
                        }
                        found
                    }
                };
                let id = match existing {
                    Some(id) => {
                        update_flags.execute(params![id, rec.flags, crate::tags::join(&rec.keywords)])?;
                        adopt.execute(params![id, rec.gm_msgid.map(|g| g as i64), identity])?;
                        id
                    }
                    None => {
                        insert.execute(params![
                            account_id,
                            rec.gm_msgid.map(|g| g as i64),
                            rec.message_id,
                            rec.in_reply_to,
                            rec.refs,
                            rec.subject,
                            rec.from_name,
                            rec.from_addr,
                            rec.to_addrs,
                            rec.date,
                            rec.size,
                            rec.flags,
                            rec.has_attachments,
                            identity,
                            crate::tags::join(&rec.keywords),
                        ])?;
                        let id = tx.last_insert_rowid();
                        if let Some(key) = identity {
                            restore.push((id, key));
                        }
                        assign_thread(
                            &tx,
                            account_id,
                            id,
                            rec.message_id.as_deref(),
                            rec.in_reply_to.as_deref(),
                            rec.refs.as_deref(),
                        )?;
                        id
                    }
                };
                entry.execute(params![folder_id, id, rec.uid, rec.date])?;

                if let Some(labels) = &rec.gm_labels {
                    apply_gmail_labels(&tx, id, folder_id, rec.date, labels, label_folders, &mut changed)?;
                }
                if crate::tags::woken_at(&crate::tags::join(&rec.keywords)).is_some() {
                    settle_inbox_date(&tx, id)?;
                }
            }
        }
        tx.commit()?;
        drop(db);
        if !restore.is_empty() {
            self.restore_stashed(account_id, &restore)?;
        }
        Ok(changed.into_iter().collect())
    }

    /// Give newly inserted messages the bodies of identical messages that recently left
    /// another folder (a move seen as expunge-then-new), instead of downloading them again.
    fn restore_stashed(&self, account_id: i64, new_rows: &[(i64, String)]) -> Result<()> {
        let mut found = Vec::new();
        {
            let bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
            let tx = bodies.unchecked_transaction()?;
            {
                let mut get = tx.prepare_cached(
                    "SELECT snippet, has_attachments, omitted, allow_remote FROM stash
                     WHERE account_id=?1 AND identity=?2",
                )?;
                let mut take = tx.prepare_cached(
                    "INSERT OR REPLACE INTO bodies (message_id, raw)
                     SELECT ?3, raw FROM stash WHERE account_id=?1 AND identity=?2",
                )?;
                let mut del = tx.prepare_cached("DELETE FROM stash WHERE account_id=?1 AND identity=?2")?;
                for (id, key) in new_rows {
                    let meta: Option<(Option<String>, bool, Option<String>, bool)> = get
                        .query_row(params![account_id, key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                        .optional()?;
                    if let Some(meta) = meta {
                        take.execute(params![account_id, key, id])?;
                        del.execute(params![account_id, key])?;
                        found.push((*id, meta));
                    }
                }
            }
            tx.commit()?;
        }
        if !found.is_empty() {
            let db = self.db();
            let mut set = db.prepare_cached(
                "UPDATE messages SET has_body=1, snippet=?2, has_attachments=?3, omitted=?4, allow_remote=?5
                 WHERE id=?1",
            )?;
            for (id, (snippet, att, omitted, remote)) in found {
                set.execute(params![id, snippet, att, omitted, remote])?;
            }
        }
        Ok(())
    }

    /// Apply flag (and Gmail label) changes for UIDs already known in this folder.
    pub fn update_flags(
        &self,
        folder_id: i64,
        changes: &[FlagChange],
        label_folders: &std::collections::HashMap<String, i64>,
    ) -> Result<Vec<i64>> {
        self.update_flags_since(None, folder_id, changes, label_folders)
    }

    /// [`Self::update_flags`] for a sync step that started at local epoch `epoch` (see
    /// [`Self::upsert_headers_since`]).
    pub fn update_flags_since(
        &self,
        epoch: Option<u64>,
        folder_id: i64,
        changes: &[FlagChange],
        label_folders: &std::collections::HashMap<String, i64>,
    ) -> Result<Vec<i64>> {
        let mut changed = std::collections::BTreeSet::from([folder_id]);
        let mut db = self.db_for_sync(epoch)?;
        let tx = db.transaction()?;
        {
            let mut find = tx.prepare_cached(
                "SELECT e.message_id, e.date FROM entries e WHERE e.folder_id=?1 AND e.uid=?2",
            )?;
            let mut set = tx.prepare_cached("UPDATE messages SET flags=?2, keywords=?3 WHERE id=?1")?;
            for c in changes {
                let Some((id, date)): Option<(i64, i64)> =
                    find.query_row(params![folder_id, c.uid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
                else {
                    continue;
                };
                set.execute(params![id, c.flags, crate::tags::join(&c.keywords)])?;
                if let Some(labels) = &c.labels {
                    apply_gmail_labels(&tx, id, folder_id, date, labels, label_folders, &mut changed)?;
                }
                // A snooze that went off (or was snoozed again) on another client.
                settle_inbox_date(&tx, id)?;
            }
        }
        tx.commit()?;
        Ok(changed.into_iter().collect())
    }

    /// Remove entries for UIDs that no longer exist on the server, then drop orphaned messages.
    pub fn remove_uids(&self, folder_id: i64, uids: &[u32]) -> Result<()> {
        let mut candidates = Vec::new();
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            {
                let mut find = tx.prepare_cached("SELECT message_id FROM entries WHERE folder_id=?1 AND uid=?2")?;
                let mut del = tx.prepare_cached("DELETE FROM entries WHERE folder_id=?1 AND uid=?2")?;
                for uid in uids {
                    if let Some(id) = find.query_row(params![folder_id, uid], |r| r.get::<_, i64>(0)).optional()? {
                        del.execute(params![folder_id, uid])?;
                        candidates.push(id);
                    }
                }
            }
            tx.commit()?;
        }
        self.drop_orphans(&candidates)?;
        Ok(())
    }

    /// Delete those of `candidates` that no longer exist on the server in any folder (no entry
    /// with a UID), with their bodies and cached attachments. Bodies of messages with a
    /// cross-folder identity are stashed first: they may reappear in a folder we haven't synced
    /// yet (a move, or a folder rename seen as delete + create). Returns how many were dropped.
    pub fn drop_orphans(&self, candidates: &[i64]) -> Result<usize> {
        type Meta = (i64, Option<String>, bool, Option<String>, bool, Option<String>, bool);
        let mut orphans: Vec<(i64, Meta)> = Vec::new();
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            {
                let mut has_uid_entry =
                    tx.prepare_cached("SELECT 1 FROM entries WHERE message_id=?1 AND uid IS NOT NULL LIMIT 1")?;
                let mut meta = tx.prepare_cached(
                    "SELECT account_id, identity, has_body, snippet, has_attachments, omitted, allow_remote
                     FROM messages WHERE id=?1",
                )?;
                let mut del_msg = tx.prepare_cached("DELETE FROM messages WHERE id=?1")?;
                let mut seen = std::collections::HashSet::new();
                for &id in candidates {
                    if !seen.insert(id) || has_uid_entry.exists([id])? {
                        continue;
                    }
                    let Some(m) = meta
                        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
                        .optional()?
                    else {
                        continue;
                    };
                    del_msg.execute([id])?;
                    orphans.push((id, m));
                }
            }
            tx.commit()?;
        }
        if orphans.is_empty() {
            return Ok(0);
        }
        let n = orphans.len();
        let bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        let tx = bodies.unchecked_transaction()?;
        for (id, (account_id, identity, has_body, snippet, att, omitted, remote)) in orphans {
            if let (Some(key), true) = (identity, has_body) {
                tx.execute(
                    "INSERT OR REPLACE INTO stash
                         (account_id, identity, raw, snippet, has_attachments, omitted, allow_remote, created)
                     SELECT ?2, ?3, raw, ?4, ?5, ?6, ?7, unixepoch() FROM bodies WHERE message_id=?1",
                    params![id, account_id, key, snippet, att, omitted, remote],
                )?;
            }
            tx.execute("DELETE FROM bodies WHERE message_id=?1", [id])?;
            tx.execute("DELETE FROM attachment_cache WHERE message_id=?1", [id])?;
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn list_messages(&self, folder_id: i64, offset: u32, limit: u32) -> Result<MessagePage> {
        let db = self.rdb();
        let total: u32 =
            db.query_row("SELECT count(*) FROM entries WHERE folder_id=?1", [folder_id], |r| r.get(0))?;
        let mut stmt = db.prepare_cached(
            "SELECT m.id, m.subject, m.from_name, m.from_addr, m.date, m.snippet, m.flags,
                    m.has_attachments, m.has_body, m.keywords,
                    (SELECT o.failed IS NOT NULL FROM outgoing o WHERE o.id = m.pending_send), m.draft_id
             FROM entries e JOIN messages m ON m.id = e.message_id
             WHERE e.folder_id=?1 ORDER BY e.date DESC, e.message_id DESC LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(params![folder_id, limit, offset], |r| {
            let flags: i64 = r.get(6)?;
            Ok(MessageSummary {
                id: r.get(0)?,
                folder_id,
                subject: r.get(1)?,
                from_name: r.get(2)?,
                from_addr: r.get(3)?,
                date: r.get(4)?,
                snippet: r.get(5)?,
                unread: flags & flags::SEEN == 0,
                flagged: flags & flags::FLAGGED != 0,
                has_attachments: r.get(7)?,
                has_body: r.get(8)?,
                tags: crate::tags::visible(&r.get::<_, String>(9)?),
                answered: flags & flags::ANSWERED != 0,
                forwarded: crate::tags::is_forwarded(&r.get::<_, String>(9)?),
                snoozed_until: crate::tags::snoozed_until(&r.get::<_, String>(9)?),
                sending: r.get::<_, Option<bool>>(10)? == Some(false),
                send_failed: r.get::<_, Option<bool>>(10)? == Some(true),
                draft_id: r.get(11)?,
            })
        })?;
        Ok(MessagePage { total, messages: rows.collect::<rusqlite::Result<_>>()? })
    }

    /// Conversations with at least one message in `folder_id`, newest first.
    pub fn list_threads(&self, folder_id: i64, scope: ThreadScope, offset: u32, limit: u32) -> Result<ThreadPage> {
        let db = self.rdb();
        let total: u32 = db.query_row(
            "SELECT count(DISTINCT thread_id) FROM entries WHERE folder_id=?1",
            [folder_id],
            |r| r.get(0),
        )?;
        let page: Vec<(i64, i64)> = {
            let mut stmt = db.prepare_cached(
                "SELECT thread_id, max(date) AS d FROM entries WHERE folder_id=?1
                 GROUP BY thread_id ORDER BY d DESC, thread_id DESC LIMIT ?2 OFFSET ?3",
            )?;
            stmt.query_map(params![folder_id, limit, offset], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        // The account's own address, to tell our messages from others' (replied markers).
        let me: String = db
            .query_row(
                "SELECT lower(a.email) FROM folders f JOIN accounts a ON a.id = f.account_id WHERE f.id=?1",
                [folder_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default();
        let mut members = db.prepare_cached(&format!(
            "SELECT m.id, m.subject, m.from_name, m.from_addr, m.flags, m.has_attachments, m.snippet, m.message_id,
                    m.keywords, (SELECT o.failed IS NOT NULL FROM outgoing o WHERE o.id = m.pending_send)
             FROM messages m WHERE m.thread_id=?1 AND {SCOPE_FILTER} ORDER BY m.date, m.id"
        ))?;
        let mut threads = Vec::with_capacity(page.len());
        for (thread_id, date) in page {
            let rows: Vec<Member> = members
                .query_map(params![thread_id, folder_id, scope.all_folders, scope.include_trash], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let Some(first) = rows.first() else { continue };
            let latest = rows.last().unwrap();
            let mut participants: Vec<String> = Vec::new();
            for r in rows.iter().rev() {
                let who = if r.2.is_empty() { r.3.clone() } else { r.2.clone() };
                if !who.is_empty() && !participants.contains(&who) {
                    participants.push(who);
                }
            }
            participants.truncate(4);
            let mut seen_ids = std::collections::HashSet::new();
            let count = rows.iter().filter(|r| r.7.as_ref().is_none_or(|m| seen_ids.insert(m.clone()))).count();
            // Replied/forwarded describe the newest message from someone else.
            let mine = |r: &Member| r.3.to_lowercase() == me;
            let incoming = rows.iter().rposition(|r| !mine(r));
            let answered = incoming.is_some_and(|i| rows[i].4 & flags::ANSWERED != 0 || rows[i + 1..].iter().any(mine));
            let forwarded = incoming.is_some_and(|i| crate::tags::is_forwarded(&rows[i].8));
            threads.push(ThreadSummary {
                id: thread_id,
                folder_id,
                subject: first.1.clone(),
                participants,
                message_count: count as u32,
                unread: rows.iter().any(|r| r.4 & flags::SEEN == 0),
                flagged: rows.iter().any(|r| r.4 & flags::FLAGGED != 0),
                has_attachments: rows.iter().any(|r| r.5),
                date,
                snippet: latest.6.clone(),
                latest_message_id: latest.0,
                tags: {
                    let mut t: Vec<String> = Vec::new();
                    for r in &rows {
                        for k in crate::tags::visible(&r.8) {
                            if !t.contains(&k) {
                                t.push(k);
                            }
                        }
                    }
                    t
                },
                answered,
                forwarded,
                sending: latest.9 == Some(false),
                send_failed: latest.9 == Some(true),
                snoozed_until: rows.iter().filter_map(|r| crate::tags::snoozed_until(&r.8)).max(),
            });
        }
        Ok(ThreadPage { total, threads })
    }

    /// A conversation's messages selected by `scope`, oldest first, and how many the scope hid.
    /// Copies sharing a Message-ID appear once. Each message reports `folder_id` when it's in
    /// it, otherwise its most specific other folder (labels before All Mail).
    pub fn thread_messages(&self, thread_id: i64, folder_id: i64, scope: ThreadScope) -> Result<ThreadView> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT m.id,
                    (SELECT e.folder_id FROM entries e JOIN folders f ON f.id = e.folder_id
                     WHERE e.message_id = m.id
                     ORDER BY e.folder_id = ?2 DESC,
                              CASE f.role WHEN 'all' THEN 2 WHEN 'trash' THEN 1 WHEN 'junk' THEN 1 ELSE 0 END
                     LIMIT 1),
                    m.subject, m.from_name, m.from_addr, m.date, m.snippet, m.flags,
                    m.has_attachments, m.has_body, m.message_id,
                    EXISTS (SELECT 1 FROM entries e WHERE e.message_id = m.id AND e.folder_id = ?2),
                    EXISTS (SELECT 1 FROM entries e JOIN folders f ON f.id = e.folder_id
                            WHERE e.message_id = m.id AND coalesce(f.role, '') NOT IN ('trash', 'junk')),
                    EXISTS (SELECT 1 FROM entries e JOIN folders f ON f.id = e.folder_id
                            WHERE e.message_id = m.id AND f.role IN ('trash', 'junk')),
                    m.keywords,
                    (SELECT o.failed IS NOT NULL FROM outgoing o WHERE o.id = m.pending_send), m.draft_id
             FROM messages m WHERE m.thread_id = ?1
             ORDER BY m.date, m.has_body DESC, m.id",
        )?;
        let rows = stmt.query_map(params![thread_id, folder_id], |r| {
            let flags: i64 = r.get(7)?;
            Ok((
                r.get::<_, Option<String>>(10)?,
                (r.get::<_, bool>(11)?, r.get::<_, bool>(12)?, r.get::<_, bool>(13)?),
                MessageSummary {
                    id: r.get(0)?,
                    folder_id: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    subject: r.get(2)?,
                    from_name: r.get(3)?,
                    from_addr: r.get(4)?,
                    date: r.get(5)?,
                    snippet: r.get(6)?,
                    unread: flags & flags::SEEN == 0,
                    flagged: flags & flags::FLAGGED != 0,
                    has_attachments: r.get(8)?,
                    has_body: r.get(9)?,
                    tags: crate::tags::visible(&r.get::<_, String>(14)?),
                    answered: flags & flags::ANSWERED != 0,
                    forwarded: crate::tags::is_forwarded(&r.get::<_, String>(14)?),
                snoozed_until: crate::tags::snoozed_until(&r.get::<_, String>(14)?),
                    sending: r.get::<_, Option<bool>>(15)? == Some(false),
                    send_failed: r.get::<_, Option<bool>>(15)? == Some(true),
                    draft_id: r.get(16)?,
                },
            ))
        })?;
        let mut seen = std::collections::HashSet::new();
        let mut view = ThreadView { messages: Vec::new(), hidden_other: 0, hidden_trash: 0 };
        for row in rows {
            let (mid, (in_folder, in_other, in_trash), summary) = row?;
            if !mid.is_none_or(|m| seen.insert(m)) {
                continue;
            }
            // Same rule as SCOPE_FILTER.
            if in_folder || (scope.all_folders && in_other) || (scope.include_trash && in_trash) {
                view.messages.push(summary);
            } else if in_other {
                view.hidden_other += 1;
            } else if in_trash {
                view.hidden_trash += 1;
            }
        }
        Ok(view)
    }

    /// Quick search over sender, recipients and subject (see `crate::search`), newest first.
    /// `folder_id` restricts to one folder; otherwise the whole account.
    pub fn search(
        &self,
        account_id: i64,
        folder_id: Option<i64>,
        query: &str,
        fields: &[&str],
        offset: u32,
        limit: u32,
    ) -> Result<MessagePage> {
        // `tag:work` / `tag:"to do"` / `tag:$label2` terms filter by keyword; the rest is FTS.
        let (rest, tag_terms) = crate::search::split_tags(query);
        let fts = crate::search::to_fts5(&rest, fields);
        if fts.is_none() && tag_terms.is_empty() {
            return Ok(MessagePage { total: 0, messages: vec![] });
        }
        let keywords: Vec<String> = tag_terms.iter().map(|t| self.tag_keyword(t)).collect::<Result<_>>()?;
        let db = self.rdb();
        let from = if fts.is_some() { "msg_fts JOIN messages m ON m.id = msg_fts.rowid" } else { "messages m" };
        let mut scope = String::from(if fts.is_some() { "msg_fts MATCH ?1" } else { "?1 IS NULL" });
        scope.push_str(
            " AND m.account_id = ?2
             AND (?3 IS NULL OR EXISTS (SELECT 1 FROM entries e WHERE e.message_id = m.id AND e.folder_id = ?3))
             AND EXISTS (SELECT 1 FROM entries e WHERE e.message_id = m.id)",
        );
        for n in 0..keywords.len() {
            scope.push_str(&format!(" AND m.keywords LIKE ?{} ESCAPE '\\'", n + 6));
        }
        let like: Vec<String> = keywords
            .iter()
            .map(|k| format!("% {} %", k.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")))
            .collect();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(fts.clone()), Box::new(account_id), Box::new(folder_id), Box::new(limit), Box::new(offset)];
        for l in &like {
            args.push(Box::new(l.clone()));
        }
        let total: u32 = {
            // Same numbering as the page query; ?4/?5 are unused here but keep positions aligned.
            let sql = format!("SELECT count(*) FROM {from} WHERE {scope} AND ?4 IS NOT NULL AND ?5 IS NOT NULL");
            db.query_row(&sql, rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())), |r| r.get(0))?
        };
        // Report each hit in its most specific folder: Inbox, then labels/folders, then
        // Trash/Spam, and Gmail's All Mail last.
        let mut stmt = db.prepare_cached(&format!(
            "SELECT m.id, coalesce(?3, (SELECT e.folder_id FROM entries e JOIN folders f ON f.id = e.folder_id
                        WHERE e.message_id = m.id
                        ORDER BY CASE f.role WHEN 'inbox' THEN 0 WHEN 'all' THEN 3
                                 WHEN 'trash' THEN 2 WHEN 'junk' THEN 2 ELSE 1 END LIMIT 1), 0),
                    m.subject, m.from_name, m.from_addr, m.date, m.snippet, m.flags,
                    m.has_attachments, m.has_body, m.keywords,
                    (SELECT o.failed IS NOT NULL FROM outgoing o WHERE o.id = m.pending_send), m.draft_id
             FROM {from} WHERE {scope} ORDER BY m.date DESC, m.id DESC LIMIT ?4 OFFSET ?5"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())), |r| {
            let flags: i64 = r.get(7)?;
            Ok(MessageSummary {
                id: r.get(0)?,
                folder_id: r.get(1)?,
                subject: r.get(2)?,
                from_name: r.get(3)?,
                from_addr: r.get(4)?,
                date: r.get(5)?,
                snippet: r.get(6)?,
                unread: flags & flags::SEEN == 0,
                flagged: flags & flags::FLAGGED != 0,
                has_attachments: r.get(8)?,
                has_body: r.get(9)?,
                tags: crate::tags::visible(&r.get::<_, String>(10)?),
                answered: flags & flags::ANSWERED != 0,
                forwarded: crate::tags::is_forwarded(&r.get::<_, String>(10)?),
                snoozed_until: crate::tags::snoozed_until(&r.get::<_, String>(10)?),
                sending: r.get::<_, Option<bool>>(11)? == Some(false),
                send_failed: r.get::<_, Option<bool>>(11)? == Some(true),
                draft_id: r.get(12)?,
            })
        })?;
        Ok(MessagePage { total, messages: rows.collect::<rusqlite::Result<_>>()? })
    }

    /// Position of a message (`threaded` = false) or conversation (`threaded` = true, `id` is a
    /// thread id) in the folder's list, using exactly the list queries' ordering; None if it
    /// isn't in the folder. Lets the UI open a remembered item and scroll to it at any depth.
    pub fn index_of(&self, folder_id: i64, id: i64, threaded: bool) -> Result<Option<u32>> {
        let db = self.rdb();
        if threaded {
            let newest: Option<i64> = db.query_row(
                "SELECT max(date) FROM entries WHERE folder_id=?1 AND thread_id=?2",
                params![folder_id, id],
                |r| r.get(0),
            )?;
            let Some(d) = newest else { return Ok(None) };
            // list_threads: ORDER BY max(date) DESC, thread_id DESC
            Ok(Some(db.query_row(
                "SELECT count(*) FROM (SELECT thread_id, max(date) AS d FROM entries WHERE folder_id=?1
                 GROUP BY thread_id) WHERE d > ?2 OR (d = ?2 AND thread_id > ?3)",
                params![folder_id, d, id],
                |r| r.get(0),
            )?))
        } else {
            let date: Option<i64> = db
                .query_row("SELECT date FROM entries WHERE folder_id=?1 AND message_id=?2", params![folder_id, id], |r| r.get(0))
                .optional()?;
            let Some(d) = date else { return Ok(None) };
            // list_messages: ORDER BY e.date DESC, e.message_id DESC
            Ok(Some(db.query_row(
                "SELECT count(*) FROM entries WHERE folder_id=?1 AND (date > ?2 OR (date = ?2 AND message_id > ?3))",
                params![folder_id, d, id],
                |r| r.get(0),
            )?))
        }
    }

    /// The first unread item (message, or conversation with an unread message here) below `after`
    /// in the list's order, wrapping to the top; with its index. None if nothing is unread.
    pub fn next_unread(&self, folder_id: i64, after: Option<i64>, threaded: bool, back: bool) -> Result<Option<(i64, u32)>> {
        let found: Option<i64> = {
            let db = self.rdb();
            // The unread rows with their list sort key (date DESC, id DESC); `?2`/`?3` = position.
            // Conversations: only those holding unread mail get their newest date computed
            // (~50 ms on a 113k-message folder, vs ~220 ms grouping every conversation).
            let rows = if threaded {
                "SELECT t.thread_id AS id,
                        (SELECT max(x.date) FROM entries x WHERE x.folder_id = ?1 AND x.thread_id = t.thread_id) AS d
                 FROM (SELECT DISTINCT e.thread_id FROM entries e JOIN messages m ON m.id = e.message_id
                       WHERE e.folder_id = ?1 AND (m.flags & 1) = 0) t"
            } else {
                "SELECT e.message_id AS id, e.date AS d
                 FROM entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id = ?1 AND (m.flags & 1) = 0"
            };
            let pos: Option<i64> = match after {
                Some(id) if threaded => db.query_row(
                    "SELECT max(date) FROM entries WHERE folder_id=?1 AND thread_id=?2",
                    params![folder_id, id],
                    |r| r.get(0),
                )?,
                Some(id) => db
                    .query_row("SELECT date FROM entries WHERE folder_id=?1 AND message_id=?2", params![folder_id, id], |r| r.get(0))
                    .optional()?,
                None => None,
            };
            // `back`: the nearest one above instead (wrapping to the bottom).
            let first = |past: bool| -> Result<Option<i64>> {
                let cond = match (past, back) {
                    (false, _) => "",
                    (true, false) => "WHERE d < ?2 OR (d = ?2 AND id < ?3)",
                    (true, true) => "WHERE d > ?2 OR (d = ?2 AND id > ?3)",
                };
                let order = if back { "ASC" } else { "DESC" };
                let sql = format!("SELECT id FROM ({rows}) {cond} ORDER BY d {order}, id {order} LIMIT 1");
                let mut stmt = db.prepare_cached(&sql)?;
                Ok(if past {
                    stmt.query_row(params![folder_id, pos, after], |r| r.get(0)).optional()?
                } else {
                    stmt.query_row(params![folder_id], |r| r.get(0)).optional()?
                })
            };
            match pos {
                Some(_) => match first(true)? {
                    Some(id) => Some(id),
                    None => first(false)?,
                },
                None => first(false)?,
            }
        };
        let Some(id) = found else { return Ok(None) };
        Ok(self.index_of(folder_id, id, threaded)?.map(|i| (id, i)))
    }

    /// Whether message `id` is still in `folder_id`, or (with `None`) still exists at all.
    pub fn message_in_folder(&self, id: i64, folder_id: Option<i64>) -> Result<bool> {
        let db = self.rdb();
        Ok(match folder_id {
            Some(f) => db
                .prepare_cached("SELECT 1 FROM entries WHERE message_id=?1 AND folder_id=?2")?
                .exists(params![id, f])?,
            None => db.prepare_cached("SELECT 1 FROM messages WHERE id=?1")?.exists([id])?,
        })
    }

    pub fn message_ids_for_uids(&self, folder_id: i64, uids: &[u32]) -> Result<Vec<i64>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT message_id FROM entries WHERE folder_id=?1 AND uid=?2")?;
        let mut out = Vec::with_capacity(uids.len());
        for uid in uids {
            if let Some(id) = stmt.query_row(params![folder_id, uid], |r| r.get(0)).optional()? {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Of these messages, the unread ones (in the order given), for new-mail notifications.
    pub fn unread_new_messages(&self, ids: &[i64]) -> Result<Vec<NewMessage>> {
        let db = self.rdb();
        let mut stmt =
            db.prepare_cached("SELECT from_name, from_addr, subject, flags FROM messages WHERE id=?1")?;
        let mut out = Vec::new();
        for &id in ids {
            let row = stmt
                .query_row([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))
                .optional()?;
            if let Some((from_name, from_addr, subject, f)) = row
                && f & flags::SEEN == 0
            {
                out.push(NewMessage { id, from_name, from_addr, subject });
            }
        }
        Ok(out)
    }

    pub fn message_account(&self, id: i64) -> Result<Option<i64>> {
        Ok(self
            .rdb()
            .query_row("SELECT account_id FROM messages WHERE id=?1", [id], |r| r.get(0))
            .optional()?)
    }

    pub fn message_date(&self, id: i64) -> Result<i64> {
        Ok(self.rdb().query_row("SELECT date FROM messages WHERE id=?1", [id], |r| r.get(0))?)
    }

    pub fn folders_of_message(&self, id: i64) -> Result<Vec<i64>> {
        let db = self.db();
        let mut stmt = db.prepare_cached("SELECT folder_id FROM entries WHERE message_id=?1")?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Where to download a body from. Prefers `preferred_folder` (e.g. Gmail's All Mail).
    pub fn body_location(&self, id: i64, preferred_folder: Option<i64>) -> Result<Option<BodyLocation>> {
        Ok(self
            .db()
            .query_row(
                "SELECT e.message_id, e.folder_id, f.raw_name, e.uid, m.size
                 FROM entries e JOIN folders f ON f.id=e.folder_id JOIN messages m ON m.id=e.message_id
                 WHERE e.message_id=?1 AND e.uid IS NOT NULL
                 ORDER BY (e.folder_id = ?2) DESC LIMIT 1",
                params![id, preferred_folder.unwrap_or(-1)],
                |r| {
                    Ok(BodyLocation {
                        message_id: r.get(0)?,
                        folder_id: r.get(1)?,
                        raw_name: r.get(2)?,
                        uid: r.get(3)?,
                        size: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// How the account signs in.
    pub fn set_account_auth(&self, account_id: i64, auth: AuthKind) -> Result<()> {
        self.db().execute("UPDATE accounts SET auth=?2 WHERE id=?1", params![account_id, auth.as_str()])?;
        Ok(())
    }

    /// The account's own SMTP server; `None` falls back to the default for its IMAP server.
    pub fn set_smtp_server(&self, account_id: i64, server: Option<(&str, u16)>) -> Result<()> {
        self.db().execute(
            "UPDATE accounts SET smtp_host=?2, smtp_port=?3 WHERE id=?1",
            params![account_id, server.map(|s| s.0), server.map(|s| s.1)],
        )?;
        Ok(())
    }

    /// Whether `account_id` downloads whole messages, attachments included (offline use).
    pub fn keeps_attachments(&self, account_id: i64) -> Result<bool> {
        Ok(self
            .rdb()
            .query_row("SELECT keep_attachments FROM accounts WHERE id=?1", [account_id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    pub fn set_display_name(&self, account_id: i64, name: &str) -> Result<()> {
        self.db().execute("UPDATE accounts SET display_name=?2 WHERE id=?1", params![account_id, name])?;
        Ok(())
    }

    /// Leave `addr` out of the address suggestions from now on.
    pub fn forget_address(&self, addr: &str) -> Result<()> {
        self.db().execute("INSERT OR IGNORE INTO forgotten_addresses (addr) VALUES (?1)", [addr.trim().to_ascii_lowercase()])?;
        Ok(())
    }

    pub fn forgotten_addresses(&self) -> Result<Vec<String>> {
        let db = self.rdb();
        let mut stmt = db.prepare("SELECT addr FROM forgotten_addresses")?;
        Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// The accounts' order: `ids` first to last (others after them, by id).
    pub fn set_account_order(&self, ids: &[i64]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("UPDATE accounts SET position=NULL", [])?;
        for (i, id) in ids.iter().enumerate() {
            tx.execute("UPDATE accounts SET position=?2 WHERE id=?1", params![id, i as i64])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// An account's marker as the user set it (None: automatic).
    pub fn set_account_mark(&self, account_id: i64, letter: Option<&str>, color: Option<&str>) -> Result<()> {
        self.db().execute(
            "UPDATE accounts SET mark_letter=?2, mark_color=?3 WHERE id=?1",
            params![account_id, letter, color],
        )?;
        Ok(())
    }

    pub fn set_keep_attachments(&self, account_id: i64, keep: bool) -> Result<()> {
        self.db().execute("UPDATE accounts SET keep_attachments=?2 WHERE id=?1", params![account_id, keep])?;
        Ok(())
    }

    /// Messages stored without their attachments, and their size on the server (bytes).
    pub fn attachments_missing(&self, account_id: i64) -> Result<(u64, u64)> {
        Ok(self.rdb().query_row(
            "SELECT count(*), coalesce(sum(size), 0) FROM messages WHERE account_id=?1 AND omitted IS NOT NULL",
            [account_id],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?)
    }

    /// Bodies marked as holding attachments their account doesn't keep (`attachments_stored = 2`)
    /// become the partial copies a download would have stored. No compaction: the space is
    /// reused by new mail.
    fn strip_unwanted_attachments(&self) -> Result<()> {
        let ids: Vec<i64> = {
            let db = self.rdb();
            let mut stmt = db.prepare("SELECT id FROM messages WHERE attachments_stored=2")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for &id in &ids {
            match self.body(id)?.and_then(|raw| crate::partial::strip_local(&raw, id as u32)) {
                Some((stripped, omitted)) => {
                    let (snippet, _) = crate::mime::body_summary(&stripped);
                    self.store_body(id, &stripped, snippet.as_deref(), true, Some(&serde_json::to_string(&omitted)?))?;
                }
                None => {
                    self.db().execute("UPDATE messages SET attachments_stored=0 WHERE id=?1", [id])?;
                }
            }
        }
        if !ids.is_empty() {
            tracing::info!(n = ids.len(), "left out attachments stored by the whole-message fallback");
        }
        Ok(())
    }

    /// Make an account's stored messages attachment-free again (Settings › Accounts › Offline ›
    /// Remove): each whole message with attachments becomes the copy a download without them
    /// would have stored (`partial::strip_local`), its attachments fetched again when opened.
    /// Then the body database is compacted so the space goes back to the disk. Returns the
    /// messages changed and the bytes freed.
    pub fn remove_attachments(&self, account_id: i64) -> Result<(u64, u64)> {
        let ids: Vec<i64> = {
            let db = self.rdb();
            let mut stmt = db.prepare(
                "SELECT id FROM messages WHERE account_id=?1 AND attachments_stored=1",
            )?;
            stmt.query_map([account_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut changed = 0u64;
        for id in ids {
            let Some(raw) = self.body(id)? else { continue };
            let Some((stripped, omitted)) = crate::partial::strip_local(&raw, id as u32) else { continue };
            let (snippet, _) = crate::mime::body_summary(&stripped);
            self.store_body(id, &stripped, snippet.as_deref(), true, Some(&serde_json::to_string(&omitted)?))?;
            changed += 1;
        }
        let file = self.dir.join("bodies.db");
        let size = || std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) + std::fs::metadata(self.dir.join("bodies.db-wal")).map(|m| m.len()).unwrap_or(0);
        let before = size();
        if changed > 0 {
            let bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
            // VACUUM builds a whole temporary copy: on disk, not in memory (temp_store is MEMORY
            // for everything else).
            bodies.execute_batch("PRAGMA temp_store=FILE; VACUUM; PRAGMA temp_store=MEMORY; PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        Ok((changed, before.saturating_sub(size())))
    }

    /// Per account: messages with attachments still to download (stored without them) and
    /// those stored with them, with their sizes on the server. One pass over the messages.
    pub fn attachment_counts(&self) -> Result<Vec<AttachmentCounts>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT account_id,
                    coalesce(sum(omitted IS NOT NULL), 0),
                    coalesce(sum(CASE WHEN omitted IS NOT NULL THEN size END), 0),
                    coalesce(sum(attachments_stored = 1), 0),
                    coalesce(sum(CASE WHEN attachments_stored = 1 THEN size END), 0)
             FROM messages GROUP BY account_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AttachmentCounts {
                account_id: r.get(0)?,
                missing: r.get::<_, i64>(1)? as u64,
                missing_bytes: r.get::<_, i64>(2)? as u64,
                kept: r.get::<_, i64>(3)? as u64,
                kept_bytes: r.get::<_, i64>(4)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Newest messages still missing a body, preferring `focus_folder`; then, for an account
    /// that keeps attachments, the newest stored without them.
    pub fn messages_missing_body(
        &self,
        account_id: i64,
        focus_folder: Option<i64>,
        limit: u32,
    ) -> Result<Vec<i64>> {
        let db = self.db();
        let mut out = Vec::new();
        if let Some(fid) = focus_folder {
            let mut stmt = db.prepare_cached(
                "SELECT m.id FROM entries e JOIN messages m ON m.id=e.message_id
                 WHERE e.folder_id=?1 AND m.has_body=0 ORDER BY e.date DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![fid, limit], |r| r.get(0))?;
            out = rows.collect::<rusqlite::Result<Vec<i64>>>()?;
        }
        if out.len() < limit as usize {
            let mut stmt = db.prepare_cached(
                "SELECT id FROM messages WHERE account_id=?1 AND has_body=0 ORDER BY date DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![account_id, limit], |r| r.get::<_, i64>(0))?;
            for id in rows {
                let id = id?;
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        let keep: bool = db
            .query_row("SELECT keep_attachments FROM accounts WHERE id=?1", [account_id], |r| r.get(0))
            .optional()?
            .unwrap_or(false);
        if keep && out.len() < limit as usize {
            let mut stmt = db.prepare_cached(
                "SELECT id FROM messages WHERE account_id=?1 AND has_body=1 AND omitted IS NOT NULL
                 ORDER BY date DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![account_id, limit - out.len() as u32], |r| r.get::<_, i64>(0))?;
            for id in rows {
                out.push(id?);
            }
        }
        Ok(out)
    }

    /// (messages, messages with everything they should have downloaded): a body, and for an
    /// account that keeps attachments, the attachments too. From indexes only (it's polled for
    /// progress): a scan of the messages took 0.18 s on a 113k-message account.
    pub fn count_missing_bodies(&self, account_id: i64) -> Result<(u64, u64)> {
        let db = self.rdb();
        let count = |sql: &str| -> Result<u64> { Ok(db.prepare_cached(sql)?.query_row([account_id], |r| r.get::<_, i64>(0))? as u64) };
        let total = count("SELECT count(*) FROM messages WHERE account_id=?1")?;
        let no_body = count("SELECT count(*) FROM messages WHERE account_id=?1 AND has_body=0")?;
        let keep = count("SELECT keep_attachments FROM accounts WHERE id=?1")? != 0;
        let partial = if keep { count("SELECT count(*) FROM messages WHERE account_id=?1 AND omitted IS NOT NULL")? } else { 0 };
        Ok((total, total.saturating_sub(no_body + partial)))
    }

    pub fn store_body(
        &self,
        id: i64,
        raw: &[u8],
        snippet: Option<&str>,
        has_attachments: bool,
        omitted: Option<&str>,
    ) -> Result<()> {
        {
            let bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
            bodies.execute(
                "INSERT OR REPLACE INTO bodies (message_id, raw) VALUES (?1, ?2)",
                params![id, raw],
            )?;
            // A whole message holds its attachments: separately fetched copies are spares now.
            if omitted.is_none() {
                bodies.execute("DELETE FROM attachment_cache WHERE message_id=?1", [id])?;
            }
        }
        // Whether Remove would have something to take (true attachments, not text parts or
        // inline pictures): parsed only for whole bodies the parser says have attachments.
        let stored = omitted.is_none() && has_attachments && crate::partial::has_omittable(raw);
        self.db().execute(
            "UPDATE messages SET has_body=1, snippet=?2, has_attachments=?3, omitted=?4, attachments_stored=?5 WHERE id=?1",
            params![id, snippet, has_attachments, omitted, stored],
        )?;
        Ok(())
    }

    /// Fill in `attachments_stored` for bodies stored before it was recorded (once, after the
    /// upgrade that added it): only whole bodies with attachments need a look. In an account
    /// that doesn't keep attachments they came by the whole-message fallback: marked to go.
    fn mark_stored_attachments(&self) -> Result<()> {
        let ids: Vec<(i64, bool)> = {
            let db = self.db();
            db.execute(
                "UPDATE messages SET attachments_stored=0
                 WHERE attachments_stored IS NULL AND NOT (has_body=1 AND omitted IS NULL AND has_attachments=1)",
                [],
            )?;
            let mut stmt = db.prepare(
                "SELECT m.id, a.keep_attachments FROM messages m JOIN accounts a ON a.id = m.account_id
                 WHERE m.attachments_stored IS NULL",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<(i64, bool)>>>()?
        };
        if ids.is_empty() {
            return Ok(());
        }
        let started = std::time::Instant::now();
        for &(id, keep) in &ids {
            let stored = self.body(id)?.is_some_and(|raw| crate::partial::has_omittable(&raw));
            let flag = match (stored, keep) {
                (false, _) => 0,
                (true, true) => 1,
                (true, false) => 2,
            };
            self.db().execute("UPDATE messages SET attachments_stored=?2 WHERE id=?1", params![id, flag])?;
        }
        tracing::info!(n = ids.len(), ms = started.elapsed().as_millis() as u64, "looked for stored attachments");
        Ok(())
    }

    pub fn allow_remote(&self, id: i64) -> Result<bool> {
        Ok(self
            .rdb()
            .query_row("SELECT allow_remote FROM messages WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    pub fn set_allow_remote(&self, id: i64, allow: bool) -> Result<()> {
        self.db().execute("UPDATE messages SET allow_remote=?2 WHERE id=?1", params![id, allow])?;
        Ok(())
    }

    pub fn cached_attachment(&self, id: i64, idx: usize) -> Result<Option<Vec<u8>>> {
        let bodies = self.bodies_read.lock().unwrap_or_else(|e| e.into_inner());
        Ok(bodies
            .query_row("SELECT data FROM attachment_cache WHERE message_id=?1 AND idx=?2", params![id, idx as i64], |r| r.get(0))
            .optional()?)
    }

    pub fn cached_attachment_indices(&self, id: i64) -> Result<Vec<usize>> {
        let bodies = self.bodies_read.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = bodies.prepare_cached("SELECT idx FROM attachment_cache WHERE message_id=?1")?;
        let rows = stmt.query_map([id], |r| r.get::<_, i64>(0).map(|i| i as usize))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn cache_attachment(&self, id: i64, idx: usize, data: &[u8]) -> Result<()> {
        let bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        bodies.execute(
            "INSERT OR REPLACE INTO attachment_cache (message_id, idx, data) VALUES (?1, ?2, ?3)",
            params![id, idx as i64, data],
        )?;
        Ok(())
    }

    /// Attachments left on the server when the body was downloaded (JSON), if any.
    pub fn omitted(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .rdb()
            .query_row("SELECT omitted FROM messages WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .flatten())
    }

    pub fn body(&self, id: i64) -> Result<Option<Vec<u8>>> {
        let bodies = self.bodies_read.lock().unwrap_or_else(|e| e.into_inner());
        Ok(bodies
            .query_row("SELECT raw FROM bodies WHERE message_id=?1", [id], |r| r.get(0))
            .optional()?)
    }

    /// Set or clear flag bits locally, adjusting cached unread counts for SEEN changes.
    /// Returns the folders containing the message, or None if nothing changed.
    pub fn set_flag(&self, id: i64, bit: i64, on: bool) -> Result<Option<Vec<i64>>> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let old: i64 = tx.query_row("SELECT flags FROM messages WHERE id=?1", [id], |r| r.get(0))?;
        let new = if on { old | bit } else { old & !bit };
        if new == old {
            return Ok(None);
        }
        tx.execute("UPDATE messages SET flags=?2 WHERE id=?1", params![id, new])?;
        let folders: Vec<i64> = {
            let mut stmt = tx.prepare_cached("SELECT folder_id FROM entries WHERE message_id=?1")?;
            stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        if bit == flags::SEEN {
            let delta = if on { -1 } else { 1 };
            for f in &folders {
                tx.execute("UPDATE folders SET unread=max(0, unread+?2) WHERE id=?1", params![f, delta])?;
            }
        }
        tx.commit()?;
        Ok(Some(folders))
    }

    // ---- user actions -----------------------------------------------------------------------

    /// Folder membership and flags of these messages, for undo.
    pub fn snapshot(&self, ids: &[i64]) -> Result<Snapshot> {
        let db = self.rdb();
        let mut snap = Snapshot::default();
        let mut ent = db.prepare_cached("SELECT folder_id, uid, date FROM entries WHERE message_id=?1")?;
        let mut fl = db.prepare_cached("SELECT flags, keywords FROM messages WHERE id=?1")?;
        for &id in ids {
            for row in ent.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<u32>>(1)?, r.get::<_, i64>(2)?)))? {
                let (folder_id, uid, date) = row?;
                snap.entries.push(SnapEntry { folder_id, message_id: id, uid, date });
            }
            if let Some((f, k)) = fl.query_row([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).optional()? {
                snap.flags.push((id, f));
                snap.keywords.push((id, k));
            }
        }
        Ok(snap)
    }

    /// Apply an action's local effect: drop/add folder entries and set flags, keeping folder
    /// counts in step. Returns the folders whose contents changed.
    pub fn apply_local(&self, change: &LocalChange) -> Result<Vec<i64>> {
        let ids: Vec<i64> = change.message_ids.clone();
        self.mutate_counted(&ids, |tx| {
            let mut del = tx.prepare_cached("DELETE FROM entries WHERE folder_id=?1 AND message_id=?2")?;
            for (f, m) in &change.remove {
                del.execute(params![f, m])?;
            }
            let mut add = tx.prepare_cached(
                "INSERT INTO entries (folder_id, message_id, uid, date)
                 SELECT ?1, ?2, NULL, date FROM messages WHERE id=?2
                 ON CONFLICT(folder_id, message_id) DO NOTHING",
            )?;
            for (f, m) in &change.add {
                add.execute(params![f, m])?;
            }
            let mut set = tx.prepare_cached("UPDATE messages SET flags=(flags | ?2) & ~?3 WHERE id=?1")?;
            for (m, on, off) in &change.flags {
                set.execute(params![m, on, off])?;
            }
            let mut get = tx.prepare_cached("SELECT keywords FROM messages WHERE id=?1")?;
            let mut put = tx.prepare_cached("UPDATE messages SET keywords=?2 WHERE id=?1")?;
            for (m, keyword, add) in &change.tags {
                let Some(cur): Option<String> = get.query_row([m], |r| r.get(0)).optional()? else { continue };
                let mut ks = crate::tags::parse(&cur);
                let k = crate::tags::normalize(keyword);
                ks.retain(|x| *x != k);
                if *add {
                    ks.push(k);
                }
                put.execute(params![m, crate::tags::join(&ks)])?;
            }
            Ok(())
        })
    }

    /// Put messages back exactly as snapshotted (undo, or a failed action).
    pub fn restore(&self, snap: &Snapshot) -> Result<Vec<i64>> {
        let ids: Vec<i64> = snap.flags.iter().map(|(m, _)| *m).collect();
        self.mutate_counted(&ids, |tx| {
            let mut clear = tx.prepare_cached("DELETE FROM entries WHERE message_id=?1")?;
            for m in &ids {
                clear.execute([m])?;
            }
            let mut ins = tx.prepare_cached(
                "INSERT INTO entries (folder_id, message_id, uid, date) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(folder_id, message_id) DO UPDATE SET uid=excluded.uid",
            )?;
            for e in &snap.entries {
                ins.execute(params![e.folder_id, e.message_id, e.uid, e.date])?;
            }
            let mut set = tx.prepare_cached("UPDATE messages SET flags=?2 WHERE id=?1")?;
            for (m, f) in &snap.flags {
                set.execute(params![m, f])?;
            }
            let mut kw = tx.prepare_cached("UPDATE messages SET keywords=?2 WHERE id=?1")?;
            for (m, k) in &snap.keywords {
                kw.execute(params![m, k])?;
            }
            Ok(())
        })
    }

    /// Run `f` on these messages and adjust the cached total/unread of every folder whose
    /// membership or unread state changed, so the sidebar updates before the next STATUS.
    fn mutate_counted(
        &self,
        ids: &[i64],
        f: impl FnOnce(&rusqlite::Transaction) -> Result<()>,
    ) -> Result<Vec<i64>> {
        type Counts = std::collections::HashMap<i64, (i64, i64)>;
        fn counts(tx: &rusqlite::Transaction, ids: &[i64]) -> Result<Counts> {
            let mut out = Counts::new();
            let mut stmt = tx.prepare_cached(
                "SELECT e.folder_id, (m.flags & 1) = 0 FROM entries e JOIN messages m ON m.id=e.message_id
                 WHERE e.message_id=?1",
            )?;
            for id in ids {
                for row in stmt.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?)))? {
                    let (folder, unread) = row?;
                    let c = out.entry(folder).or_default();
                    c.0 += 1;
                    c.1 += unread as i64;
                }
            }
            Ok(out)
        }
        let mut db = self.db();
        let tx = db.transaction()?;
        let before = counts(&tx, ids)?;
        f(&tx)?;
        let after = counts(&tx, ids)?;
        let mut changed = Vec::new();
        {
            let mut adj = tx.prepare_cached(
                "UPDATE folders SET total=max(0, total+?2), unread=max(0, unread+?3) WHERE id=?1",
            )?;
            let folders: std::collections::BTreeSet<i64> = before.keys().chain(after.keys()).copied().collect();
            for folder in folders {
                let (bt, bu) = before.get(&folder).copied().unwrap_or_default();
                let (at, au) = after.get(&folder).copied().unwrap_or_default();
                if (bt, bu) != (at, au) {
                    adj.execute(params![folder, at - bt, au - bu])?;
                    changed.push(folder);
                }
            }
        }
        tx.commit()?;
        // Still under the writer lock: a sync write either landed before this or sees the bump.
        self.local_epoch.fetch_add(1, Ordering::SeqCst);
        Ok(changed)
    }

    pub fn enqueue_ops(&self, account_id: i64, ops: &str, snapshot: &str, folders: &str, not_before_ms: i64) -> Result<i64> {
        let db = self.db();
        db.execute(
            "INSERT INTO pending_ops (account_id, ops, snapshot, folders, not_before) VALUES (?1,?2,?3,?4,?5)",
            params![account_id, ops, snapshot, folders, not_before_ms],
        )?;
        // Also bumped here: a step that started between the local change and this (seeing no
        // pending op yet) read the server before the action reached it.
        self.local_epoch.fetch_add(1, Ordering::SeqCst);
        Ok(db.last_insert_rowid())
    }

    /// Cancel an op that hasn't been sent yet; returns its snapshot JSON.
    pub fn cancel_op(&self, op_id: i64) -> Result<Option<String>> {
        let db = self.db();
        let snap: Option<String> = db
            .query_row("SELECT snapshot FROM pending_ops WHERE id=?1 AND started=0", [op_id], |r| r.get(0))
            .optional()?;
        if snap.is_some() {
            db.execute("DELETE FROM pending_ops WHERE id=?1", [op_id])?;
        }
        Ok(snap)
    }

    /// The next due op for an account, marked started (no longer cancellable).
    pub fn claim_due_op(&self, account_id: i64, now_ms: i64) -> Result<Option<PendingOp>> {
        let db = self.db();
        let row: Option<PendingOp> = db
            .query_row(
                "SELECT id, ops, snapshot, folders, attempts FROM pending_ops
                 WHERE account_id=?1 AND not_before<=?2 ORDER BY id LIMIT 1",
                params![account_id, now_ms],
                |r| Ok(PendingOp { id: r.get(0)?, ops: r.get(1)?, snapshot: r.get(2)?, folders: r.get(3)?, attempts: r.get(4)? }),
            )
            .optional()?;
        if let Some(op) = &row {
            db.execute("UPDATE pending_ops SET started=1 WHERE id=?1", [op.id])?;
        }
        Ok(row)
    }

    /// Record progress: the commands still to send.
    pub fn update_op(&self, op_id: i64, ops: &str) -> Result<()> {
        self.db().execute("UPDATE pending_ops SET ops=?2 WHERE id=?1", params![op_id, ops])?;
        Ok(())
    }

    pub fn finish_op(&self, op_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM pending_ops WHERE id=?1", [op_id])?;
        Ok(())
    }

    /// Put an op back for a later retry.
    pub fn retry_op(&self, op_id: i64, not_before_ms: i64) -> Result<()> {
        self.db().execute(
            "UPDATE pending_ops SET started=0, attempts=attempts+1, not_before=?2 WHERE id=?1",
            params![op_id, not_before_ms],
        )?;
        Ok(())
    }

    /// When the last queued op for the account is due (None: nothing queued).
    pub fn last_op_due(&self, account_id: i64) -> Result<Option<i64>> {
        Ok(self.rdb().query_row("SELECT max(not_before) FROM pending_ops WHERE account_id=?1", [account_id], |r| r.get(0))?)
    }

    pub fn next_op_due(&self, account_id: i64) -> Result<Option<i64>> {
        Ok(self.rdb().query_row("SELECT min(not_before) FROM pending_ops WHERE account_id=?1", [account_id], |r| r.get(0))?)
    }

    pub fn has_pending_ops(&self, account_id: i64) -> Result<bool> {
        Ok(self.rdb().prepare_cached("SELECT 1 FROM pending_ops WHERE account_id=?1")?.exists([account_id])?)
    }

    /// Ops interrupted by a crash or quit mid-send are retried, not lost.
    pub fn reset_started_ops(&self) -> Result<()> {
        self.db().execute("UPDATE pending_ops SET started=0 WHERE started=1", [])?;
        // A send interrupted mid-SMTP is tried again: SMTP has no "did it arrive?", so a rare
        // duplicate beats a lost message.
        self.db().execute("UPDATE outgoing SET started=0 WHERE started=1", [])?;
        Ok(())
    }

    // ---- outgoing mail ---------------------------------------------------------------------

    /// Queue a built message to be sent at `not_before_ms` (after the undo window).
    pub fn enqueue_outgoing(&self, q: &NewOutgoing) -> Result<i64> {
        let db = self.db();
        db.execute(
            "INSERT INTO outgoing (account_id, raw, envelope_from, recipients, not_before, replied, forwarded, draft)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                q.account_id,
                q.raw,
                q.envelope_from,
                serde_json::to_string(&q.recipients)?,
                q.not_before_ms,
                q.replied,
                q.forwarded,
                q.draft
            ],
        )?;
        Ok(db.last_insert_rowid())
    }

    /// The built message of a queue entry.
    pub fn outgoing_raw(&self, id: i64) -> Result<Option<Vec<u8>>> {
        Ok(self.rdb().query_row("SELECT raw FROM outgoing WHERE id=?1", [id], |r| r.get(0)).optional()?)
    }

    /// Take back a message that hasn't started sending (undo): returns its draft state, or None
    /// if it's already on its way (or gone).
    pub fn cancel_outgoing(&self, id: i64) -> Result<Option<String>> {
        let db = self.db();
        let draft: Option<Option<String>> = db
            .query_row("SELECT draft FROM outgoing WHERE id=?1 AND started=0", [id], |r| r.get(0))
            .optional()?;
        let Some(draft) = draft else { return Ok(None) };
        db.execute("DELETE FROM outgoing WHERE id=?1 AND started=0", [id])?;
        Ok(Some(draft.unwrap_or_default()))
    }

    /// The next message due to be sent (not failed), marked as started.
    pub fn claim_due_outgoing(&self, account_id: i64, now_ms: i64) -> Result<Option<OutgoingRow>> {
        let db = self.db();
        let row: Option<OutgoingRow> = db
            .query_row(
                "SELECT id, raw, envelope_from, recipients, attempts, replied, forwarded FROM outgoing
                 WHERE account_id=?1 AND not_before<=?2 AND failed IS NULL AND started=0 ORDER BY id LIMIT 1",
                params![account_id, now_ms],
                |r| {
                    Ok(OutgoingRow {
                        id: r.get(0)?,
                        raw: r.get(1)?,
                        envelope_from: r.get(2)?,
                        recipients: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                        attempts: r.get(4)?,
                        replied: r.get(5)?,
                        forwarded: r.get(6)?,
                    })
                },
            )
            .optional()?;
        if let Some(o) = &row {
            db.execute("UPDATE outgoing SET started=1 WHERE id=?1", [o.id])?;
        }
        Ok(row)
    }

    /// Sent: gone from the queue.
    pub fn finish_outgoing(&self, id: i64) -> Result<()> {
        self.db().execute("DELETE FROM outgoing WHERE id=?1", [id])?;
        Ok(())
    }

    /// Couldn't send this time: try again at `at_ms`.
    pub fn retry_outgoing(&self, id: i64, at_ms: i64) -> Result<()> {
        self.db()
            .execute("UPDATE outgoing SET started=0, attempts=attempts+1, not_before=?2 WHERE id=?1", params![id, at_ms])?;
        Ok(())
    }

    /// The server refused it: kept (with its draft) for the user to fix or retry.
    pub fn fail_outgoing(&self, id: i64, message: &str) -> Result<()> {
        self.db().execute("UPDATE outgoing SET started=0, failed=?2 WHERE id=?1", params![id, message])?;
        Ok(())
    }

    /// When the next waiting message is due (ms), if any.
    pub fn next_outgoing_due(&self, account_id: i64) -> Result<Option<i64>> {
        Ok(self.rdb().query_row(
            "SELECT min(not_before) FROM outgoing WHERE account_id=?1 AND failed IS NULL AND started=0",
            [account_id],
            |r| r.get(0),
        )?)
    }

    /// Drop messages that are in no folder any more, with their body.
    pub fn purge_orphans(&self, ids: &[i64]) -> Result<()> {
        let mut gone = Vec::new();
        {
            let db = self.db();
            let mut has = db.prepare_cached("SELECT 1 FROM entries WHERE message_id=?1")?;
            let mut del = db.prepare_cached("DELETE FROM messages WHERE id=?1")?;
            for id in ids {
                if !has.exists([id])? {
                    del.execute([id])?;
                    gone.push(*id);
                }
            }
        }
        let bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        for id in gone {
            bodies.execute("DELETE FROM bodies WHERE message_id=?1", [id])?;
            bodies.execute("DELETE FROM attachment_cache WHERE message_id=?1", [id])?;
        }
        Ok(())
    }

    /// (folder raw name, uid) for every copy of a message the server knows a UID for.
    pub fn message_copies(&self, id: i64) -> Result<Vec<(i64, String, u32)>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT f.id, f.raw_name, e.uid FROM entries e JOIN folders f ON f.id=e.folder_id
             WHERE e.message_id=?1 AND e.uid IS NOT NULL",
        )?;
        let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Show a message we're sending at once, in `folder_id` (Sent) and its conversation, before
    /// the server has it: a local copy, linked to its queue entry. The server's copy takes the
    /// row over when it syncs (see `upsert_headers_since`). Returns the message id.
    pub fn insert_local_copy(&self, account_id: i64, folder_id: i64, raw: &[u8], outgoing_id: i64) -> Result<i64> {
        self.insert_local(account_id, folder_id, raw, Some(outgoing_id), None)
    }

    fn insert_local(
        &self,
        account_id: i64,
        folder_id: i64,
        raw: &[u8],
        outgoing_id: Option<i64>,
        draft_id: Option<i64>,
    ) -> Result<i64> {
        let parsed = crate::mime::parse_header_block(raw);
        let date = parsed.date.unwrap_or_else(|| chrono::Utc::now().timestamp());
        let rec = HeaderRecord {
            original_message_id: None,
            uid: 0,
            gm_msgid: None,
            email_id: None,
            gm_labels: None,
            keywords: vec![],
            message_id: parsed.message_id,
            in_reply_to: parsed.in_reply_to,
            refs: parsed.refs,
            subject: parsed.subject,
            from_name: parsed.from_name,
            from_addr: parsed.from_addr,
            to_addrs: parsed.to_addrs,
            date,
            size: raw.len() as u32,
            flags: flags::SEEN | if draft_id.is_some() { flags::DRAFT } else { 0 },
            has_attachments: parsed.likely_attachments,
        };
        let id = {
            let mut db = self.db();
            let tx = db.transaction()?;
            tx.execute(
                "INSERT INTO messages (account_id, message_id, in_reply_to, refs, subject, from_name, from_addr,
                    to_addrs, date, size, flags, has_attachments, identity, keywords, local_copy, pending_send, draft_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,'',1,?14,?15)",
                params![
                    account_id,
                    rec.message_id,
                    rec.in_reply_to,
                    rec.refs,
                    rec.subject,
                    rec.from_name,
                    rec.from_addr,
                    rec.to_addrs,
                    rec.date,
                    rec.size,
                    rec.flags,
                    rec.has_attachments,
                    message_identity(&rec),
                    outgoing_id,
                    draft_id
                ],
            )?;
            let id = tx.last_insert_rowid();
            assign_thread(&tx, account_id, id, rec.message_id.as_deref(), rec.in_reply_to.as_deref(), rec.refs.as_deref())?;
            tx.execute("INSERT INTO entries (folder_id, message_id, uid, date) VALUES (?1, ?2, NULL, ?3)", params![folder_id, id, date])?;
            tx.execute("UPDATE folders SET total=total+1 WHERE id=?1", [folder_id])?;
            tx.commit()?;
            id
        };
        let (snippet, has_attachments) = crate::mime::body_summary(raw);
        self.store_body(id, raw, snippet.as_deref(), has_attachments, None)?;
        Ok(id)
    }

    // ---- drafts ------------------------------------------------------------------------------

    pub fn create_draft(&self, account_id: i64, state: &str, message_id: &str) -> Result<i64> {
        let db = self.db();
        db.execute("INSERT INTO drafts (account_id, state, message_id) VALUES (?1, ?2, ?3)", params![account_id, state, message_id])?;
        Ok(db.last_insert_rowid())
    }

    /// (account, state, Message-ID) of a draft.
    pub fn draft(&self, id: i64) -> Result<Option<(i64, String, String)>> {
        Ok(self
            .rdb()
            .query_row("SELECT account_id, state, message_id FROM drafts WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?)
    }

    pub fn update_draft(&self, id: i64, account_id: i64, state: &str) -> Result<()> {
        self.db().execute(
            "UPDATE drafts SET account_id=?2, state=?3, updated=unixepoch() WHERE id=?1",
            params![id, account_id, state],
        )?;
        Ok(())
    }

    /// The draft's copy in `folder_id` (Drafts): updated in place, so its row doesn't flicker
    /// while being written; created the first time. Returns the folders it's in.
    pub fn upsert_draft_copy(&self, draft_id: i64, account_id: i64, folder_id: i64, raw: &[u8]) -> Result<Vec<i64>> {
        let existing: Option<i64> =
            self.rdb().query_row("SELECT id FROM messages WHERE draft_id=?1", [draft_id], |r| r.get(0)).optional()?;
        let id = match existing {
            None => self.insert_local(account_id, folder_id, raw, None, Some(draft_id))?,
            Some(id) => {
                let p = crate::mime::parse_header_block(raw);
                let date = p.date.unwrap_or_else(|| chrono::Utc::now().timestamp());
                {
                    let db = self.db();
                    db.execute(
                        "UPDATE messages SET subject=?2, from_name=?3, from_addr=?4, to_addrs=?5, date=?6, size=?7 WHERE id=?1",
                        params![id, p.subject, p.from_name, p.from_addr, p.to_addrs, date, raw.len() as u32],
                    )?;
                    db.execute("UPDATE entries SET date=?2 WHERE message_id=?1", params![id, date])?;
                }
                let (snippet, has_attachments) = crate::mime::body_summary(raw);
                self.store_body(id, raw, snippet.as_deref(), has_attachments, None)?;
                id
            }
        };
        self.message_folder_ids(id)
    }

    /// Discard (or sent): the draft and its copy in Drafts go. Returns the folders it was in.
    pub fn delete_draft(&self, draft_id: i64) -> Result<Vec<i64>> {
        let folders = self.delete_draft_copy(draft_id)?;
        self.db().execute("DELETE FROM drafts WHERE id=?1", [draft_id])?;
        Ok(folders)
    }

    /// Just the copy in Drafts (an empty draft isn't shown). Returns the folders it was in.
    pub fn delete_draft_copy(&self, draft_id: i64) -> Result<Vec<i64>> {
        let copy: Option<i64> =
            self.rdb().query_row("SELECT id FROM messages WHERE draft_id=?1", [draft_id], |r| r.get(0)).optional()?;
        let mut folders = Vec::new();
        if let Some(id) = copy {
            folders = self.message_folder_ids(id)?;
            {
                let mut db = self.db();
                let tx = db.transaction()?;
                for f in &folders {
                    tx.execute("UPDATE folders SET total=max(0, total-1) WHERE id=?1", [f])?;
                }
                tx.execute("DELETE FROM entries WHERE message_id=?1", [id])?;
                tx.execute("DELETE FROM messages WHERE id=?1", [id])?;
                tx.commit()?;
            }
            self.bodies.lock().unwrap_or_else(|e| e.into_inner()).execute("DELETE FROM bodies WHERE message_id=?1", [id])?;
        }
        Ok(folders)
    }

    /// (from name, from address, To+Cc, date) of every message: the input for address
    /// suggestions (`contacts::build`).
    pub fn contact_rows(&self) -> Result<Vec<(String, String, String, i64)>> {
        let db = self.rdb();
        let mut stmt = db.prepare("SELECT from_name, from_addr, to_addrs, date FROM messages")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The conversation a message belongs to.
    pub fn message_thread_id(&self, id: i64) -> Result<Option<i64>> {
        Ok(self.rdb().query_row("SELECT thread_id FROM messages WHERE id=?1", [id], |r| r.get(0)).optional()?.flatten())
    }

    /// The local copy of queue entry `outgoing_id`, if it's still local.
    pub fn local_copy_of(&self, outgoing_id: i64) -> Result<Option<i64>> {
        Ok(self
            .rdb()
            .query_row("SELECT id FROM messages WHERE pending_send=?1 AND local_copy=1", [outgoing_id], |r| r.get(0))
            .optional()?)
    }

    /// Sent: the local copy stays (now a plain message) until the server's copy takes it over.
    pub fn local_copy_sent(&self, outgoing_id: i64) -> Result<()> {
        self.db().execute("UPDATE messages SET pending_send=NULL WHERE pending_send=?1", [outgoing_id])?;
        Ok(())
    }

    /// A safety net behind the takeover by Message-ID (`upsert_headers_since`): a local copy of
    /// sent mail still there a day later, while its folder holds the server's copy of the same
    /// message (subject, sender, the same recipients, sent within 10 minutes), is dropped. A
    /// server that changed the Message-ID in a way we don't recognise would otherwise leave it
    /// beside the real one for good; one that never filed it keeps it (the only record).
    /// `account`: one account (None: all). Returns the folders that changed.
    pub fn drop_matched_local_copies(&self, account: Option<i64>) -> Result<Vec<i64>> {
        let day_ago = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64 - 86_400;
        // Two index lookups instead of one join (left to the planner, that took minutes on a
        // large mailbox): the few old local copies, then each one's folder around its date.
        let pairs: Vec<(i64, String, String)> = {
            let db = self.rdb();
            let locals: Vec<(i64, i64, i64, String, String, String)> = db
                .prepare_cached(
                    "SELECT l.id, e.folder_id, e.date, l.subject, lower(l.from_addr), l.to_addrs
                     FROM messages l INDEXED BY messages_local JOIN entries e ON e.message_id = l.id
                     WHERE l.local_copy = 1 AND l.pending_send IS NULL AND l.draft_id IS NULL AND l.date < ?1
                       AND (?2 IS NULL OR l.account_id = ?2)",
                )?
                .query_map(params![day_ago, account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut near = db.prepare_cached(
                "SELECT m.to_addrs FROM entries e INDEXED BY entries_list JOIN messages m ON m.id = e.message_id
                 WHERE e.folder_id = ?1 AND e.date BETWEEN ?2 - 600 AND ?2 + 600 AND e.uid IS NOT NULL
                   AND m.local_copy = 0 AND m.subject = ?3 AND lower(m.from_addr) = ?4",
            )?;
            let mut out = Vec::new();
            for (id, folder, date, subject, from, to) in locals {
                for theirs in near.query_map(params![folder, date, subject, from], |r| r.get::<_, String>(0))? {
                    out.push((id, to.clone(), theirs?));
                }
            }
            out
        };
        let addrs = |list: &str| {
            let mut v: Vec<String> = crate::contacts::parse_list(list).into_iter().map(|a| a.addr.to_ascii_lowercase()).collect();
            v.sort();
            v
        };
        let mut drop: Vec<i64> = pairs.iter().filter(|(_, l, s)| addrs(l) == addrs(s)).map(|p| p.0).collect();
        drop.sort_unstable();
        drop.dedup();
        let mut changed = Vec::new();
        for id in drop {
            let folders = self.message_folder_ids(id)?;
            {
                let mut db = self.db();
                let tx = db.transaction()?;
                tx.execute("DELETE FROM entries WHERE message_id=?1", [id])?;
                tx.execute("DELETE FROM messages WHERE id=?1 AND local_copy=1", [id])?;
                tx.commit()?;
            }
            self.bodies.lock().unwrap_or_else(|e| e.into_inner()).execute("DELETE FROM bodies WHERE message_id=?1", [id])?;
            tracing::info!(id, "dropped a local copy of sent mail: the server's copy is there");
            changed.extend(folders);
        }
        changed.sort_unstable();
        changed.dedup();
        Ok(changed)
    }

    /// Copies moved into a folder locally whose server copy arrived as a message of its own
    /// (before stand-ins were matched by Message-ID alone; see `upsert_headers_since`): the
    /// stand-in goes, the server's copy stays. Not while the account has actions queued, whose
    /// copies are still on their way. Gmail's copies are matched exactly and never left over.
    /// `account`: one account (None: all). Returns the folders that changed.
    pub fn drop_resolved_stand_ins(&self, account: Option<i64>) -> Result<Vec<i64>> {
        let pairs: Vec<(i64, i64)> = {
            let db = self.rdb();
            db.prepare_cached(
                "SELECT p.folder_id, p.message_id FROM entries p INDEXED BY entries_waiting
                 JOIN messages m ON m.id = p.message_id
                 WHERE p.uid IS NULL AND m.gm_msgid IS NULL AND m.local_copy = 0
                   AND m.pending_send IS NULL AND m.draft_id IS NULL AND m.message_id <> ''
                   AND (?1 IS NULL OR m.account_id = ?1)
                   AND NOT EXISTS (SELECT 1 FROM pending_ops o WHERE o.account_id = m.account_id)
                   AND EXISTS (
                       SELECT 1 FROM messages t INDEXED BY messages_msgid JOIN entries te ON te.message_id = t.id
                       WHERE t.account_id = m.account_id AND t.message_id = m.message_id AND t.id <> m.id
                         AND te.folder_id = p.folder_id AND te.uid IS NOT NULL)",
            )?
            .query_map(params![account], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
        };
        if pairs.is_empty() {
            return Ok(vec![]);
        }
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            {
                let mut del = tx.prepare_cached("DELETE FROM entries WHERE folder_id=?1 AND message_id=?2")?;
                for (folder, id) in &pairs {
                    del.execute(params![folder, id])?;
                }
            }
            tx.commit()?;
        }
        let ids: Vec<i64> = pairs.iter().map(|p| p.1).collect();
        self.drop_orphans(&ids)?;
        tracing::info!(n = pairs.len(), "dropped moved copies the server's copy replaced");
        let mut folders: Vec<i64> = pairs.into_iter().map(|p| p.0).collect();
        folders.sort_unstable();
        folders.dedup();
        Ok(folders)
    }

    /// Taken back (undo, edit after a failure): the local copy goes. Returns its folders.
    pub fn drop_local_copy(&self, outgoing_id: i64) -> Result<Vec<i64>> {
        let Some(id) = self.local_copy_of(outgoing_id)? else { return Ok(vec![]) };
        let folders = self.message_folder_ids(id)?;
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            for f in &folders {
                tx.execute("UPDATE folders SET total=max(0, total-1) WHERE id=?1", [f])?;
            }
            tx.execute("DELETE FROM entries WHERE message_id=?1", [id])?;
            tx.execute("DELETE FROM messages WHERE id=?1", [id])?;
            tx.commit()?;
        }
        self.bodies.lock().unwrap_or_else(|e| e.into_inner()).execute("DELETE FROM bodies WHERE message_id=?1", [id])?;
        Ok(folders)
    }

    /// A message's own Message-ID, In-Reply-To and References (for replying to it).
    pub fn threading_of(&self, id: i64) -> Result<Option<crate::compose::Original>> {
        let row: Option<(Option<String>, Option<String>, Option<String>)> = self
            .rdb()
            .query_row("SELECT message_id, in_reply_to, refs FROM messages WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        Ok(row.map(|(message_id, in_reply_to, refs)| crate::compose::Original {
            message_id,
            in_reply_to,
            references: refs.map(|r| r.split_whitespace().map(String::from).collect()).unwrap_or_default(),
            ..Default::default()
        }))
    }

    /// Gmail's X-GM-MSGID for a message, if known.
    /// The message's Message-ID header (without brackets).
    pub fn message_header_id(&self, id: i64) -> Result<Option<String>> {
        let v: Option<Option<String>> =
            self.rdb().query_row("SELECT message_id FROM messages WHERE id=?1", [id], |r| r.get(0)).optional()?;
        Ok(v.flatten())
    }

    /// Copies placed locally (a move) still waiting for their UID: (folder id, raw name).
    pub fn message_waiting_copies(&self, id: i64) -> Result<Vec<(i64, String)>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT f.id, f.raw_name FROM entries e JOIN folders f ON f.id=e.folder_id
             WHERE e.message_id=?1 AND e.uid IS NULL",
        )?;
        let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn message_gm_msgid(&self, id: i64) -> Result<Option<u64>> {
        let v: Option<Option<i64>> =
            self.rdb().query_row("SELECT gm_msgid FROM messages WHERE id=?1", [id], |r| r.get(0)).optional()?;
        Ok(v.flatten().map(|v| v as u64))
    }

    /// Folder ids a message is in (with or without a UID).
    pub fn message_folder_ids(&self, id: i64) -> Result<Vec<i64>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT folder_id FROM entries WHERE message_id=?1")?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- tags --------------------------------------------------------------------------------

    /// Tag definitions plus keywords seen on messages that have none (shown under their keyword).
    pub fn tags(&self) -> Result<Vec<crate::tags::TagDef>> {
        let db = self.rdb();
        let mut out: Vec<crate::tags::TagDef> = {
            let mut stmt = db.prepare_cached("SELECT keyword, name, color, position FROM tags ORDER BY position, name")?;
            stmt.query_map([], |r| {
                Ok(crate::tags::TagDef { keyword: r.get(0)?, name: r.get(1)?, color: r.get(2)?, position: r.get(3)?, defined: true })
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        let mut stmt = db.prepare_cached("SELECT DISTINCT keywords FROM messages WHERE keywords != ''")?;
        let combos: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut extra: Vec<String> = Vec::new();
        for c in combos {
            for k in crate::tags::visible(&c) {
                if !out.iter().any(|t| t.keyword == k) && !extra.contains(&k) {
                    extra.push(k);
                }
            }
        }
        extra.sort();
        let base = out.len() as i64;
        for (i, k) in extra.into_iter().enumerate() {
            out.push(crate::tags::TagDef {
                color: crate::tags::generated_color(&k),
                name: k.clone(),
                keyword: k,
                position: base + i as i64,
                defined: false,
            });
        }
        Ok(out)
    }

    /// Create or update a tag definition. A new tag without a keyword gets Thunderbird's keyword
    /// for its name. Returns the keyword.
    pub fn set_tag(&self, keyword: Option<&str>, name: &str, color: &str) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("A tag needs a name.");
        }
        let db = self.db();
        let keyword = match keyword {
            Some(k) => crate::tags::normalize(k),
            None => {
                let k = crate::tags::keyword_for_name(name);
                let taken: bool = db.prepare_cached("SELECT 1 FROM tags WHERE keyword=?1")?.exists([&k])?;
                if taken {
                    anyhow::bail!("A tag with that name already exists.");
                }
                k
            }
        };
        let position: i64 = db
            .query_row("SELECT position FROM tags WHERE keyword=?1", [&keyword], |r| r.get(0))
            .optional()?
            .unwrap_or(db.query_row("SELECT coalesce(max(position), -1) + 1 FROM tags", [], |r| r.get(0))?);
        db.execute(
            "INSERT INTO tags (keyword, name, color, position) VALUES (?1,?2,?3,?4)
             ON CONFLICT(keyword) DO UPDATE SET name=excluded.name, color=excluded.color",
            params![keyword, name, color, position],
        )?;
        Ok(keyword)
    }

    /// Forget a tag's definition (messages keep the keyword; it's then shown as an undefined tag).
    pub fn delete_tag(&self, keyword: &str) -> Result<()> {
        self.db().execute("DELETE FROM tags WHERE keyword=?1", [crate::tags::normalize(keyword)])?;
        Ok(())
    }

    /// Keyword for a `tag:` search term: a defined tag's name (case-insensitive), else the term
    /// itself as a keyword.
    pub fn tag_keyword(&self, term: &str) -> Result<String> {
        let found: Option<String> = self
            .rdb()
            .query_row("SELECT keyword FROM tags WHERE lower(name)=lower(?1)", [term.trim()], |r| r.get(0))
            .optional()?;
        Ok(found.unwrap_or_else(|| crate::tags::normalize(term)))
    }

    /// After a local snooze change: Inbox dates follow the keywords (see `settle_inbox_date`).
    pub fn settle_inbox_dates(&self, ids: &[i64]) -> Result<()> {
        let db = self.db();
        for &id in ids {
            settle_inbox_date(&db, id)?;
        }
        Ok(())
    }

    /// The account's snoozed messages whose time is at or before `now`, each with the folder to
    /// act on it from: the Inbox if it's there, else Gmail's All Mail, else wherever it is.
    /// Only copies the server has (one still being moved waits for its UID); not in Trash or
    /// Spam (deleted, it isn't wanted back).
    pub fn snoozed_due(&self, account_id: i64, now: i64) -> Result<Vec<(i64, i64)>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT m.id, m.keywords,
                    (SELECT e.folder_id FROM entries e JOIN folders f ON f.id = e.folder_id
                     WHERE e.message_id = m.id AND e.uid IS NOT NULL
                       AND coalesce(f.role, '') NOT IN ('trash', 'junk')
                     ORDER BY CASE f.role WHEN 'inbox' THEN 0 WHEN 'all' THEN 1 ELSE 2 END LIMIT 1)
             FROM messages m
             WHERE m.account_id=?1 AND m.keywords LIKE '%$snooze-%'",
        )?;
        let rows = stmt.query_map([account_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?))
        })?;
        let mut due = Vec::new();
        for row in rows {
            let (id, keywords, folder) = row?;
            if let Some(folder) = folder
                && crate::tags::snoozed_until(&keywords).is_some_and(|t| t <= now)
            {
                due.push((id, folder));
            }
        }
        Ok(due)
    }

    /// Sender and subject of messages, for a notification.
    pub fn new_messages(&self, ids: &[i64]) -> Result<Vec<NewMessage>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT from_name, from_addr, subject FROM messages WHERE id=?1")?;
        let mut out = Vec::new();
        for &id in ids {
            if let Some((from_name, from_addr, subject)) =
                stmt.query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?
            {
                out.push(NewMessage { id, from_name, from_addr, subject });
            }
        }
        Ok(out)
    }

    /// The message's stored keywords (space-separated, normalised).
    pub fn message_keywords(&self, id: i64) -> Result<String> {
        let k: Option<String> = self.rdb().query_row("SELECT keywords FROM messages WHERE id=?1", [id], |r| r.get(0)).optional()?;
        Ok(k.unwrap_or_default())
    }

    pub fn message_tags(&self, id: i64) -> Result<Vec<String>> {
        let k: Option<String> = self.rdb().query_row("SELECT keywords FROM messages WHERE id=?1", [id], |r| r.get(0)).optional()?;
        Ok(k.map(|k| crate::tags::visible(&k)).unwrap_or_default())
    }

    pub fn message_flags(&self, id: i64) -> Result<i64> {
        Ok(self.db().query_row("SELECT flags FROM messages WHERE id=?1", [id], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn rec(uid: u32, gm: u64, labels: &[&str]) -> HeaderRecord {
        HeaderRecord {
            original_message_id: None,
            uid,
            gm_msgid: Some(gm),
            email_id: None,
            gm_labels: Some(labels.iter().map(|s| s.to_string()).collect()),
            keywords: vec![],
            message_id: None,
            in_reply_to: None,
            refs: None,
            subject: format!("m{uid}"),
            from_name: String::new(),
            from_addr: "a@b.c".into(),
            to_addrs: String::new(),
            date: uid as i64,
            size: 10,
            flags: 0,
            has_attachments: false,
        }
    }

    #[test]
    fn kept_attachments_queue_partial_bodies() {
        let dir = std::env::temp_dir().join(format!("mail-core-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@gmail.com", "x", Provider::Gmail, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store.sync_folder_list(acct.id, &[lf("[Gmail]/All Mail", Some(FolderRole::All))]).unwrap();
        let all = store.folders(acct.id).unwrap()[0].id;
        store.upsert_headers(acct.id, all, &[rec(1, 101, &[]), rec(2, 102, &[]), rec(3, 103, &[])], &HashMap::new()).unwrap();
        let id = |u: u32| store.message_ids_for_uids(all, &[u]).unwrap()[0];
        // 1: no body yet; 2: stored without its attachment; 3: complete.
        store.store_body(id(2), b"Subject: m2\r\n\r\npartial", None, true, Some("[]")).unwrap();
        store.store_body(id(3), b"Subject: m3\r\n\r\nwhole", None, false, None).unwrap();
        assert_eq!(store.messages_missing_body(acct.id, None, 10).unwrap(), vec![id(1)]);
        assert_eq!(store.count_missing_bodies(acct.id).unwrap(), (3, 2));
        assert_eq!(store.attachments_missing(acct.id).unwrap(), (1, 10));
        let c = &store.attachment_counts().unwrap()[0];
        assert_eq!((c.missing, c.missing_bytes, c.kept, c.kept_bytes), (1, 10, 0, 0));
        // Kept: the partial one is queued too, after anything without a body.
        store.set_keep_attachments(acct.id, true).unwrap();
        assert_eq!(store.messages_missing_body(acct.id, None, 10).unwrap(), vec![id(1), id(2)]);
        assert_eq!(store.count_missing_bodies(acct.id).unwrap(), (3, 1));
        // Downloaded whole (a real attachment this time): done.
        let whole = b"Subject: m2\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nwhole\r\n\
--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"r.pdf\"\r\n\r\n%PDF\r\n--b--\r\n";
        store.store_body(id(2), whole, None, true, None).unwrap();
        assert_eq!(store.messages_missing_body(acct.id, None, 10).unwrap(), vec![id(1)]);
        assert_eq!(store.attachments_missing(acct.id).unwrap(), (0, 0));
        let c = &store.attachment_counts().unwrap()[0];
        assert_eq!((c.missing, c.kept, c.kept_bytes), (0, 1, 10), "now stored with its attachment");
        // Storage: three messages of 10 bytes on the server; two bodies here.
        let s = store.account_storage().unwrap();
        assert_eq!((s.len(), s[0].messages, s[0].server_bytes), (1, 3, 30));
        assert_eq!(s[0].local_bytes, (whole.len() + b"Subject: m3\r\n\r\nwhole".len()) as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_attachments_keeps_the_text() {
        let dir = std::env::temp_dir().join(format!("mail-core-remove-att-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@gmail.com", "x", Provider::Gmail, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store.sync_folder_list(acct.id, &[lf("[Gmail]/All Mail", Some(FolderRole::All))]).unwrap();
        let all = store.folders(acct.id).unwrap()[0].id;
        store.upsert_headers(acct.id, all, &[rec(1, 101, &[])], &HashMap::new()).unwrap();
        let id = store.message_ids_for_uids(all, &[1]).unwrap()[0];
        let raw = b"Subject: Report\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n\
--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"r.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQK\r\n\
--b--\r\n";
        store.store_body(id, raw, None, true, None).unwrap();
        let c = &store.attachment_counts().unwrap()[0];
        assert_eq!((c.missing, c.kept), (0, 1));

        assert_eq!(store.remove_attachments(acct.id).unwrap().0, 1);
        let c = &store.attachment_counts().unwrap()[0];
        assert_eq!((c.missing, c.kept), (1, 0), "to be fetched again when opened");
        let body = store.body(id).unwrap().unwrap();
        assert!(String::from_utf8_lossy(&body).contains("See attached."));
        assert!(!String::from_utf8_lossy(&body).contains("JVBERi0xLjQK"));
        let omitted: Vec<crate::partial::OmittedPart> =
            serde_json::from_str(&store.rdb().query_row("SELECT omitted FROM messages WHERE id=?1", [id], |r| r.get::<_, String>(0)).unwrap()).unwrap();
        assert_eq!((omitted[0].name.as_str(), omitted[0].section.as_deref()), ("r.pdf", Some("2")));
        // Nothing left to remove.
        assert_eq!(store.remove_attachments(acct.id).unwrap().0, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_attachments_are_left_out_once() {
        let dir = std::env::temp_dir().join(format!("mail-core-fallback-att-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@gmail.com", "x", Provider::Gmail, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store.sync_folder_list(acct.id, &[lf("[Gmail]/All Mail", Some(FolderRole::All))]).unwrap();
        let all = store.folders(acct.id).unwrap()[0].id;
        store.upsert_headers(acct.id, all, &[rec(1, 101, &[])], &HashMap::new()).unwrap();
        let id = store.message_ids_for_uids(all, &[1]).unwrap()[0];
        let raw = b"Subject: Report\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n\
--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"r.pdf\"\r\n\r\n%PDF\r\n--b--\r\n";
        store.store_body(id, raw, None, true, None).unwrap();
        // As upgrade step 7 (or the first look after step 6) marks it in an account not keeping
        // attachments; the next start strips it, once.
        store.db().execute("UPDATE messages SET attachments_stored=2 WHERE id=?1", [id]).unwrap();
        drop(store);
        let store = Store::open(&dir).unwrap();
        let c = &store.attachment_counts().unwrap()[0];
        assert_eq!((c.missing, c.kept), (1, 0));
        assert!(String::from_utf8_lossy(&store.body(id).unwrap().unwrap()).contains("See attached."));
        let flag: i64 = store.rdb().query_row("SELECT attachments_stored FROM messages WHERE id=?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(flag, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn accounts_keep_the_users_order_and_markers() {
        let dir = std::env::temp_dir().join(format!("mail-core-acct-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let a = store.insert_account("john@example.com", "John", Provider::Imap, "h", 993).unwrap().id;
        let b = store.insert_account("jane@example.org", "Jane", Provider::Imap, "h", 993).unwrap().id;
        let c = store.insert_account("jdoe@example.net", "J", Provider::Imap, "h", 993).unwrap().id;
        let ids = |s: &Store| s.accounts().unwrap().iter().map(|x| x.id).collect::<Vec<_>>();
        assert_eq!(ids(&store), [a, b, c]);
        // c first, then a; b (not in the list) after them
        store.set_account_order(&[c, a]).unwrap();
        assert_eq!(ids(&store), [c, a, b]);
        store.set_account_mark(b, Some("W"), Some("teal")).unwrap();
        let d = store.insert_account("doe@example.com", "D", Provider::Imap, "h", 993).unwrap();
        assert_eq!((d.mark_letter, d.mark_color), (None, None));
        drop(store);
        let store = Store::open(&dir).unwrap();
        assert_eq!(ids(&store), [c, a, b, d.id], "a new account comes last");
        let jane = store.accounts().unwrap().into_iter().find(|x| x.id == b).unwrap();
        assert_eq!((jane.mark_letter.as_deref(), jane.mark_color.as_deref()), (Some("W"), Some("teal")));
        store.set_account_mark(b, None, None).unwrap();
        let jane = store.accounts().unwrap().into_iter().find(|x| x.id == b).unwrap();
        assert_eq!((jane.mark_letter, jane.mark_color), (None, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leftover_local_copies_of_sent_mail_go() {
        let dir = std::env::temp_dir().join(format!("mail-core-leftover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("john@example.com", "John", Provider::Outlook, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store.sync_folder_list(acct.id, &[lf("Sent", Some(FolderRole::Sent))]).unwrap();
        let sent = store.folders(acct.id).unwrap()[0].id;
        let raw = b"From: John <john@example.com>\r\nTo: Jane Doe <jane@example.org>\r\nSubject: Plans\r\nMessage-ID: <ours@example.com>\r\nDate: Fri, 6 Nov 2026 23:03:51 +0000\r\n\r\nHi\r\n";
        let copy = store.insert_local_copy(acct.id, sent, raw, 7).unwrap();
        store.local_copy_sent(7).unwrap();
        // Sent just now (the Date header above is fixed, and "a day old" is measured from now).
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        store.db().execute("UPDATE messages SET date=?1 WHERE id=?2", params![now, copy]).unwrap();
        store.db().execute("UPDATE entries SET date=?1 WHERE message_id=?2", params![now, copy]).unwrap();
        // The server's copy: another Message-ID, nothing pointing back at ours.
        let date: i64 = store.rdb().query_row("SELECT date FROM messages WHERE id=?1", [copy], |r| r.get(0)).unwrap();
        let server = HeaderRecord {
            gm_msgid: None,
            gm_labels: None,
            message_id: Some("theirs@prod.example".into()),
            subject: "Plans".into(),
            from_addr: "John@Example.com".into(),
            to_addrs: "jane@example.org".into(),
            date: date + 12,
            ..rec(41, 0, &[])
        };
        store.upsert_headers(acct.id, sent, &[server], &HashMap::new()).unwrap();
        let count = || store.list_messages(sent, 0, 10).unwrap().messages.len();
        assert_eq!(count(), 2);
        // Not yet: only copies a day old are looked at (the server's may still be on its way).
        assert!(store.drop_matched_local_copies(None).unwrap().is_empty());
        store.db().execute("UPDATE messages SET date=date-2*86400", []).unwrap();
        // Another recipient: not the same message.
        store.db().execute("UPDATE messages SET to_addrs='bob@example.org' WHERE id<>?1", [copy]).unwrap();
        assert!(store.drop_matched_local_copies(None).unwrap().is_empty());
        store.db().execute("UPDATE messages SET to_addrs='Jane <JANE@example.org>' WHERE id<>?1", [copy]).unwrap();
        assert_eq!(store.drop_matched_local_copies(None).unwrap(), vec![sent]);
        assert_eq!(count(), 1);
        assert!(store.body(copy).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn threads_by_references() {
        let dir = std::env::temp_dir().join(format!("mail-core-thread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        store
            .sync_folder_list(acct.id, &[ListedFolder { raw_name: "INBOX".into(), path: vec!["INBOX".into()], role: Some(FolderRole::Inbox), selectable: true }])
            .unwrap();
        let inbox = store.folders(acct.id).unwrap()[0].id;
        let msg = |uid: u32, mid: &str, irt: Option<&str>, refs: Option<&str>| HeaderRecord {
            gm_msgid: None,
            gm_labels: None,
            message_id: Some(mid.into()),
            in_reply_to: irt.map(Into::into),
            refs: refs.map(Into::into),
            ..rec(uid, 0, &[])
        };
        let none = HashMap::new();
        // b and c reply to a parent "p" we never received; d replies to c; e is unrelated.
        // f arrives before its parent g; g must join f's thread.
        store
            .upsert_headers(acct.id, inbox, &[
                msg(1, "b@x", Some("<p@x>"), Some("<p@x>")),
                msg(2, "c@x", Some("<p@x>"), Some("<p@x>")),
                msg(3, "d@x", Some("<c@x>"), Some("<p@x> <c@x>")),
                msg(4, "e@x", None, None),
                msg(5, "f@x", Some("<g@x>"), None),
                msg(6, "g@x", None, None),
            ], &none)
            .unwrap();
        let page = store.list_threads(inbox, ThreadScope::default(), 0, 10).unwrap();
        assert_eq!(page.total, 3);
        let mut counts: Vec<u32> = page.threads.iter().map(|t| t.message_count).collect();
        counts.sort();
        assert_eq!(counts, vec![1, 2, 3]);
        let big = page.threads.iter().find(|t| t.message_count == 3).unwrap();
        let msgs = store.thread_messages(big.id, inbox, ThreadScope::default()).unwrap().messages;
        assert_eq!(msgs.iter().map(|m| m.subject.as_str()).collect::<Vec<_>>(), vec!["m1", "m2", "m3"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conversation_scope() {
        let dir = std::env::temp_dir().join(format!("mail-core-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store
            .sync_folder_list(acct.id, &[lf("INBOX", Some(FolderRole::Inbox)), lf("Sent", Some(FolderRole::Sent)), lf("Trash", Some(FolderRole::Trash))])
            .unwrap();
        let folders = store.folders(acct.id).unwrap();
        let id = |n: &str| folders.iter().find(|f| f.raw_name == n).unwrap().id;
        let msg = |uid: u32, mid: &str, irt: Option<&str>| HeaderRecord {
            gm_msgid: None,
            gm_labels: None,
            message_id: Some(mid.into()),
            in_reply_to: irt.map(Into::into),
            ..rec(uid, 0, &[])
        };
        let none = HashMap::new();
        // a (Inbox) <- b (Sent reply) <- c (Inbox) ; d (Trash) replies to c.
        store.upsert_headers(acct.id, id("INBOX"), &[msg(1, "a@x", None), msg(3, "c@x", Some("<b@x>"))], &none).unwrap();
        store.upsert_headers(acct.id, id("Sent"), &[msg(2, "b@x", Some("<a@x>"))], &none).unwrap();
        store.upsert_headers(acct.id, id("Trash"), &[msg(4, "d@x", Some("<c@x>"))], &none).unwrap();

        let inbox = id("INBOX");
        let thread = store.list_threads(inbox, ThreadScope::default(), 0, 10).unwrap().threads[0].id;
        let subjects = |scope| -> Vec<String> {
            store.thread_messages(thread, inbox, scope).unwrap().messages.into_iter().map(|m| m.subject).collect()
        };
        let s = |all_folders, include_trash| ThreadScope { all_folders, include_trash };
        assert_eq!(subjects(s(false, false)), vec!["m1", "m3"]);
        assert_eq!(subjects(s(true, false)), vec!["m1", "m2", "m3"]);
        assert_eq!(subjects(s(false, true)), vec!["m1", "m3", "m4"]);
        assert_eq!(subjects(s(true, true)), vec!["m1", "m2", "m3", "m4"]);
        let hidden = |scope| {
            let v = store.thread_messages(thread, inbox, scope).unwrap();
            (v.hidden_other, v.hidden_trash)
        };
        assert_eq!(hidden(s(false, false)), (1, 1));
        assert_eq!(hidden(s(true, false)), (0, 1));
        assert_eq!(hidden(s(true, true)), (0, 0));
        assert_eq!(store.list_threads(inbox, s(false, false), 0, 10).unwrap().threads[0].message_count, 2);
        assert_eq!(store.list_threads(inbox, s(true, true), 0, 10).unwrap().threads[0].message_count, 4);
        // Browsing Trash itself still shows the trashed message by default.
        let trash = id("Trash");
        assert_eq!(store.list_threads(trash, ThreadScope::default(), 0, 10).unwrap().threads[0].message_count, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_gmail_copies_share_one_message_and_moves_keep_bodies() {
        let dir = std::env::temp_dir().join(format!("mail-core-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store
            .sync_folder_list(acct.id, &[lf("INBOX", Some(FolderRole::Inbox)), lf("Archive", Some(FolderRole::Archive))])
            .unwrap();
        let folders = store.folders(acct.id).unwrap();
        let id = |n: &str| folders.iter().find(|f| f.raw_name == n).unwrap().id;
        let (inbox, archive) = (id("INBOX"), id("Archive"));
        let msg = |uid: u32, mid: &str| HeaderRecord {
            gm_msgid: None,
            gm_labels: None,
            message_id: Some(mid.into()),
            ..rec(uid, 0, &[])
        };
        let none = HashMap::new();
        let total = |s: &Store| s.count_missing_bodies(acct.id).unwrap().0;

        // A copy in Archive joins the Inbox message: one row, one download.
        store.upsert_headers(acct.id, inbox, &[msg(1, "a@x")], &none).unwrap();
        store.upsert_headers(acct.id, archive, &[msg(7, "a@x")], &none).unwrap();
        assert_eq!(total(&store), 1);
        // A duplicate delivery in the same folder stays separate (each UID needs an entry).
        store.upsert_headers(acct.id, inbox, &[msg(2, "a@x")], &none).unwrap();
        assert_eq!(total(&store), 2);
        assert_eq!(store.list_messages(inbox, 0, 10).unwrap().total, 2);

        // Move b@x from Inbox to Archive, seeing the Inbox expunge first: the body survives.
        store.upsert_headers(acct.id, inbox, &[msg(3, "b@x")], &none).unwrap();
        let b = store.message_ids_for_uids(inbox, &[3]).unwrap()[0];
        store.store_body(b, b"Subject: b\r\n\r\nbody b", Some("body b"), false, None).unwrap();
        store.remove_uids(inbox, &[3]).unwrap();
        store.upsert_headers(acct.id, archive, &[msg(8, "b@x")], &none).unwrap();
        let moved = store.message_ids_for_uids(archive, &[8]).unwrap()[0];
        assert_eq!(store.body(moved).unwrap().as_deref(), Some(&b"Subject: b\r\n\r\nbody b"[..]));
        let page = store.list_messages(archive, 0, 10).unwrap();
        assert!(page.messages.iter().any(|m| m.id == moved && m.has_body && m.snippet.as_deref() == Some("body b")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_of_matches_list_order() {
        let dir = std::env::temp_dir().join(format!("mail-core-indexof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        store
            .sync_folder_list(acct.id, &[ListedFolder { raw_name: "INBOX".into(), path: vec!["INBOX".into()], role: Some(FolderRole::Inbox), selectable: true }])
            .unwrap();
        let inbox = store.folders(acct.id).unwrap()[0].id;
        // Same dates on purpose (ties), replies forming threads.
        let recs: Vec<HeaderRecord> = (1..=40u32)
            .map(|u| HeaderRecord {
                gm_msgid: None,
                gm_labels: None,
                message_id: Some(format!("m{u}@x")),
                in_reply_to: (u % 3 == 0).then(|| format!("m{}@x", u - 1)),
                date: (u / 4) as i64,
                ..rec(u, 0, &[])
            })
            .collect();
        store.upsert_headers(acct.id, inbox, &recs, &HashMap::new()).unwrap();
        let flat = store.list_messages(inbox, 0, 100).unwrap().messages;
        for (i, m) in flat.iter().enumerate() {
            assert_eq!(store.index_of(inbox, m.id, false).unwrap(), Some(i as u32));
        }
        let threads = store.list_threads(inbox, ThreadScope::default(), 0, 100).unwrap().threads;
        for (i, t) in threads.iter().enumerate() {
            assert_eq!(store.index_of(inbox, t.id, true).unwrap(), Some(i as u32));
        }
        assert_eq!(store.index_of(inbox, 999_999, false).unwrap(), None);
        assert_eq!(store.index_of(inbox, 999_999, true).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_deleted_elsewhere_drops_its_messages() {
        let dir = std::env::temp_dir().join(format!("mail-core-folderdel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        let all = [lf("INBOX", Some(FolderRole::Inbox)), lf("Old", None), lf("Keep", None)];
        store.sync_folder_list(acct.id, &all).unwrap();
        let id = |n: &str| store.folders(acct.id).unwrap().into_iter().find(|f| f.raw_name == n).unwrap().id;
        let none = HashMap::new();
        let msg = |uid: u32, mid: &str| HeaderRecord { gm_msgid: None, gm_labels: None, message_id: Some(mid.into()), ..rec(uid, 0, &[]) };
        // 30 messages only in Old (bodies downloaded), one also in Keep.
        let recs: Vec<HeaderRecord> = (1..=30).map(|u| msg(u, &format!("o{u}@x"))).collect();
        store.upsert_headers(acct.id, id("Old"), &recs, &none).unwrap();
        store.upsert_headers(acct.id, id("Keep"), &[msg(7, "o7@x")], &none).unwrap();
        for m in store.list_messages(id("Old"), 0, 100).unwrap().messages {
            store.store_body(m.id, b"Subject: x\r\n\r\nbody", Some("body"), false, None).unwrap();
        }
        assert_eq!(store.count_missing_bodies(acct.id).unwrap(), (30, 30));

        // A glitchy empty listing removes nothing.
        store.sync_folder_list(acct.id, &[]).unwrap();
        assert_eq!(store.count_missing_bodies(acct.id).unwrap(), (30, 30));

        // Another client deletes "Old".
        store.sync_folder_list(acct.id, &[all[0].clone(), all[2].clone()]).unwrap();
        assert!(store.folders(acct.id).unwrap().iter().all(|f| f.raw_name != "Old"));
        // Only the message that's also in Keep survives, body intact; the rest are gone,
        // from counts and search alike.
        assert_eq!(store.count_missing_bodies(acct.id).unwrap(), (1, 1));
        assert_eq!(store.search(acct.id, None, "m1", &[], 0, 50).unwrap().total, 0);
        let kept = store.list_messages(id("Keep"), 0, 10).unwrap().messages;
        assert!(kept.len() == 1 && store.body(kept[0].id).unwrap().is_some());

        // Renamed rather than deleted: the same messages reappear under "Older" and pick up
        // their stashed bodies instead of downloading again.
        store.sync_folder_list(acct.id, &[all[0].clone(), all[2].clone(), lf("Older", None)]).unwrap();
        store.upsert_headers(acct.id, id("Older"), &recs, &none).unwrap();
        let older = store.list_messages(id("Older"), 0, 100).unwrap().messages;
        assert_eq!(older.len(), 30);
        assert!(older.iter().all(|m| m.has_body), "bodies restored from the stash");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quick_search() {
        let dir = std::env::temp_dir().join(format!("mail-core-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        store
            .sync_folder_list(acct.id, &[ListedFolder { raw_name: "INBOX".into(), path: vec!["INBOX".into()], role: Some(FolderRole::Inbox), selectable: true }])
            .unwrap();
        let inbox = store.folders(acct.id).unwrap()[0].id;
        let mk = |uid: u32, subject: &str, from: (&str, &str), to: &str| HeaderRecord {
            gm_msgid: None,
            gm_labels: None,
            subject: subject.into(),
            from_name: from.0.into(),
            from_addr: from.1.into(),
            to_addrs: to.into(),
            ..rec(uid, 0, &[])
        };
        store
            .upsert_headers(acct.id, inbox, &[
                mk(1, "Invoice #42 for October", ("Åda Lovelace", "ada@example.com"), "Bob <bob@corp.com>"),
                mk(2, "Weekly report", ("Bob", "bob@corp.com"), "Ada <ada@example.com>"),
                mk(3, "Draft: weekly report", ("Bob", "bob@corp.com"), "team@corp.com"),
            ], &HashMap::new())
            .unwrap();
        let hits = |q: &str| -> Vec<String> {
            store.search(acct.id, None, q, &[], 0, 50).unwrap().messages.into_iter().map(|m| m.subject).collect()
        };
        assert_eq!(hits("inv"), vec!["Invoice #42 for October"]);
        assert_eq!(hits("ada"), vec!["Weekly report", "Invoice #42 for October"]);
        assert_eq!(hits("from:ada"), vec!["Invoice #42 for October"]); // diacritics folded: Åda
        assert_eq!(hits("to:ada@exa"), vec!["Weekly report"]);
        assert_eq!(hits("\"weekly report\" -draft"), vec!["Weekly report"]);
        assert_eq!(hits("subject:report from:bob"), vec!["Draft: weekly report", "Weekly report"]);
        assert!(hits("nothing-matches-this").is_empty());
        assert_eq!(store.search(acct.id, Some(inbox), "report", &[], 0, 1).unwrap().total, 2);
        // "bob" is a sender of two messages and a recipient of one.
        let only = |fields: &[&str]| store.search(acct.id, None, "bob", fields, 0, 50).unwrap().total;
        assert_eq!((only(&["sender"]), only(&["recipients"]), only(&["subject"])), (2, 1, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Timing against a real database copy: MAIL_DB_DIR=/path cargo test -p mail-core real_db -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_db_thread_timing() {
        let dir = std::path::PathBuf::from(std::env::var("MAIL_DB_DIR").unwrap());
        let t = std::time::Instant::now();
        let store = Store::open(&dir).unwrap();
        println!("open (incl. thread backfill): {:?}", t.elapsed());
        for a in store.accounts().unwrap() {
            for f in store.folders(a.id).unwrap().iter().filter(|f| f.synced_count > 0) {
                let t = std::time::Instant::now();
                let p = store.list_threads(f.id, ThreadScope::default(), 0, 200).unwrap();
                let first = t.elapsed();
                let t = std::time::Instant::now();
                let _ = store.list_threads(f.id, ThreadScope::default(), 2000.min(p.total.saturating_sub(1)), 200).unwrap();
                println!("{:<40} {:>7} msgs {:>7} threads  page0 {:>6.1?}  deep {:>6.1?}", f.raw_name, f.synced_count, p.total, first, t.elapsed());
            }
            for q in ["a", "re", "invoice", "from:hall", "to:richard subject:re", "\"pull request\""] {
                let t = std::time::Instant::now();
                let r = store.search(a.id, None, q, &[], 0, 200).unwrap();
                println!("search {:<28} {:>7} hits  {:>6.1?}", q, r.total, t.elapsed());
            }
        }
    }

    #[test]
    fn schema_versions() {
        let dir = std::env::temp_dir().join(format!("mail-core-test-versions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let version = |dir: &Path| -> i64 {
            Connection::open(dir.join("mail.db")).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
        };
        let set_version = |dir: &Path, v: i64| Connection::open(dir.join("mail.db")).unwrap().execute_batch(&format!("PRAGMA user_version={v};")).unwrap();
        // New: the whole schema, current version.
        let store = Store::open(&dir).unwrap();
        store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        drop(store);
        assert_eq!(version(&dir), SCHEMA_VERSION);
        // At the base (or later): kept, and brought up to date by the steps since.
        {
            let db = Connection::open(dir.join("mail.db")).unwrap();
            db.execute_batch(&format!(
                "ALTER TABLE accounts DROP COLUMN keep_attachments; ALTER TABLE accounts DROP COLUMN auth;
                 ALTER TABLE accounts DROP COLUMN keywords; ALTER TABLE accounts DROP COLUMN position;
                 ALTER TABLE accounts DROP COLUMN mark_letter; ALTER TABLE accounts DROP COLUMN mark_color;
                 ALTER TABLE messages DROP COLUMN attachments_stored;
                 PRAGMA user_version={BASE_VERSION};"
            ))
            .unwrap();
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.accounts().unwrap().len(), 1);
        assert!(!store.keeps_attachments(store.accounts().unwrap()[0].id).unwrap());
        drop(store);
        assert_eq!(version(&dir), SCHEMA_VERSION);
        // From a newer build: refused, untouched.
        set_version(&dir, SCHEMA_VERSION + 1);
        assert!(Store::open(&dir).is_err());
        assert_eq!(version(&dir), SCHEMA_VERSION + 1);
        // Older than the base: started over.
        set_version(&dir, BASE_VERSION - 1);
        let store = Store::open(&dir).unwrap();
        assert!(store.accounts().unwrap().is_empty());
        drop(store);
        assert_eq!(version(&dir), SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gmail_labels_drive_membership_and_dedupe() {
        let dir = std::env::temp_dir().join(format!("mail-core-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@gmail.com", "x", Provider::Gmail, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store
            .sync_folder_list(acct.id, &[lf("INBOX", Some(FolderRole::Inbox)), lf("[Gmail]/All Mail", Some(FolderRole::All)), lf("Work", None)])
            .unwrap();
        let folders = store.folders(acct.id).unwrap();
        let id = |n: &str| folders.iter().find(|f| f.raw_name == n).unwrap().id;
        let (inbox, all, work) = (id("INBOX"), id("[Gmail]/All Mail"), id("Work"));
        let labels: HashMap<String, i64> = crate::sync::gmail_label_map(&folders);

        // Same message seen in INBOX and All Mail is stored once.
        store.upsert_headers(acct.id, inbox, &[rec(5, 100, &["\\Inbox", "Work"])], &labels).unwrap();
        store.upsert_headers(acct.id, all, &[rec(900, 100, &["\\Inbox", "Work"])], &labels).unwrap();
        assert_eq!(store.count_missing_bodies(acct.id).unwrap().0, 1);
        assert_eq!(store.list_messages(work, 0, 10).unwrap().total, 1);
        assert_eq!(store.list_messages(inbox, 0, 10).unwrap().total, 1);

        // Archived and unlabelled via All Mail: leaves Inbox and Work, stays in All Mail.
        store
            .update_flags(all, &[FlagChange { uid: 900, flags: flags::SEEN, keywords: vec![], labels: Some(vec![]) }], &labels)
            .unwrap();
        assert_eq!(store.list_messages(inbox, 0, 10).unwrap().total, 0);
        assert_eq!(store.list_messages(work, 0, 10).unwrap().total, 0);
        let all_page = store.list_messages(all, 0, 10).unwrap();
        assert_eq!(all_page.total, 1);
        assert!(!all_page.messages[0].unread);

        // Body is fetched from All Mail when preferred.
        let loc = store.body_location(all_page.messages[0].id, Some(all)).unwrap().unwrap();
        assert_eq!((loc.folder_id, loc.uid), (all, 900));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gmail_trash_and_spam_hide_labels() {
        let dir = std::env::temp_dir().join(format!("mail-core-test-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@gmail.com", "x", Provider::Gmail, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store
            .sync_folder_list(
                acct.id,
                &[
                    lf("[Gmail]/All Mail", Some(FolderRole::All)),
                    lf("[Gmail]/Sent Mail", Some(FolderRole::Sent)),
                    lf("[Gmail]/Trash", Some(FolderRole::Trash)),
                    lf("Work", None),
                ],
            )
            .unwrap();
        let folders = store.folders(acct.id).unwrap();
        let id = |n: &str| folders.iter().find(|f| f.raw_name == n).unwrap().id;
        let (all, sent, trash, work) = (id("[Gmail]/All Mail"), id("[Gmail]/Sent Mail"), id("[Gmail]/Trash"), id("Work"));
        let labels: HashMap<String, i64> = crate::sync::gmail_label_map(&folders);

        // A sent, labelled message…
        store.upsert_headers(acct.id, all, &[rec(900, 100, &["\\Sent", "Work"])], &labels).unwrap();
        assert_eq!(store.list_messages(sent, 0, 10).unwrap().total, 1);
        assert_eq!(store.list_messages(work, 0, 10).unwrap().total, 1);
        // …deleted: in Trash it keeps its labels, but shows under none of them.
        store.upsert_headers(acct.id, trash, &[rec(44, 100, &["\\Sent", "Work"])], &labels).unwrap();
        assert_eq!(store.list_messages(trash, 0, 10).unwrap().total, 1);
        assert_eq!(store.list_messages(sent, 0, 10).unwrap().total, 0);
        assert_eq!(store.list_messages(work, 0, 10).unwrap().total, 0);
        // A flag change seen in Trash doesn't bring them back either.
        store
            .update_flags(trash, &[FlagChange { uid: 44, flags: flags::SEEN, keywords: vec![], labels: Some(vec!["\\Sent".into()]) }], &labels)
            .unwrap();
        assert_eq!(store.list_messages(sent, 0, 10).unwrap().total, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
