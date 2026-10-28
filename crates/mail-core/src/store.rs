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
    imap_port    INTEGER NOT NULL
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
    thread_id       INTEGER  -- conversation; the id of one of its messages
);
CREATE UNIQUE INDEX IF NOT EXISTS messages_gm ON messages(account_id, gm_msgid) WHERE gm_msgid IS NOT NULL;
CREATE INDEX IF NOT EXISTS messages_nobody ON messages(account_id, has_body, date DESC);

-- Message-IDs each message refers to (In-Reply-To + References), for threading.
CREATE TABLE IF NOT EXISTS msgrefs (
    account_id INTEGER NOT NULL,
    ref_msgid  TEXT NOT NULL,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS msgrefs_ref ON msgrefs(account_id, ref_msgid);
CREATE INDEX IF NOT EXISTS msgrefs_msg ON msgrefs(message_id);

CREATE TABLE IF NOT EXISTS entries (
    folder_id  INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    uid        INTEGER,
    date       INTEGER NOT NULL,
    PRIMARY KEY (folder_id, message_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS entries_list ON entries(folder_id, date DESC);
CREATE UNIQUE INDEX IF NOT EXISTS entries_uid ON entries(folder_id, uid) WHERE uid IS NOT NULL;
CREATE INDEX IF NOT EXISTS entries_msg ON entries(message_id);
"#;

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

/// Header data for one message as fetched from a folder.
#[derive(Debug, Clone)]
pub struct HeaderRecord {
    pub uid: u32,
    pub gm_msgid: Option<u64>,
    /// RFC 8474 EMAILID (servers with OBJECTID), identical for every copy of a message.
    pub email_id: Option<String>,
    pub gm_labels: Option<Vec<String>>,
    pub message_id: Option<String>,
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

/// v2: references used to be split on whitespace, so damaged headers produced junk ids
/// (`org`, `(Your`, encoded-words) that glued unrelated conversations together. Re-parse every
/// message's references from its stored headers (first 64 KB of the raw message; headers come
/// first) and rebuild all conversations. Messages without a body get their stored lists cleaned.
fn rethread(db: &mut Connection, bodies: &Connection) -> Result<()> {
    let started = std::time::Instant::now();
    let rows: Vec<(i64, Option<String>, Option<String>, Option<String>, bool)> = {
        let mut stmt = db.prepare("SELECT id, message_id, in_reply_to, refs, has_body FROM messages")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    if rows.is_empty() {
        return Ok(());
    }
    let mut head = bodies.prepare("SELECT substr(raw, 1, 65536) FROM bodies WHERE message_id = ?1")?;
    let tx = db.transaction()?;
    {
        let mut update =
            tx.prepare("UPDATE messages SET message_id = ?2, in_reply_to = ?3, refs = ?4 WHERE id = ?1")?;
        let clean = |s: &Option<String>| -> Option<String> {
            s.as_deref().map(crate::mime::clean_stored_ids).filter(|v| !v.is_empty()).map(|v| v.join(" "))
        };
        for (id, mid, irt, refs, has_body) in &rows {
            let raw: Option<Vec<u8>> =
                if *has_body { head.query_row([id], |r| r.get(0)).optional()? } else { None };
            let (mid, irt, refs) = match raw {
                Some(raw) => {
                    let p = crate::mime::parse_header_block(&raw);
                    (p.message_id.or_else(|| mid.clone()), p.in_reply_to, p.refs)
                }
                None => (mid.clone(), clean(irt), clean(refs)),
            };
            update.execute(params![id, mid, irt, refs])?;
        }
        tx.execute_batch("DELETE FROM msgrefs; UPDATE messages SET thread_id = NULL;")?;
    }
    tx.commit()?;
    backfill_threads(db)?;
    tracing::info!(n = rows.len(), ms = started.elapsed().as_millis() as u64, "rebuilt conversations from cleaned references");
    Ok(())
}

/// Thread messages stored before threading existed (oldest first, as they arrived).
fn backfill_threads(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let pending: Vec<(i64, i64, Option<String>, Option<String>, Option<String>)> = {
        let mut stmt = tx.prepare(
            "SELECT id, account_id, message_id, in_reply_to, refs FROM messages
             WHERE thread_id IS NULL ORDER BY date, id",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    if pending.is_empty() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    for (id, account_id, mid, irt, refs) in &pending {
        assign_thread(&tx, *account_id, *id, mid.as_deref(), irt.as_deref(), refs.as_deref())?;
    }
    tx.commit()?;
    tracing::info!(n = pending.len(), ms = started.elapsed().as_millis() as u64, "threaded existing messages");
    Ok(())
}

/// Make a message's membership in label-backed folders match its Gmail labels exactly.
/// `current` (the folder being synced) is left alone; folders not in `label_folders`
/// (All Mail, Trash, Spam) are never touched.
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
    let wanted: std::collections::HashSet<i64> =
        labels.iter().filter_map(|l| label_folders.get(l).copied()).filter(|&f| f != current).collect();
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
pub struct Store {
    db: Mutex<Connection>,
    read: Mutex<Connection>,
    bodies: Mutex<Connection>,
    bodies_read: Mutex<Connection>,
}

// Schema/data versions (PRAGMA user_version): 1 = Gmail label resync, 2 = conversations
// rebuilt from cleaned references.

fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;
         PRAGMA temp_store=MEMORY; PRAGMA cache_size=-65536;",
    )?;
    Ok(conn)
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        let mut db = open(&dir.join("mail.db"))?;
        db.execute_batch(SCHEMA)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            // v1: Gmail system labels were mis-parsed, dropping label-derived memberships.
            db.execute_batch(
                "UPDATE folders SET top_uid=0, low_uid=0, complete=0, highest_modseq=0;
                 PRAGMA user_version=1;",
            )?;
        }
        for (column, ddl) in [
            ("omitted", "ALTER TABLE messages ADD COLUMN omitted TEXT"),
            ("allow_remote", "ALTER TABLE messages ADD COLUMN allow_remote INTEGER NOT NULL DEFAULT 0"),
            ("thread_id", "ALTER TABLE messages ADD COLUMN thread_id INTEGER"),
            ("identity", "ALTER TABLE messages ADD COLUMN identity TEXT"),
        ] {
            let exists = db
                .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name=?1")?
                .exists([column])?;
            if !exists {
                db.execute_batch(ddl)?;
            }
        }
        let entries_has_thread = db
            .prepare("SELECT 1 FROM pragma_table_info('entries') WHERE name='thread_id'")?
            .exists([])?;
        if !entries_has_thread {
            db.execute_batch(
                "ALTER TABLE entries ADD COLUMN thread_id INTEGER;
                 UPDATE entries SET thread_id=(SELECT thread_id FROM messages m WHERE m.id=entries.message_id);",
            )?;
        }
        // entries.thread_id mirrors messages.thread_id so a folder's conversations can be listed
        // from one covering index; triggers keep the copy exact.
        db.execute_batch(
            "CREATE INDEX IF NOT EXISTS messages_thread ON messages(thread_id, date);
             CREATE INDEX IF NOT EXISTS messages_msgid ON messages(account_id, message_id);
             CREATE UNIQUE INDEX IF NOT EXISTS messages_identity ON messages(account_id, identity)
                 WHERE identity IS NOT NULL;
             CREATE INDEX IF NOT EXISTS entries_thread ON entries(folder_id, thread_id, date);
             CREATE TRIGGER IF NOT EXISTS entries_thread_ins AFTER INSERT ON entries BEGIN
                 UPDATE entries SET thread_id=(SELECT thread_id FROM messages WHERE id=NEW.message_id)
                 WHERE folder_id=NEW.folder_id AND message_id=NEW.message_id;
             END;
             CREATE TRIGGER IF NOT EXISTS messages_thread_upd AFTER UPDATE OF thread_id ON messages BEGIN
                 UPDATE entries SET thread_id=NEW.thread_id WHERE message_id=NEW.id;
             END;",
        )?;
        backfill_threads(&mut db)?;
        // Quick search index over sender, recipients and subject. Contentless: it stores only
        // the index, rows are joined back to `messages` by rowid. Triggers keep it current.
        db.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS msg_fts USING fts5(
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
             END;",
        )?;
        let indexed: bool = db.prepare("SELECT 1 FROM msg_fts LIMIT 1")?.exists([])?;
        if !indexed {
            let started = std::time::Instant::now();
            let n = db.execute(
                "INSERT INTO msg_fts(rowid, subject, sender, recipients)
                 SELECT id, subject, from_name || ' ' || from_addr, to_addrs FROM messages",
                [],
            )?;
            if n > 0 {
                tracing::info!(n, ms = started.elapsed().as_millis() as u64, "built search index");
            }
        }
        let bodies = open(&dir.join("bodies.db"))?;
        bodies.execute_batch(BODY_SCHEMA)?;
        bodies.execute_batch(ATTACHMENT_SCHEMA)?;
        bodies.execute(
            "DELETE FROM stash WHERE created < unixepoch() - ?1",
            [STASH_TTL_SECS],
        )?;
        if version < 2 {
            rethread(&mut db, &bodies)?;
            db.execute_batch("PRAGMA user_version=2;")?;
        }
        Ok(Store {
            db: Mutex::new(db),
            read: Mutex::new(open(&dir.join("mail.db"))?),
            bodies: Mutex::new(bodies),
            bodies_read: Mutex::new(open(&dir.join("bodies.db"))?),
        })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
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
        Ok(Account {
            id,
            email: email.into(),
            display_name: display_name.into(),
            provider,
            imap_host: host.into(),
            imap_port: port,
        })
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, email, display_name, provider, imap_host, imap_port FROM accounts ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Account {
                id: r.get(0)?,
                email: r.get(1)?,
                display_name: r.get(2)?,
                provider: Provider::parse(&r.get::<_, String>(3)?),
                imap_host: r.get(4)?,
                imap_port: r.get(5)?,
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
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("UPDATE folders SET present=0 WHERE account_id=?1", [account_id])?;
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
        tx.execute("DELETE FROM folders WHERE account_id=?1 AND present=0", [account_id])?;
        tx.commit()?;
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
        let mut changed = std::collections::BTreeSet::from([folder_id]);
        // New rows whose body may be waiting in the stash (moved between folders).
        let mut restore: Vec<(i64, String)> = Vec::new();
        let mut db = self.db();
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
                    from_name, from_addr, to_addrs, date, size, flags, has_attachments, identity)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?;
            let mut update_flags = tx.prepare_cached("UPDATE messages SET flags=?2 WHERE id=?1")?;
            let mut entry = tx.prepare_cached(
                "INSERT INTO entries (folder_id, message_id, uid, date) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(folder_id, message_id) DO UPDATE SET uid=coalesce(excluded.uid, entries.uid)",
            )?;
            for rec in records {
                let mut identity = message_identity(rec);
                let existing: Option<i64> = match rec.gm_msgid {
                    Some(gm) => by_gm.query_row(params![account_id, gm as i64], |r| r.get(0)).optional()?,
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
                let id = match existing {
                    Some(id) => {
                        update_flags.execute(params![id, rec.flags])?;
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
        changes: &[(u32, i64, Option<Vec<String>>)],
        label_folders: &std::collections::HashMap<String, i64>,
    ) -> Result<Vec<i64>> {
        let mut changed = std::collections::BTreeSet::from([folder_id]);
        let mut db = self.db();
        let tx = db.transaction()?;
        {
            let mut find = tx.prepare_cached(
                "SELECT e.message_id, e.date FROM entries e WHERE e.folder_id=?1 AND e.uid=?2",
            )?;
            let mut set = tx.prepare_cached("UPDATE messages SET flags=?2 WHERE id=?1")?;
            for (uid, flags, labels) in changes {
                let Some((id, date)): Option<(i64, i64)> =
                    find.query_row(params![folder_id, uid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
                else {
                    continue;
                };
                set.execute(params![id, flags])?;
                if let Some(labels) = labels {
                    apply_gmail_labels(&tx, id, folder_id, date, labels, label_folders, &mut changed)?;
                }
            }
        }
        tx.commit()?;
        Ok(changed.into_iter().collect())
    }

    /// Remove entries for UIDs that no longer exist on the server, then drop orphaned messages.
    pub fn remove_uids(&self, folder_id: i64, uids: &[u32]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let mut orphans = Vec::new();
        {
            let mut find =
                tx.prepare_cached("SELECT message_id FROM entries WHERE folder_id=?1 AND uid=?2")?;
            let mut del = tx.prepare_cached("DELETE FROM entries WHERE folder_id=?1 AND uid=?2")?;
            let mut has_uid_entry = tx.prepare_cached(
                "SELECT 1 FROM entries WHERE message_id=?1 AND uid IS NOT NULL LIMIT 1",
            )?;
            let mut del_msg = tx.prepare_cached("DELETE FROM messages WHERE id=?1")?;
            let mut meta = tx.prepare_cached(
                "SELECT account_id, identity, has_body, snippet, has_attachments, omitted, allow_remote
                 FROM messages WHERE id=?1",
            )?;
            for uid in uids {
                let Some(id): Option<i64> =
                    find.query_row(params![folder_id, uid], |r| r.get(0)).optional()?
                else {
                    continue;
                };
                del.execute(params![folder_id, uid])?;
                // Message still physically present in some other folder?
                if has_uid_entry.query_row([id], |_| Ok(())).optional()?.is_none() {
                    let m: (i64, Option<String>, bool, Option<String>, bool, Option<String>, bool) =
                        meta.query_row([id], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
                        })?;
                    del_msg.execute([id])?;
                    orphans.push((id, m));
                }
            }
        }
        tx.commit()?;
        drop(db);
        if !orphans.is_empty() {
            let bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
            let tx = bodies.unchecked_transaction()?;
            for (id, (account_id, identity, has_body, snippet, att, omitted, remote)) in orphans {
                // Possibly moved to a folder we haven't seen yet: keep the body for a while.
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
        }
        Ok(())
    }

    pub fn list_messages(&self, folder_id: i64, offset: u32, limit: u32) -> Result<MessagePage> {
        let db = self.rdb();
        let total: u32 =
            db.query_row("SELECT count(*) FROM entries WHERE folder_id=?1", [folder_id], |r| r.get(0))?;
        let mut stmt = db.prepare_cached(
            "SELECT m.id, m.subject, m.from_name, m.from_addr, m.date, m.snippet, m.flags,
                    m.has_attachments, m.has_body
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
        let mut members = db.prepare_cached(&format!(
            "SELECT m.id, m.subject, m.from_name, m.from_addr, m.flags, m.has_attachments, m.snippet, m.message_id
             FROM messages m WHERE m.thread_id=?1 AND {SCOPE_FILTER} ORDER BY m.date, m.id"
        ))?;
        let mut threads = Vec::with_capacity(page.len());
        for (thread_id, date) in page {
            #[allow(clippy::type_complexity)]
            let rows: Vec<(i64, String, String, String, i64, bool, Option<String>, Option<String>)> = members
                .query_map(params![thread_id, folder_id, scope.all_folders, scope.include_trash], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
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
                            WHERE e.message_id = m.id AND f.role IN ('trash', 'junk'))
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
        let Some(fts) = crate::search::to_fts5(query, fields) else {
            return Ok(MessagePage { total: 0, messages: vec![] });
        };
        let db = self.rdb();
        let scope = "msg_fts MATCH ?1 AND m.account_id = ?2
             AND (?3 IS NULL OR EXISTS (SELECT 1 FROM entries e WHERE e.message_id = m.id AND e.folder_id = ?3))";
        let total: u32 = db.query_row(
            &format!("SELECT count(*) FROM msg_fts JOIN messages m ON m.id = msg_fts.rowid WHERE {scope}"),
            params![fts, account_id, folder_id],
            |r| r.get(0),
        )?;
        // Report each hit in its most specific folder: Inbox, then labels/folders, then
        // Trash/Spam, and Gmail's All Mail last.
        let mut stmt = db.prepare_cached(&format!(
            "SELECT m.id, coalesce(?3, (SELECT e.folder_id FROM entries e JOIN folders f ON f.id = e.folder_id
                        WHERE e.message_id = m.id
                        ORDER BY CASE f.role WHEN 'inbox' THEN 0 WHEN 'all' THEN 3
                                 WHEN 'trash' THEN 2 WHEN 'junk' THEN 2 ELSE 1 END LIMIT 1), 0),
                    m.subject, m.from_name, m.from_addr, m.date, m.snippet, m.flags,
                    m.has_attachments, m.has_body
             FROM msg_fts JOIN messages m ON m.id = msg_fts.rowid
             WHERE {scope} ORDER BY m.date DESC, m.id DESC LIMIT ?4 OFFSET ?5"
        ))?;
        let rows = stmt.query_map(params![fts, account_id, folder_id, limit, offset], |r| {
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
            })
        })?;
        Ok(MessagePage { total, messages: rows.collect::<rusqlite::Result<_>>()? })
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

    /// Newest messages still missing a body, preferring `focus_folder`.
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
        Ok(out)
    }

    pub fn count_missing_bodies(&self, account_id: i64) -> Result<(u64, u64)> {
        Ok(self.rdb().query_row(
            "SELECT count(*), sum(has_body) FROM messages WHERE account_id=?1",
            [account_id],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64)),
        )?)
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
        }
        self.db().execute(
            "UPDATE messages SET has_body=1, snippet=?2, has_attachments=?3, omitted=?4 WHERE id=?1",
            params![id, snippet, has_attachments, omitted],
        )?;
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
            uid,
            gm_msgid: Some(gm),
            email_id: None,
            gm_labels: Some(labels.iter().map(|s| s.to_string()).collect()),
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

    /// Rethread a copy of a real mail.db against a real bodies.db (opened read-only):
    /// MAIL_DB_COPY=/copy/mail.db BODIES_DB=/live/bodies.db cargo test -p mail-core real_rethread -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_rethread() {
        let mut db = Connection::open(std::env::var("MAIL_DB_COPY").unwrap()).unwrap();
        let bodies = Connection::open_with_flags(
            std::env::var("BODIES_DB").unwrap(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let stats = |db: &Connection| -> (i64, Vec<i64>) {
            let junk: i64 = db.query_row("SELECT count(*) FROM msgrefs WHERE instr(ref_msgid,'@')=0", [], |r| r.get(0)).unwrap();
            let mut stmt = db.prepare("SELECT count(*) c FROM messages GROUP BY thread_id ORDER BY c DESC LIMIT 5").unwrap();
            (junk, stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect())
        };
        println!("before: junk refs {:?}", stats(&db));
        let t = std::time::Instant::now();
        rethread(&mut db, &bodies).unwrap();
        println!("rethread took {:?}", t.elapsed());
        println!("after:  junk refs {:?}", stats(&db));
        let threads: i64 = db.query_row("SELECT count(DISTINCT thread_id) FROM messages", [], |r| r.get(0)).unwrap();
        println!("conversations: {threads}");
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
        store.update_flags(all, &[(900, flags::SEEN, Some(vec![]))], &labels).unwrap();
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
}
