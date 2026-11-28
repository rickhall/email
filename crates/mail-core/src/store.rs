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
use crate::sort::{ListFilter, ListGroup, ListSort};

/// The entries of folder `?1` a conversation list is made of: those `filter` lets through
/// (`e`, joined with their messages `m` when it needs them). Ends in a WHERE, so more
/// conditions can follow with AND.
fn thread_rows_from(filter: &ListFilter) -> String {
    let f = filter.sql(true);
    if f.is_empty() {
        "entries e WHERE e.folder_id=?1".to_string()
    } else {
        format!("entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id=?1{f}")
    }
}

/// A message as listed with its folder: id, folder, subject, from_name, from_addr, date, snippet,
/// flags, has_attachments, has_body, keywords, (outgoing failed?), draft_id, size, to_addrs.
fn placed_summary(r: &rusqlite::Row) -> rusqlite::Result<MessageSummary> {
    let flags: i64 = r.get(7)?;
    let keywords: String = r.get(10)?;
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
        tags: crate::tags::visible(&keywords),
        answered: flags & flags::ANSWERED != 0,
        forwarded: crate::tags::is_forwarded(&keywords),
        snoozed_until: crate::tags::snoozed_until(&keywords),
        sending: r.get::<_, Option<bool>>(11)? == Some(false),
        send_failed: r.get::<_, Option<bool>>(11)? == Some(true),
        draft_id: r.get(12)?,
        size: r.get(13)?,
        to: crate::contacts::parse_list(&r.get::<_, String>(14)?),
    })
}

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
    mark_color   TEXT,                  -- its marker's colour (a name from the app's palette); NULL: automatic
    graph        INTEGER                -- Microsoft: tags and snooze through Graph (1), allowed by the user
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
    attachments_stored INTEGER,
    body_indexed    INTEGER NOT NULL DEFAULT 0 -- its text is in body_fts (0: still to do, or a new body)
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

-- Signatures and each account's default ones (see crate::signatures).
CREATE TABLE IF NOT EXISTS signatures (
    id       INTEGER PRIMARY KEY,
    name     TEXT NOT NULL,
    html     TEXT NOT NULL,
    position INTEGER NOT NULL
);
-- Other addresses an account sends as (see crate::identities).
CREATE TABLE IF NOT EXISTS identities (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email      TEXT NOT NULL,
    name       TEXT NOT NULL,
    new_sig    INTEGER REFERENCES signatures(id) ON DELETE SET NULL,
    reply_sig  INTEGER REFERENCES signatures(id) ON DELETE SET NULL,
    is_default INTEGER NOT NULL DEFAULT 0,
    -- None chosen (no signature) rather than the account's, for new messages / replies
    new_none   INTEGER NOT NULL DEFAULT 0,
    reply_none INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS signature_defaults (
    account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    new_id     INTEGER REFERENCES signatures(id) ON DELETE SET NULL,
    reply_id   INTEGER REFERENCES signatures(id) ON DELETE SET NULL
);

-- The address book (see crate::addressbook); emails and phones as JSON arrays.
CREATE TABLE IF NOT EXISTS address_book (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL,
    emails  TEXT NOT NULL,
    phones  TEXT NOT NULL,
    company TEXT NOT NULL,
    notes   TEXT NOT NULL,
    updated INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Google Contacts linked to an account (crate::gcontacts): who signed in, where the last sync
-- left off, when, and what went wrong.
CREATE TABLE IF NOT EXISTS google_links (
    account_id   INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    google_email TEXT NOT NULL,
    sync_token   TEXT,
    synced       INTEGER,
    error        TEXT
);
-- An address book card that is a Google contact: its resource (NULL: still to be created),
-- etag, the person as last seen (fields the app doesn't edit are kept from it), and whether it
-- was changed here since (to send).
CREATE TABLE IF NOT EXISTS card_links (
    card_id    INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    resource   TEXT,
    etag       TEXT,
    raw        TEXT,
    dirty      INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS card_links_resource ON card_links(account_id, resource) WHERE resource IS NOT NULL;
-- A Google contact's picture, fetched when the contact is shown (from `url`; a new url there:
-- fetched again).
CREATE TABLE IF NOT EXISTS card_photos (
    card_id INTEGER PRIMARY KEY,
    url     TEXT NOT NULL,
    mime    TEXT NOT NULL,
    data    BLOB NOT NULL
);
-- A linked account's groups in Google Contacts (crate::gcontacts::ContactGroup), in the order
-- shown; replaced by every sync. A card's own groups are in its person (card_links.raw).
CREATE TABLE IF NOT EXISTS contact_groups (
    account_id INTEGER NOT NULL,
    position   INTEGER NOT NULL,
    resource   TEXT NOT NULL,
    name       TEXT NOT NULL,
    PRIMARY KEY (account_id, resource)
);
-- Google contacts deleted here, still to delete there.
CREATE TABLE IF NOT EXISTS card_tombstones (
    account_id INTEGER NOT NULL,
    resource   TEXT NOT NULL,
    PRIMARY KEY (account_id, resource)
);

-- A picture of the user's own for an account's marker (a data: URL), instead of the letter.
CREATE TABLE IF NOT EXISTS account_logos (
    account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    data_url   TEXT NOT NULL
);

-- Senders whose remote images always load ("Always load images from …"), lowercase addresses,
-- or "@domain" for everyone there (and at its subdomains).
CREATE TABLE IF NOT EXISTS remote_senders (
    addr  TEXT PRIMARY KEY,
    added INTEGER NOT NULL DEFAULT (unixepoch())
);

-- The user's yes/no choices the core acts on, "1" or "0" (missing: the default): "domain_rules",
-- "Always load…" can cover everyone at a domain; "http_pictures", a picture that only loads
-- over plain http may be fetched that way (`Core::http_pictures`).
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Sender pictures by domain (crate::icons): mime + data, or both NULL for "has none"; when.
CREATE TABLE IF NOT EXISTS sender_icons (
    domain  TEXT PRIMARY KEY,
    mime    TEXT,
    data    BLOB,
    fetched INTEGER NOT NULL
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

-- The text of messages whose body is here (rowid = message id), for search. Bodies are in
-- bodies.db, so it's filled from Rust, in the background (`Store::index_bodies`), not by a
-- trigger; a message whose body comes or changes gets body_indexed=0 and is done again.
CREATE VIRTUAL TABLE IF NOT EXISTS body_fts USING fts5(
    body,
    content='', contentless_delete=1,
    tokenize='unicode61 remove_diacritics 2', prefix='2 3');
CREATE TRIGGER IF NOT EXISTS body_fts_del AFTER DELETE ON messages BEGIN
    DELETE FROM body_fts WHERE rowid = OLD.id;
END;
CREATE INDEX IF NOT EXISTS messages_unindexed ON messages(id) WHERE has_body=1 AND body_indexed=0;
-- The names of a message's attachments and the text in them (PDFs, plain-text files), from
-- the same background indexing: search's Attachments field.
CREATE VIRTUAL TABLE IF NOT EXISTS file_fts USING fts5(
    files,
    content='', contentless_delete=1,
    tokenize='unicode61 remove_diacritics 2', prefix='2 3');
CREATE TRIGGER IF NOT EXISTS file_fts_del AFTER DELETE ON messages BEGIN
    DELETE FROM file_fts WHERE rowid = OLD.id;
END;

-- The highest message id used yet: a new message's id counts on from it (`NEXT_MESSAGE_ID`), so
-- no id is used twice. (SQLite would reuse the newest one's id once it's deleted: a sent
-- message taking its draft's, and the app, which remembers bodies by id, showing the draft.)
-- What AUTOINCREMENT does, without rebuilding the table.
CREATE TABLE IF NOT EXISTS message_id_floor (id INTEGER NOT NULL);
INSERT INTO message_id_floor (id)
    SELECT coalesce(max(id), 0) FROM messages WHERE NOT EXISTS (SELECT 1 FROM message_id_floor);
CREATE TRIGGER IF NOT EXISTS messages_id_floor AFTER INSERT ON messages BEGIN
    UPDATE message_id_floor SET id = NEW.id WHERE id < NEW.id;
END;
"#;

/// A new message's id, in an INSERT: past every id used before (see `message_id_floor`).
const NEXT_MESSAGE_ID: &str = "(SELECT id + 1 FROM message_id_floor)";

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
/// A change that only adds tables (or indexes) needs no step: `SCHEMA` (all `IF NOT EXISTS`) runs
/// at every open and adds them, and the version stays, so an older build still opens the
/// database (it ignores tables it doesn't know). Signatures (`signatures`,
/// `signature_defaults`) and the address book (`address_book`) came that way.
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
    // 12: Microsoft accounts can keep tags and snooze through Graph
    |db| Ok(db.execute_batch("ALTER TABLE accounts ADD COLUMN graph INTEGER;")?),
    // 13: search in the text of messages (body_fts, filled by `index_bodies`)
    |db| Ok(db.execute_batch("ALTER TABLE messages ADD COLUMN body_indexed INTEGER NOT NULL DEFAULT 0;")?),
    // 14: settings; rules for a whole domain became opt-in, on for those who already had one
    |db| {
        db.execute_batch("CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        // `remote_senders` came by `SCHEMA`, which runs after the steps: a database from before
        // it has no such table yet (and so no rules to keep on).
        if table_exists(db, "remote_senders")? {
            db.execute_batch(
                "INSERT OR IGNORE INTO settings (key, value)
                   SELECT 'domain_rules', '1' WHERE EXISTS (SELECT 1 FROM remote_senders WHERE addr LIKE '@%');",
            )?;
        }
        Ok(())
    },
    // 15: another address can have no signature (None), not only its own or the account's
    |db| {
        // (No table yet: `SCHEMA` makes it with them.)
        let cols: Vec<String> = db.prepare("SELECT name FROM pragma_table_info('identities')")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for col in ["new_none", "reply_none"] {
            if !cols.is_empty() && !cols.iter().any(|c| c == col) {
                db.execute_batch(&format!("ALTER TABLE identities ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0;"))?;
            }
        }
        Ok(())
    },
];
const SCHEMA_VERSION: i64 = BASE_VERSION + MIGRATIONS.len() as i64;

/// Whether the database has the table `name`. A step that reads a table `SCHEMA` added (rather
/// than a step) must check: `SCHEMA` runs after the steps, so an older database may not have it.
fn table_exists(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")?.exists([name])?)
}

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
#[derive(Debug, Clone, Default)]
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
/// A message's keywords as its server reports them (`server`), to store: on an account whose
/// tags and snooze go through Graph, those it has now stay (the server has none of them).
fn server_keywords(db: &Connection, message_id: i64, server: &[String]) -> Result<String> {
    let (stored, graph): (String, bool) = db
        .prepare_cached("SELECT m.keywords, coalesce(a.graph, 0) FROM messages m JOIN accounts a ON a.id = m.account_id WHERE m.id=?1")?
        .query_row([message_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .unwrap_or_default();
    if !graph {
        return Ok(crate::tags::join(server));
    }
    let mut all: Vec<String> = crate::tags::parse(&stored).into_iter().filter(|k| crate::tags::kept_by_graph(k)).collect();
    all.extend(server.iter().filter(|k| !crate::tags::kept_by_graph(k)).cloned());
    Ok(crate::tags::join(&all))
}

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
type Member = (i64, String, String, String, i64, bool, Option<String>, Option<String>, String, Option<bool>, String);

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

/// Save a card (new when its id is 0) on `db`; returns its id.
fn write_card(db: &Connection, card: crate::addressbook::Card) -> Result<i64> {
    let c = card.cleaned();
    if c.title().is_empty() {
        anyhow::bail!("Give the contact a name, a company or an address.");
    }
    let (emails, phones) = (serde_json::to_string(&c.emails)?, serde_json::to_string(&c.phones)?);
    if c.id == 0 {
        db.execute(
            "INSERT INTO address_book (name, emails, phones, company, notes) VALUES (?1,?2,?3,?4,?5)",
            params![c.name, emails, phones, c.company, c.notes],
        )?;
        return Ok(db.last_insert_rowid());
    }
    let n = db.execute(
        "UPDATE address_book SET name=?2, emails=?3, phones=?4, company=?5, notes=?6, updated=unixepoch() WHERE id=?1",
        params![c.id, c.name, emails, phones, c.company, c.notes],
    )?;
    if n == 0 {
        anyhow::bail!("That contact no longer exists.");
    }
    Ok(c.id)
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
    // Sort keys for lists ordered by sender / recipient (see `crate::sort`).
    crate::sort::register(&conn)?;
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
        // Mail, the log and the rest are the user's alone: the folder opens to no one else on a
        // shared computer, whatever the home folder allows (Windows: a profile's own folders
        // already are).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("making {} private", dir.display()))?;
        }
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
        let (smtp_host, smtp_port, auth, keywords, mark_letter, mark_color, graph) = db.query_row(
            "SELECT smtp_host, smtp_port, auth, keywords, mark_letter, mark_color, coalesce(graph, 0) FROM accounts WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, AuthKind::parse(&r.get::<_, String>(2)?), r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
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
            graph,
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
                    mark_letter, mark_color, coalesce(graph, 0)
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
                graph: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Give back the space of data deleted in bulk (an account removed): both databases rebuilt
    /// (VACUUM), with their temporary copies on disk. Slow on a large mailbox.
    pub fn compact(&self) -> Result<()> {
        let sql = "PRAGMA temp_store=FILE; VACUUM; PRAGMA temp_store=MEMORY; PRAGMA wal_checkpoint(TRUNCATE);";
        self.bodies.lock().unwrap_or_else(|e| e.into_inner()).execute_batch(sql)?;
        self.db().execute_batch(sql)?;
        Ok(())
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
            let mut insert = tx.prepare_cached(&format!(
                "INSERT INTO messages (id, account_id, gm_msgid, message_id, in_reply_to, refs, subject,
                    from_name, from_addr, to_addrs, date, size, flags, has_attachments, identity, keywords)
                 VALUES ({NEXT_MESSAGE_ID},?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"
            ))?;
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
                        update_flags.execute(params![id, rec.flags, server_keywords(&tx, id, &rec.keywords)?])?;
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
                set.execute(params![id, c.flags, server_keywords(&tx, id, &c.keywords)?])?;
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
        self.list_messages_by(folder_id, ListSort::Received, &ListFilter::default(), offset, limit)
    }

    /// A page of the folder's messages in `sort`'s order: newest first, or by sender /
    /// recipient (`crate::sort`) and newest first within each.
    pub fn list_messages_by(&self, folder_id: i64, sort: ListSort, filter: &ListFilter, offset: u32, limit: u32) -> Result<MessagePage> {
        let db = self.rdb();
        let f = filter.sql(false);
        let total: u32 = if f.is_empty() {
            db.query_row("SELECT count(*) FROM entries WHERE folder_id=?1", [folder_id], |r| r.get(0))?
        } else {
            db.query_row(
                &format!("SELECT count(*) FROM entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id=?1{f}"),
                [folder_id],
                |r| r.get(0),
            )?
        };
        let order = match sort.key_sql() {
            None => "e.date DESC, e.message_id DESC".to_string(),
            Some(k) => format!("{k}, e.date DESC, e.message_id DESC"),
        };
        let mut stmt = db.prepare_cached(&format!(
            "SELECT m.id, m.subject, m.from_name, m.from_addr, m.date, m.snippet, m.flags,
                    m.has_attachments, m.has_body, m.keywords,
                    (SELECT o.failed IS NOT NULL FROM outgoing o WHERE o.id = m.pending_send), m.draft_id, m.size, m.to_addrs
             FROM entries e JOIN messages m ON m.id = e.message_id
             WHERE e.folder_id=?1{f} ORDER BY {order} LIMIT ?2 OFFSET ?3"
        ))?;
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
                size: r.get(12)?,
                to: crate::contacts::parse_list(&r.get::<_, String>(13)?),
            })
        })?;
        Ok(MessagePage { total, messages: rows.collect::<rusqlite::Result<_>>()? })
    }

    /// Conversations with at least one message in `folder_id`, newest first.
    pub fn list_threads(&self, folder_id: i64, scope: ThreadScope, offset: u32, limit: u32) -> Result<ThreadPage> {
        self.list_threads_filtered(folder_id, scope, &ListFilter::default(), offset, limit)
    }

    /// [`Store::list_threads`], only conversations with a message here that `filter` lets
    /// through (placed by those messages' newest date).
    pub fn list_threads_filtered(&self, folder_id: i64, scope: ThreadScope, filter: &ListFilter, offset: u32, limit: u32) -> Result<ThreadPage> {
        let db = self.rdb();
        let from = thread_rows_from(filter);
        let total: u32 = db.query_row(
            &format!("SELECT count(DISTINCT e.thread_id) FROM {from}"),
            [folder_id],
            |r| r.get(0),
        )?;
        let page: Vec<(i64, i64)> = {
            let mut stmt = db.prepare_cached(&format!(
                "SELECT e.thread_id, max(e.date) AS d FROM {from}
                 GROUP BY e.thread_id ORDER BY d DESC, e.thread_id DESC LIMIT ?2 OFFSET ?3"
            ))?;
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
                    m.keywords, (SELECT o.failed IS NOT NULL FROM outgoing o WHERE o.id = m.pending_send), m.to_addrs
             FROM messages m WHERE m.thread_id=?1 AND {SCOPE_FILTER} ORDER BY m.date, m.id"
        ))?;
        let mut threads = Vec::with_capacity(page.len());
        for (thread_id, date) in page {
            let rows: Vec<Member> = members
                .query_map(params![thread_id, folder_id, scope.all_folders, scope.include_trash], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?))
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
                from_addr: incoming.map(|i| rows[i].3.clone()).unwrap_or_else(|| latest.3.clone()),
                to: crate::contacts::parse_list(&latest.10),
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
                    (SELECT o.failed IS NOT NULL FROM outgoing o WHERE o.id = m.pending_send), m.draft_id, m.size, m.to_addrs
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
                    size: r.get(17)?,
                    to: crate::contacts::parse_list(&r.get::<_, String>(18)?),
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

    /// A search's condition on `messages m` and its parameters: ?1 account, ?2 folder, ?3 limit,
    /// ?4 offset, then each term's expressions and each tag. None when it finds nothing.
    #[allow(clippy::type_complexity)]
    fn search_scope(
        &self,
        account_id: i64,
        folder_id: Option<i64>,
        query: &str,
        fields: &[&str],
        limit: u32,
        offset: u32,
    ) -> Result<Option<(String, Vec<Box<dyn rusqlite::ToSql>>)>> {
        // `tag:work` / `tag:"to do"` / `tag:$label2` terms filter by keyword; the rest is FTS.
        let (rest, tag_terms) = crate::search::split_tags(query);
        let terms = crate::search::terms(&rest, fields);
        if terms.is_none() && tag_terms.is_empty() {
            return Ok(None);
        }
        let keywords: Vec<String> = tag_terms.iter().map(|t| self.tag_keyword(t)).collect::<Result<_>>()?;
        let mut args: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(account_id), Box::new(folder_id), Box::new(limit), Box::new(offset)];
        let mut scope = String::from(
            "m.account_id = ?1
             AND (?2 IS NULL OR EXISTS (SELECT 1 FROM entries e WHERE e.message_id = m.id AND e.folder_id = ?2))
             AND EXISTS (SELECT 1 FROM entries e WHERE e.message_id = m.id)",
        );
        // A term matches in the header index or in the text: the union of the two.
        for t in terms.into_iter().flatten() {
            let mut sets = Vec::new();
            for (table, expr) in [("msg_fts", t.head), ("body_fts", t.body), ("file_fts", t.files)] {
                if let Some(expr) = expr {
                    args.push(Box::new(expr));
                    sets.push(format!("SELECT rowid FROM {table} WHERE {table} MATCH ?{}", args.len()));
                }
            }
            if !sets.is_empty() {
                scope.push_str(&format!(" AND m.id {}IN ({})", if t.negated { "NOT " } else { "" }, sets.join(" UNION ")));
            }
        }
        for k in &keywords {
            args.push(Box::new(format!("% {} %", k.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))));
            scope.push_str(&format!(" AND m.keywords LIKE ?{} ESCAPE '\\'", args.len()));
        }
        Ok(Some((scope, args)))
    }

    /// Where message `id` is among a search's results (newest first), or None if it isn't one:
    /// so a list can keep it selected before its page has loaded.
    pub fn search_index_of(&self, account_id: i64, folder_id: Option<i64>, query: &str, fields: &[&str], id: i64) -> Result<Option<u32>> {
        let Some((scope, mut args)) = self.search_scope(account_id, folder_id, query, fields, 0, 0)? else { return Ok(None) };
        args.push(Box::new(id));
        let n = args.len();
        // ?3/?4 unused here but kept, so the numbering is the same as the search's.
        let sql = format!(
            "SELECT (SELECT count(*) FROM messages m WHERE {scope}
                       AND (m.date > t.date OR (m.date = t.date AND m.id > t.id)))
             FROM messages t WHERE t.id = ?{n} AND ?3 IS NOT NULL AND ?4 IS NOT NULL
               AND EXISTS (SELECT 1 FROM messages m WHERE {scope} AND m.id = ?{n})"
        );
        let db = self.rdb();
        Ok(db.query_row(&sql, rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())), |r| r.get(0)).optional()?)
    }

    /// Quick search over sender, recipients, subject and the text of messages whose body is
    /// here (see `crate::search`), newest first.
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
        let Some((scope, args)) = self.search_scope(account_id, folder_id, query, fields, limit, offset)? else {
            return Ok(MessagePage { total: 0, messages: vec![] });
        };
        let db = self.rdb();
        let from = "messages m";
        let total: u32 = {
            // Same numbering as the page query; ?3/?4 are unused here but keep positions aligned.
            let sql = format!("SELECT count(*) FROM {from} WHERE {scope} AND ?3 IS NOT NULL AND ?4 IS NOT NULL");
            db.query_row(&sql, rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())), |r| r.get(0))?
        };
        // Report each hit in its most specific folder: Inbox, then labels/folders, then
        // Trash/Spam, and Gmail's All Mail last.
        let mut stmt = db.prepare_cached(&format!(
            "SELECT m.id, coalesce(?2, (SELECT e.folder_id FROM entries e JOIN folders f ON f.id = e.folder_id
                        WHERE e.message_id = m.id
                        ORDER BY CASE f.role WHEN 'inbox' THEN 0 WHEN 'all' THEN 3
                                 WHEN 'trash' THEN 2 WHEN 'junk' THEN 2 ELSE 1 END LIMIT 1), 0),
                    m.subject, m.from_name, m.from_addr, m.date, m.snippet, m.flags,
                    m.has_attachments, m.has_body, m.keywords,
                    (SELECT o.failed IS NOT NULL FROM outgoing o WHERE o.id = m.pending_send), m.draft_id, m.size, m.to_addrs
             FROM {from} WHERE {scope} ORDER BY m.date DESC, m.id DESC LIMIT ?3 OFFSET ?4"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())), placed_summary)?;
        Ok(MessagePage { total, messages: rows.collect::<rusqlite::Result<_>>()? })
    }

    /// A message's sender: (account, name, address). For one of our own, the newest someone else
    /// in its conversation (if any), as that's whom it's with.
    pub fn sender_of(&self, message_id: i64) -> Result<Option<(i64, String, String)>> {
        let db = self.rdb();
        let Some((account_id, mut name, mut addr, thread_id)): Option<(i64, String, String, Option<i64>)> = db
            .query_row(
                "SELECT account_id, from_name, from_addr, thread_id FROM messages WHERE id=?1",
                [message_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let me: String =
            db.query_row("SELECT email FROM accounts WHERE id=?1", [account_id], |r| r.get(0)).optional()?.unwrap_or_default();
        if addr.eq_ignore_ascii_case(&me) {
            if let Some(t) = thread_id {
                if let Some((n, a)) = db
                    .query_row(
                        "SELECT from_name, from_addr FROM messages WHERE thread_id=?1 AND lower(from_addr) <> lower(?2)
                         ORDER BY date DESC LIMIT 1",
                        params![t, me],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()?
                {
                    (name, addr) = (n, a);
                }
            }
        }
        Ok(Some((account_id, name, addr)))
    }

    /// The messages from `addr` (any case) in an account, newest first, each in its most specific
    /// folder (as search reports them), leaving out what's only in Trash or Spam. Named as on
    /// the newest of them (else `name`).
    pub fn messages_from(&self, account_id: i64, name: &str, addr: &str, offset: u32, limit: u32) -> Result<SenderPage> {
        let db = self.rdb();
        let scope = "m.account_id = ?1 AND lower(m.from_addr) = lower(?2)
             AND EXISTS (SELECT 1 FROM entries e JOIN folders f ON f.id = e.folder_id
                         WHERE e.message_id = m.id AND coalesce(f.role, '') NOT IN ('trash', 'junk'))";
        let total: u32 = db.query_row(&format!("SELECT count(*) FROM messages m WHERE {scope}"), params![account_id, addr], |r| r.get(0))?;
        let newest: Option<String> = db
            .query_row(
                &format!("SELECT m.from_name FROM messages m WHERE {scope} AND m.from_name <> '' ORDER BY m.date DESC LIMIT 1"),
                params![account_id, addr],
                |r| r.get(0),
            )
            .optional()?;
        let mut stmt = db.prepare_cached(&format!(
            "SELECT m.id, coalesce((SELECT e.folder_id FROM entries e JOIN folders f ON f.id = e.folder_id
                        WHERE e.message_id = m.id
                        ORDER BY CASE f.role WHEN 'inbox' THEN 0 WHEN 'all' THEN 3
                                 WHEN 'trash' THEN 4 WHEN 'junk' THEN 4 ELSE 1 END LIMIT 1), 0),
                    m.subject, m.from_name, m.from_addr, m.date, m.snippet, m.flags,
                    m.has_attachments, m.has_body, m.keywords,
                    (SELECT o.failed IS NOT NULL FROM outgoing o WHERE o.id = m.pending_send), m.draft_id, m.size, m.to_addrs
             FROM messages m WHERE {scope} ORDER BY m.date DESC, m.id DESC LIMIT ?3 OFFSET ?4"
        ))?;
        let messages = stmt
            .query_map(params![account_id, addr, limit, offset], placed_summary)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(SenderPage { account_id, name: newest.unwrap_or_else(|| name.to_string()), addr: addr.to_string(), total, messages })
    }

    /// [`Store::messages_from`] the sender of `message_id` ([`Store::sender_of`]).
    pub fn sender_messages(&self, message_id: i64, offset: u32, limit: u32) -> Result<Option<SenderPage>> {
        let Some((account_id, name, addr)) = self.sender_of(message_id)? else { return Ok(None) };
        Ok(Some(self.messages_from(account_id, &name, &addr, offset, limit)?))
    }

    /// Position of a message (`threaded` = false) or conversation (`threaded` = true, `id` is a
    /// thread id) in the folder's list, using exactly the list queries' ordering; None if it
    /// isn't in the folder. Lets the UI open a remembered item and scroll to it at any depth.
    pub fn index_of(&self, folder_id: i64, id: i64, threaded: bool) -> Result<Option<u32>> {
        self.index_of_by(folder_id, id, threaded, ListSort::Received, &ListFilter::default())
    }

    /// [`Store::index_of`] in a list sorted by `sort` (conversations are only ever by date).
    pub fn index_of_by(&self, folder_id: i64, id: i64, threaded: bool, sort: ListSort, filter: &ListFilter) -> Result<Option<u32>> {
        let db = self.rdb();
        let f = filter.sql(threaded);
        if let (false, Some(k)) = (threaded, sort.key_sql()) {
            // list_messages_by: ORDER BY key, e.date DESC, e.message_id DESC
            let at: Option<(rusqlite::types::Value, i64)> = db
                .query_row(
                    &format!(
                        "SELECT {k}, e.date FROM entries e JOIN messages m ON m.id = e.message_id
                         WHERE e.folder_id=?1 AND e.message_id=?2{f}"
                    ),
                    params![folder_id, id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((key, d)) = at else { return Ok(None) };
            return Ok(Some(db.query_row(
                &format!(
                    "SELECT count(*) FROM (SELECT {k} AS k, e.date AS d, e.message_id AS id
                       FROM entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id=?1{f})
                     WHERE k < ?2 OR (k = ?2 AND (d > ?3 OR (d = ?3 AND id > ?4)))"
                ),
                params![folder_id, key, d, id],
                |r| r.get(0),
            )?));
        }
        if threaded {
            let from = thread_rows_from(filter);
            let newest: Option<i64> = db.query_row(
                &format!("SELECT max(e.date) FROM {from} AND e.thread_id=?2"),
                params![folder_id, id],
                |r| r.get(0),
            )?;
            let Some(d) = newest else { return Ok(None) };
            // list_threads: ORDER BY max(date) DESC, thread_id DESC
            Ok(Some(db.query_row(
                &format!(
                    "SELECT count(*) FROM (SELECT e.thread_id AS t, max(e.date) AS d FROM {from}
                     GROUP BY e.thread_id) WHERE d > ?2 OR (d = ?2 AND t > ?3)"
                ),
                params![folder_id, d, id],
                |r| r.get(0),
            )?))
        } else if f.is_empty() {
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
        } else {
            let date: Option<i64> = db
                .query_row(
                    &format!("SELECT e.date FROM entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id=?1 AND e.message_id=?2{f}"),
                    params![folder_id, id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(d) = date else { return Ok(None) };
            Ok(Some(db.query_row(
                &format!(
                    "SELECT count(*) FROM entries e JOIN messages m ON m.id = e.message_id
                     WHERE e.folder_id=?1{f} AND (e.date > ?2 OR (e.date = ?2 AND e.message_id > ?3))"
                ),
                params![folder_id, d, id],
                |r| r.get(0),
            )?))
        }
    }

    /// The first unread item (message, or conversation with an unread message here) below `after`
    /// in the list's order, wrapping to the top; with its index. None if nothing is unread.
    pub fn next_unread(&self, folder_id: i64, after: Option<i64>, threaded: bool, back: bool) -> Result<Option<(i64, u32)>> {
        self.next_unread_by(folder_id, after, threaded, back, ListSort::Received, &ListFilter::default())
    }

    /// [`Store::next_unread`] in a list sorted by `sort` (conversations are only ever by date).
    pub fn next_unread_by(
        &self,
        folder_id: i64,
        after: Option<i64>,
        threaded: bool,
        back: bool,
        sort: ListSort,
        filter: &ListFilter,
    ) -> Result<Option<(i64, u32)>> {
        let sort = if threaded { ListSort::Received } else { sort };
        // Unread ones pass any filter but "read only" (where there are none, but the kept one).
        if filter.read == Some(true) {
            return Ok(None);
        }
        let f = filter.sql(threaded);
        if let Some(k) = sort.key_sql() {
            return self.next_unread_keyed(folder_id, after, back, k, sort, filter);
        }
        let found: Option<i64> = {
            let db = self.rdb();
            // The unread rows with their list sort key (date DESC, id DESC); `?2`/`?3` = position.
            // Conversations: only those holding unread mail get their newest date computed
            // (~50 ms on a 113k-message folder, vs ~220 ms grouping every conversation).
            let rows = if threaded && f.is_empty() {
                "SELECT t.thread_id AS id,
                        (SELECT max(x.date) FROM entries x WHERE x.folder_id = ?1 AND x.thread_id = t.thread_id) AS d
                 FROM (SELECT DISTINCT e.thread_id FROM entries e JOIN messages m ON m.id = e.message_id
                       WHERE e.folder_id = ?1 AND (m.flags & 1) = 0) t"
                    .to_string()
            } else if threaded {
                // Placed by the newest of their messages the filter lets through.
                format!(
                    "SELECT t.thread_id AS id,
                            (SELECT max(e.date) FROM entries e JOIN messages m ON m.id = e.message_id
                             WHERE e.folder_id = ?1 AND e.thread_id = t.thread_id{f}) AS d
                     FROM (SELECT DISTINCT e.thread_id FROM entries e JOIN messages m ON m.id = e.message_id
                           WHERE e.folder_id = ?1 AND (m.flags & 1) = 0{f}) t"
                )
            } else {
                format!(
                    "SELECT e.message_id AS id, e.date AS d
                     FROM entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id = ?1 AND (m.flags & 1) = 0{f}"
                )
            };
            let thread_from = thread_rows_from(filter);
            let pos: Option<i64> = match after {
                Some(id) if threaded => db.query_row(
                    &format!("SELECT max(e.date) FROM {thread_from} AND e.thread_id=?2"),
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
        Ok(self.index_of_by(folder_id, id, threaded, ListSort::Received, filter)?.map(|i| (id, i)))
    }

    /// next_unread in a message list sorted by key `k` (SQL), then newest first.
    fn next_unread_keyed(
        &self,
        folder_id: i64,
        after: Option<i64>,
        back: bool,
        k: &str,
        sort: ListSort,
        filter: &ListFilter,
    ) -> Result<Option<(i64, u32)>> {
        let f = filter.sql(false);
        let found: Option<i64> = {
            let db = self.rdb();
            let rows = format!(
                "SELECT e.message_id AS id, {k} AS k, e.date AS d
                 FROM entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id = ?1 AND (m.flags & 1) = 0{f}"
            );
            let pos: Option<(rusqlite::types::Value, i64)> = match after {
                Some(id) => db
                    .query_row(
                        &format!(
                            "SELECT {k}, e.date FROM entries e JOIN messages m ON m.id = e.message_id
                             WHERE e.folder_id=?1 AND e.message_id=?2"
                        ),
                        params![folder_id, id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?,
                None => None,
            };
            // Order: k ASC, d DESC, id DESC; `back` walks it the other way.
            let order = if back { "k DESC, d ASC, id ASC" } else { "k ASC, d DESC, id DESC" };
            let first = |past: bool| -> Result<Option<i64>> {
                let cond = match (past, back) {
                    (false, _) => "",
                    (true, false) => "WHERE k > ?4 OR (k = ?4 AND (d < ?2 OR (d = ?2 AND id < ?3)))",
                    (true, true) => "WHERE k < ?4 OR (k = ?4 AND (d > ?2 OR (d = ?2 AND id > ?3)))",
                };
                let sql = format!("SELECT id FROM ({rows}) {cond} ORDER BY {order} LIMIT 1");
                let mut stmt = db.prepare_cached(&sql)?;
                Ok(match (&pos, past) {
                    (Some((key, d)), true) => stmt.query_row(params![folder_id, d, after, key], |r| r.get(0)).optional()?,
                    _ => stmt.query_row(params![folder_id], |r| r.get(0)).optional()?,
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
        Ok(self.index_of_by(folder_id, id, false, sort, filter)?.map(|i| (id, i)))
    }

    /// The groups of a folder's list, in its order; their counts add up to the list's rows.
    /// Sorted by date, `bounds` (Unix seconds, newest first) split it into periods: one count
    /// per period (`date >= bounds[0]`, then each below the one before), the last for what's
    /// older than every bound. Conversations go by their newest message here. Sorted by sender or
    /// recipient (messages only), one group per sender / recipient, labelled as shown.
    pub fn list_groups(&self, folder_id: i64, threaded: bool, sort: ListSort, filter: &ListFilter, bounds: &[i64]) -> Result<Vec<ListGroup>> {
        let db = self.rdb();
        let f = filter.sql(threaded);
        match (threaded, sort) {
            (true, _) | (_, ListSort::Received) => {
                let sql = match (threaded, f.is_empty()) {
                    (true, _) => format!("SELECT max(e.date) FROM {} GROUP BY e.thread_id", thread_rows_from(filter)),
                    (false, true) => "SELECT date FROM entries WHERE folder_id=?1".to_string(),
                    (false, false) => {
                        format!("SELECT e.date FROM entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id=?1{f}")
                    }
                };
                let mut counts = vec![0u32; bounds.len() + 1];
                let mut stmt = db.prepare_cached(&sql)?;
                let mut rows = stmt.query([folder_id])?;
                while let Some(r) = rows.next()? {
                    let d: i64 = r.get(0)?;
                    counts[bounds.partition_point(|&b| b > d)] += 1;
                }
                Ok(counts.into_iter().map(|count| ListGroup { label: None, count }).collect())
            }
            (false, ListSort::Size) => {
                let mut counts = [0u32; crate::sort::SIZE_RANGES.len()];
                let mut stmt = db.prepare_cached(&format!(
                    "SELECT m.size FROM entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id=?1{f}"
                ))?;
                let mut rows = stmt.query([folder_id])?;
                while let Some(r) = rows.next()? {
                    counts[crate::sort::size_range(r.get(0)?)] += 1;
                }
                Ok(counts
                    .into_iter()
                    .zip(crate::sort::SIZE_RANGES)
                    .filter(|(count, _)| *count > 0)
                    .map(|(count, (_, label))| ListGroup { label: Some(label.to_string()), count })
                    .collect())
            }
            (false, ListSort::From | ListSort::To) => {
                let k = sort.key_sql().unwrap_or_default();
                // The label comes from the group's newest message (bare columns go with max()).
                let mut stmt = db.prepare_cached(&format!(
                    "SELECT {k} AS k, m.from_name, m.from_addr, m.to_addrs, max(e.date), count(*)
                     FROM entries e JOIN messages m ON m.id = e.message_id WHERE e.folder_id=?1{f}
                     GROUP BY k ORDER BY k"
                ))?;
                let rows = stmt.query_map([folder_id], |r| {
                    let key: String = r.get(0)?;
                    let (name, addr, to): (String, String, String) = (r.get(1)?, r.get(2)?, r.get(3)?);
                    let shown = if key == crate::sort::NOBODY {
                        String::new()
                    } else if sort == ListSort::From {
                        crate::sort::sender_shown(&name, &addr).to_string()
                    } else {
                        crate::sort::recipient_shown(&to)
                    };
                    Ok(ListGroup { label: Some(shown), count: r.get(5)? })
                })?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            }
        }
    }

    /// Where mail from these messages' senders is kept: (folder, how many of theirs), most
    /// first. The Move list puts those folders on top.
    pub fn sender_folder_counts(&self, message_ids: &[i64]) -> Result<Vec<(i64, u32)>> {
        let db = self.rdb();
        let mut senders: Vec<(i64, String)> = Vec::new();
        let mut stmt = db.prepare_cached("SELECT account_id, lower(from_addr) FROM messages WHERE id=?1")?;
        for &id in message_ids.iter().take(200) {
            if let Some(s) = stmt.query_row([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).optional()?
                && !s.1.is_empty()
                && !senders.contains(&s)
            {
                senders.push(s);
            }
        }
        let Some(&(account_id, _)) = senders.first() else { return Ok(vec![]) };
        let addrs: Vec<String> = senders.into_iter().filter(|s| s.0 == account_id).map(|s| s.1).take(50).collect();
        let marks = vec!["?"; addrs.len()].join(",");
        let mut stmt = db.prepare(&format!(
            "SELECT e.folder_id, count(*) AS n FROM messages m JOIN entries e ON e.message_id = m.id
             WHERE m.account_id = {account_id} AND lower(m.from_addr) IN ({marks})
             GROUP BY e.folder_id ORDER BY n DESC"
        ))?;
        Ok(stmt.query_map(rusqlite::params_from_iter(addrs.iter()), |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// A domain's kept picture: Some((icon or None for "has none", when looked up)).
    pub fn sender_icon(&self, domain: &str) -> Result<Option<(Option<crate::icons::Icon>, i64)>> {
        let db = self.rdb();
        Ok(db
            .query_row("SELECT mime, data, fetched FROM sender_icons WHERE domain=?1", [domain], |r| {
                let mime: Option<String> = r.get(0)?;
                let data: Option<Vec<u8>> = r.get(1)?;
                Ok((mime.zip(data).map(|(mime, data)| crate::icons::Icon { mime, data }), r.get(2)?))
            })
            .optional()?)
    }

    pub fn save_sender_icon(&self, domain: &str, icon: Option<&crate::icons::Icon>, fetched: i64) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO sender_icons (domain, mime, data, fetched) VALUES (?1, ?2, ?3, ?4)",
            params![domain, icon.map(|i| i.mime.as_str()), icon.map(|i| i.data.as_slice()), fetched],
        )?;
        Ok(())
    }

    /// A folder's name as shown (its path, "/"-joined), for AI apps' answers.
    pub fn folder_name(&self, folder_id: i64) -> Result<Option<String>> {
        let db = self.rdb();
        let path: Option<String> = db.query_row("SELECT path FROM folders WHERE id=?1", [folder_id], |r| r.get(0)).optional()?;
        Ok(path.map(|p| serde_json::from_str::<Vec<String>>(&p).map(|v| v.join("/")).unwrap_or(p)))
    }

    /// The local draft a message is the copy in Drafts of, if it is one.
    pub fn message_draft(&self, id: i64) -> Result<Option<i64>> {
        let db = self.rdb();
        Ok(db.query_row("SELECT draft_id FROM messages WHERE id=?1", [id], |r| r.get(0)).optional()?.flatten())
    }

    pub fn message_subject(&self, id: i64) -> Result<Option<String>> {
        let db = self.rdb();
        Ok(db.query_row("SELECT subject FROM messages WHERE id=?1", [id], |r| r.get(0)).optional()?)
    }

    /// The list's rows (message ids, or conversation ids with `threaded`) from `offset` on, in
    /// the order `list_messages_by` / `list_threads_filtered` show them: what a ⇧-click range
    /// or Select All covers, most of it not loaded.
    pub fn list_item_ids(&self, folder_id: i64, threaded: bool, sort: ListSort, filter: &ListFilter, offset: u32, limit: u32) -> Result<Vec<i64>> {
        let db = self.rdb();
        let sql = if threaded {
            format!(
                "SELECT e.thread_id, max(e.date) AS d FROM {} GROUP BY e.thread_id
                 ORDER BY d DESC, e.thread_id DESC LIMIT ?2 OFFSET ?3",
                thread_rows_from(filter)
            )
        } else {
            let order = match sort.key_sql() {
                None => "e.date DESC, e.message_id DESC".to_string(),
                Some(k) => format!("{k}, e.date DESC, e.message_id DESC"),
            };
            format!(
                "SELECT e.message_id FROM entries e JOIN messages m ON m.id = e.message_id
                 WHERE e.folder_id=?1{} ORDER BY {order} LIMIT ?2 OFFSET ?3",
                filter.sql(false)
            )
        };
        let mut stmt = db.prepare_cached(&sql)?;
        Ok(stmt.query_map(params![folder_id, limit, offset], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// The messages behind a list's rows (`items`: message ids, or conversation ids with
    /// `threaded`: their messages in this folder), with what the actions need: (id, flags,
    /// keywords, size, subject).
    pub fn selection_messages(&self, folder_id: i64, items: &[i64], threaded: bool) -> Result<Vec<(i64, i64, String, i64, String)>> {
        let db = self.rdb();
        let mut out = Vec::with_capacity(items.len());
        for chunk in items.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = if threaded {
                format!(
                    "SELECT m.id, m.flags, m.keywords, m.size, m.subject FROM entries e JOIN messages m ON m.id = e.message_id
                     WHERE e.folder_id = {folder_id} AND e.thread_id IN ({marks}) ORDER BY e.date DESC, m.id DESC"
                )
            } else {
                format!(
                    "SELECT m.id, m.flags, m.keywords, m.size, m.subject FROM entries e JOIN messages m ON m.id = e.message_id
                     WHERE e.folder_id = {folder_id} AND e.message_id IN ({marks}) ORDER BY e.date DESC, m.id DESC"
                )
            };
            let mut stmt = db.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    /// The unread messages in a folder (Mark all as read).
    pub fn unread_in_folder(&self, folder_id: i64) -> Result<Vec<i64>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT e.message_id FROM entries e JOIN messages m ON m.id = e.message_id
             WHERE e.folder_id=?1 AND (m.flags & 1) = 0",
        )?;
        Ok(stmt.query_map([folder_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
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

    /// Every account's other addresses, each account's in the order added.
    pub fn identities(&self) -> Result<Vec<crate::identities::Identity>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT id, account_id, email, name, new_sig, reply_sig, is_default, new_none, reply_none FROM identities ORDER BY account_id, id",
        )?;
        // None chosen reads as signature 0 (`Identity::new_signature_id`).
        let sig = |id: Option<i64>, none: bool| if none { Some(0) } else { id };
        let rows = stmt.query_map([], |r| {
            Ok(crate::identities::Identity {
                id: r.get(0)?,
                account_id: r.get(1)?,
                email: r.get(2)?,
                name: r.get(3)?,
                new_signature_id: sig(r.get(4)?, r.get(7)?),
                reply_signature_id: sig(r.get(5)?, r.get(8)?),
                is_default: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Add (`id` 0) or change an identity, already checked. Returns its id. As the default, the
    /// account's others stop being it.
    pub fn save_identity(&self, i: &crate::identities::Identity) -> Result<i64> {
        let mut db = self.db();
        let tx = db.transaction()?;
        // Signature 0 (None chosen) is stored as its flag: the columns name real signatures.
        let sig = |id: Option<i64>| (id.filter(|&id| id != 0), id == Some(0));
        let (new_sig, new_none) = sig(i.new_signature_id);
        let (reply_sig, reply_none) = sig(i.reply_signature_id);
        let id = if i.id == 0 {
            tx.execute(
                "INSERT INTO identities (account_id, email, name, new_sig, reply_sig, is_default, new_none, reply_none)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![i.account_id, i.email, i.name, new_sig, reply_sig, i.is_default, new_none, reply_none],
            )?;
            tx.last_insert_rowid()
        } else {
            tx.execute(
                "UPDATE identities SET email=?2, name=?3, new_sig=?4, reply_sig=?5, is_default=?6, new_none=?8, reply_none=?9
                 WHERE id=?1 AND account_id=?7",
                params![i.id, i.email, i.name, new_sig, reply_sig, i.is_default, i.account_id, new_none, reply_none],
            )?;
            i.id
        };
        if i.is_default {
            tx.execute("UPDATE identities SET is_default=0 WHERE account_id=?1 AND id<>?2", params![i.account_id, id])?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn delete_identity(&self, id: i64) -> Result<()> {
        self.db().execute("DELETE FROM identities WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn set_display_name(&self, account_id: i64, name: &str) -> Result<()> {
        self.db().execute("UPDATE accounts SET display_name=?2 WHERE id=?1", params![account_id, name])?;
        Ok(())
    }

    /// Its server keeps keywords of our own (seen at its first sync).
    pub fn account_keeps_keywords(&self, account_id: i64) -> Result<bool> {
        Ok(self
            .rdb()
            .prepare_cached("SELECT coalesce(keywords, 0) FROM accounts WHERE id=?1")?
            .query_row([account_id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    pub fn account_uses_graph(&self, account_id: i64) -> Result<bool> {
        Ok(self
            .rdb()
            .prepare_cached("SELECT coalesce(graph, 0) FROM accounts WHERE id=?1")?
            .query_row([account_id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    /// An account's messages for an export: (message id, UID or 0 while unknown, date) per
    /// folder id, newest last.
    pub fn export_entries(&self, account_id: i64) -> Result<Vec<(i64, Vec<(i64, u32, i64)>)>> {
        let db = self.rdb();
        let mut stmt = db.prepare(
            "SELECT e.folder_id, e.message_id, coalesce(e.uid, 0), e.date
             FROM entries e JOIN folders f ON f.id = e.folder_id
             WHERE f.account_id=?1 AND f.selectable=1
             ORDER BY e.folder_id, e.date, e.message_id",
        )?;
        let mut out: Vec<(i64, Vec<(i64, u32, i64)>)> = Vec::new();
        for row in stmt.query_map([account_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, u32>(2)?, r.get::<_, i64>(3)?)))? {
            let (f, m, uid, date) = row?;
            match out.last_mut() {
                Some((last, list)) if *last == f => list.push((m, uid, date)),
                _ => out.push((f, vec![(m, uid, date)])),
            }
        }
        Ok(out)
    }

    /// Whether the message is here whole: a body stored, with no attachment left on the server.
    pub fn stored_whole(&self, id: i64) -> Result<bool> {
        let omitted = self.omitted(id)?;
        if omitted.as_deref().is_some_and(|o| !o.trim().is_empty() && o.trim() != "[]") {
            return Ok(false);
        }
        let bodies = self.bodies_read.lock().unwrap_or_else(|e| e.into_inner());
        Ok(bodies.prepare_cached("SELECT 1 FROM bodies WHERE message_id=?1")?.exists([id])?)
    }

    /// For the export dialog: (messages, of which not here whole, their size in bytes).
    pub fn export_plan(&self, account_id: i64) -> Result<(u32, u32, u64)> {
        let ids: Vec<(i64, i64)> = {
            let db = self.rdb();
            let mut stmt = db.prepare(
                "SELECT DISTINCT m.id, m.size FROM messages m JOIN entries e ON e.message_id = m.id
                 JOIN folders f ON f.id = e.folder_id WHERE f.account_id=?1 AND f.selectable=1",
            )?;
            stmt.query_map([account_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let (mut missing, mut bytes) = (0u32, 0u64);
        for (id, size) in &ids {
            if !self.stored_whole(*id)? {
                missing += 1;
                bytes += (*size).max(0) as u64;
            }
        }
        Ok((ids.len() as u32, missing, bytes))
    }

    /// Whether an account's tags and snooze go through Microsoft Graph.
    pub fn set_account_graph(&self, account_id: i64, on: bool) -> Result<()> {
        self.db().execute("UPDATE accounts SET graph=?2 WHERE id=?1", params![account_id, on])?;
        Ok(())
    }

    /// (id, Message-ID, stored keywords) of an account's messages that have a Message-ID: what
    /// Graph's tags and markers are matched against.
    pub fn account_message_keywords(&self, account_id: i64) -> Result<Vec<(i64, String, String)>> {
        let db = self.rdb();
        let mut stmt = db.prepare(
            "SELECT id, message_id, keywords FROM messages WHERE account_id=?1 AND message_id IS NOT NULL AND message_id <> ''",
        )?;
        Ok(stmt.query_map([account_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// New keywords for messages (id, stored form), from a sync step that started at `epoch`;
    /// the folders they're in.
    pub fn set_keywords_since(&self, epoch: Option<u64>, updates: &[(i64, String)]) -> Result<Vec<i64>> {
        let mut db = self.db_for_sync(epoch)?;
        let tx = db.transaction()?;
        let mut folders = std::collections::BTreeSet::new();
        {
            let mut set = tx.prepare_cached("UPDATE messages SET keywords=?2 WHERE id=?1")?;
            let mut where_ = tx.prepare_cached("SELECT folder_id FROM entries WHERE message_id=?1")?;
            for (id, keywords) in updates {
                set.execute(params![id, keywords])?;
                settle_inbox_date(&tx, *id)?;
                for f in where_.query_map([id], |r| r.get::<_, i64>(0))? {
                    folders.insert(f?);
                }
            }
        }
        tx.commit()?;
        Ok(folders.into_iter().collect())
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
            "UPDATE messages SET has_body=1, snippet=?2, has_attachments=?3, omitted=?4, attachments_stored=?5, body_indexed=0
             WHERE id=?1",
            params![id, snippet, has_attachments, omitted, stored],
        )?;
        Ok(())
    }

    /// Make the text of up to `batch` messages whose body is here searchable (body_fts), newest
    /// first. Returns how many were done: 0 when none are waiting. Each body is read and parsed
    /// with no lock held but the moment's, and the batch is written in one short transaction.
    pub fn index_bodies(&self, batch: usize) -> Result<usize> {
        let ids: Vec<i64> = {
            let db = self.rdb();
            let mut stmt = db.prepare_cached("SELECT id FROM messages WHERE has_body=1 AND body_indexed=0 ORDER BY id DESC LIMIT ?1")?;
            stmt.query_map([batch as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        if ids.is_empty() {
            return Ok(0);
        }
        let read_files = self.attachment_search()?;
        // (id, the body's length and how many attachments were downloaded apart, as read; its text)
        let mut texts: Vec<(i64, Option<usize>, usize, Option<(String, String)>)> = Vec::with_capacity(ids.len());
        for id in &ids {
            let (raw, cached): (Option<Vec<u8>>, Vec<(usize, Vec<u8>)>) = {
                let bodies = self.bodies_read.lock().unwrap_or_else(|e| e.into_inner());
                let raw = bodies.prepare_cached("SELECT raw FROM bodies WHERE message_id=?1")?.query_row([id], |r| r.get(0)).optional()?;
                let mut stmt = bodies.prepare_cached("SELECT idx, data FROM attachment_cache WHERE message_id=?1 ORDER BY idx")?;
                let cached = stmt.query_map([id], |r| Ok((r.get::<_, i64>(0)? as usize, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
                (raw, cached)
            };
            let fetched = match &raw {
                Some(raw) => self.fetched_attachments(*id, raw, cached.iter().filter(|_| read_files).cloned())?,
                None => Vec::new(),
            };
            let text = raw.as_deref().and_then(|r| crate::mime::search_text(r, &fetched, read_files));
            texts.push((*id, raw.as_ref().map(Vec::len), cached.len(), text));
        }
        let mut db = self.db();
        let tx = db.transaction()?;
        let mut done_n = 0;
        {
            let mut del = tx.prepare_cached("DELETE FROM body_fts WHERE rowid=?1")?;
            let mut add = tx.prepare_cached("INSERT INTO body_fts (rowid, body) VALUES (?1, ?2)")?;
            let mut del_files = tx.prepare_cached("DELETE FROM file_fts WHERE rowid=?1")?;
            let mut add_files = tx.prepare_cached("INSERT INTO file_fts (rowid, files) VALUES (?1, ?2)")?;
            let mut done = tx.prepare_cached("UPDATE messages SET body_indexed=1 WHERE id=?1")?;
            let mut waiting = tx.prepare_cached("SELECT 1 FROM messages WHERE id=?1 AND has_body=1 AND body_indexed=0")?;
            let bodies = self.bodies_read.lock().unwrap_or_else(|e| e.into_inner());
            let mut len_now = bodies.prepare_cached("SELECT length(raw) FROM bodies WHERE message_id=?1")?;
            let mut cached_now = bodies.prepare_cached("SELECT count(*) FROM attachment_cache WHERE message_id=?1")?;
            for (id, len, cached, text) in &texts {
                // Read without a lock: the message may have gone since (its id could come back
                // for another), or got a new body (which marked it to do again after writing
                // the body, so the body here is already the new one). Then it's left as it is.
                if !waiting.exists([id])? {
                    continue;
                }
                let now: Option<i64> = len_now.query_row([id], |r| r.get(0)).optional()?;
                if now.map(|n| n as usize) != *len || cached_now.query_row([id], |r| r.get::<_, i64>(0))? as usize != *cached {
                    continue;
                }
                done_n += 1;
                del.execute([id])?;
                del_files.execute([id])?;
                let (body, files) = text.as_ref().map_or(("", ""), |(b, f)| (b.as_str(), f.as_str()));
                if !body.trim().is_empty() {
                    add.execute(params![id, body])?;
                }
                if !files.trim().is_empty() {
                    add_files.execute(params![id, files])?;
                }
                done.execute([id])?;
            }
        }
        tx.commit()?;
        // Those passed over are still waiting (a later batch takes them); a batch that only
        // passed over still counts as work, so the caller asks again.
        Ok(done_n.max(usize::from(!texts.is_empty())))
    }

    /// The attachments of a message stored without them, as (name, mime, bytes) from the
    /// omitted parts' descriptions: with the bytes of those in `cached` (downloaded later, the
    /// cache's `(idx, bytes)`), none for the others (their names are searched all the same).
    fn fetched_attachments(&self, id: i64, raw: &[u8], cached: impl Iterator<Item = (usize, Vec<u8>)>) -> Result<Vec<(String, String, Vec<u8>)>> {
        let omitted: Vec<crate::partial::OmittedPart> = self.omitted(id)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        if omitted.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<(String, String, Vec<u8>)> = omitted.iter().map(|p| (p.name.clone(), p.mime.clone(), Vec::new())).collect();
        // Omitted attachments are numbered after those in the body.
        let stored = crate::mime::stored_attachment_count(raw);
        for (idx, data) in cached {
            if let Some(slot) = idx.checked_sub(stored).and_then(|o| out.get_mut(o)) {
                slot.2 = data;
            }
        }
        Ok(out)
    }

    /// Once, after the upgrade that made attachments searchable on their own (file_fts):
    /// messages with attachments indexed before that are indexed again. Returns how many.
    pub fn index_attachment_text_once(&self) -> Result<usize> {
        if self.choice("attachment_fts_indexed", false)? {
            return Ok(0);
        }
        let n = self.db().execute("UPDATE messages SET body_indexed=0 WHERE has_body=1 AND body_indexed=1 AND has_attachments=1", [])?;
        self.set_choice("attachment_fts_indexed", true)?;
        Ok(n)
    }

    /// Messages whose body is here and still to be made searchable.
    pub fn bodies_to_index(&self) -> Result<u64> {
        let db = self.rdb();
        Ok(db.query_row("SELECT count(*) FROM messages WHERE has_body=1 AND body_indexed=0", [], |r| r.get::<_, i64>(0))? as u64)
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

    /// "Load images" chosen for this message, or always for its sender or the sender's domain
    /// ("@papercut.com" takes ann@papercut.com and ann@mail.papercut.com, not ann@xpapercut.com).
    /// `domain_verified`: a domain's rule counts (domain rules are on, and the From address is
    /// believed, `Core::from_believed`). `sender_verified`: a rule for the sender's own address
    /// counts (false where the server checks senders and didn't vouch for this one).
    pub fn allow_remote(&self, id: i64, domain_verified: bool, sender_verified: bool) -> Result<bool> {
        Ok(self
            .rdb()
            .query_row(
                "SELECT m.allow_remote != 0 OR EXISTS (SELECT 1 FROM remote_senders r WHERE (?3 AND r.addr = lower(m.from_addr))
                   OR (?2 AND substr(r.addr, 1, 1) = '@'
                       AND substr(lower(m.from_addr), -length(r.addr)) IN (r.addr, '.' || substr(r.addr, 2))))
                 FROM messages m WHERE m.id=?1",
                rusqlite::params![id, domain_verified, sender_verified],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Every account's own marker picture (data: URLs).
    pub fn account_logos(&self) -> Result<Vec<(i64, String)>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT account_id, data_url FROM account_logos")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Set (or with None take away) an account's marker picture: a data: URL of a PNG, JPEG,
    /// GIF, WebP or SVG image, small (the app scales it down first).
    pub fn set_account_logo(&self, account_id: i64, data_url: Option<&str>) -> Result<()> {
        match data_url {
            Some(url) => {
                let ok = ["data:image/png;", "data:image/jpeg;", "data:image/gif;", "data:image/webp;", "data:image/svg+xml;"]
                    .iter()
                    .any(|p| url.starts_with(p));
                if !ok || !url.contains(";base64,") {
                    anyhow::bail!("That isn't a picture the app can show.");
                }
                if url.len() > 512 * 1024 {
                    anyhow::bail!("That picture is too big.");
                }
                self.db().execute(
                    "INSERT OR REPLACE INTO account_logos (account_id, data_url) VALUES (?1, ?2)",
                    params![account_id, url],
                )?;
            }
            None => {
                self.db().execute("DELETE FROM account_logos WHERE account_id=?1", [account_id])?;
            }
        }
        Ok(())
    }

    /// Always (or no longer) load remote images in mail from `addr` (or "@domain": everyone there).
    pub fn set_remote_sender(&self, addr: &str, on: bool) -> Result<()> {
        let addr = addr.trim().to_lowercase();
        if addr.is_empty() {
            return Ok(());
        }
        if on {
            self.db().execute("INSERT OR IGNORE INTO remote_senders (addr) VALUES (?1)", [&addr])?;
        } else {
            self.db().execute("DELETE FROM remote_senders WHERE addr=?1", [&addr])?;
        }
        Ok(())
    }

    /// One of the user's yes/no choices in `settings` ("1" or "0"), or `default` when unset.
    pub fn choice(&self, key: &str, default: bool) -> Result<bool> {
        Ok(self
            .rdb()
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get::<_, String>(0))
            .optional()?
            .map_or(default, |v| v == "1"))
    }

    pub fn set_choice(&self, key: &str, on: bool) -> Result<()> {
        self.db().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key, if on { "1" } else { "0" }],
        )?;
        Ok(())
    }

    /// Whether search reads the text in attachments (PDFs, plain-text files): off unless the
    /// user turns it on (Settings › General › Privacy). Off, only their names are searched.
    pub fn attachment_search(&self) -> Result<bool> {
        self.choice("attachment_search", false)
    }

    /// Turn reading attachments for search on or off: messages with attachments are indexed
    /// again, so their text comes into the index, or goes from it.
    pub fn set_attachment_search(&self, on: bool) -> Result<()> {
        if self.attachment_search()? == on {
            return Ok(());
        }
        self.set_choice("attachment_search", on)?;
        self.db().execute("UPDATE messages SET body_indexed=0 WHERE has_body=1 AND has_attachments=1", [])?;
        Ok(())
    }

    /// Whether "Always load…" can cover everyone at a domain (off by default).
    pub fn domain_rules(&self) -> Result<bool> {
        self.choice("domain_rules", false)
    }

    pub fn set_domain_rules(&self, on: bool) -> Result<()> {
        self.set_choice("domain_rules", on)
    }

    /// The senders whose images always load, A to Z.
    pub fn remote_senders(&self) -> Result<Vec<String>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT addr FROM remote_senders ORDER BY addr")?;
        Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn message_from_addr(&self, id: i64) -> Result<Option<String>> {
        Ok(self.rdb().query_row("SELECT from_addr FROM messages WHERE id=?1", [id], |r| r.get(0)).optional()?)
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
        drop(bodies);
        // Its text is searchable too, if the user chose that: the message is indexed again
        // (after the write, as in `store_body`).
        if self.attachment_search()? {
            self.db().execute("UPDATE messages SET body_indexed=0 WHERE id=?1 AND has_body=1", [id])?;
        }
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
                &format!(
                    "INSERT INTO messages (id, account_id, message_id, in_reply_to, refs, subject, from_name, from_addr,
                    to_addrs, date, size, flags, has_attachments, identity, keywords, local_copy, pending_send, draft_id)
                 VALUES ({NEXT_MESSAGE_ID},?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,'',1,?14,?15)"
                ),
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
    /// Returns the folders it's in, and the copy's id when it was there already (its body
    /// rewritten under the same id: what's shown of it has to be read again).
    pub fn upsert_draft_copy(&self, draft_id: i64, account_id: i64, folder_id: i64, raw: &[u8]) -> Result<(Vec<i64>, Option<i64>)> {
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
                return Ok((self.message_folder_ids(id)?, Some(id)));
            }
        };
        Ok((self.message_folder_ids(id)?, None))
    }

    /// Every draft's and every queued message's composer state (to see which files they use).
    pub fn draft_states(&self) -> Result<Vec<String>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT state FROM drafts UNION ALL SELECT draft FROM outgoing WHERE draft IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A message in `folder_id` with this Message-ID header, if there's one.
    pub fn message_in_folder_by_header(&self, folder_id: i64, header_id: &str) -> Result<Option<i64>> {
        Ok(self
            .rdb()
            .query_row(
                "SELECT m.id FROM messages m JOIN entries e ON e.message_id=m.id WHERE e.folder_id=?1 AND m.message_id=?2 LIMIT 1",
                params![folder_id, header_id],
                |r| r.get(0),
            )
            .optional()?)
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
    /// suggestions (`contacts::build`). To+Cc only for a message in a Sent folder: they count as
    /// people written to (`build` reads them only for mail from the user), and anyone can send
    /// mail that claims to be from the user's own address; in Sent, it really was sent.
    pub fn contact_rows(&self) -> Result<Vec<(String, String, String, i64)>> {
        let db = self.rdb();
        // Not from Spam: spammers borrow familiar names ("Ya Hall" on a throwaway address).
        let mut stmt = db.prepare(
            "SELECT m.from_name, m.from_addr,
                    CASE WHEN EXISTS (SELECT 1 FROM entries e JOIN folders f ON f.id = e.folder_id
                                      WHERE e.message_id = m.id AND f.role = 'sent')
                         THEN m.to_addrs ELSE '' END,
                    m.date FROM messages m
             WHERE m.id NOT IN (SELECT e.message_id FROM entries e JOIN folders f ON f.id = e.folder_id
                                WHERE f.role = 'junk')",
        )?;
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

    // ---- address book -----------------------------------------------------------------------

    pub fn cards(&self) -> Result<Vec<crate::addressbook::Card>> {
        let mut cards = self.cards_where(None)?;
        cards.sort_by(|a, b| a.title().to_lowercase().cmp(&b.title().to_lowercase()).then(a.id.cmp(&b.id)));
        Ok(cards)
    }

    /// Every card, or (`Some(id)`) just that one.
    fn cards_where(&self, id: Option<i64>) -> Result<Vec<crate::addressbook::Card>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT b.id, b.name, b.emails, b.phones, b.company, b.notes, l.account_id,
                    (SELECT json_group_array(json_extract(m.value, '$.contactGroupMembership.contactGroupResourceName'))
                     FROM json_each(l.raw, '$.memberships') m
                     WHERE json_extract(m.value, '$.contactGroupMembership.contactGroupResourceName') IS NOT NULL)
             FROM address_book b LEFT JOIN card_links l ON l.card_id = b.id
             WHERE ?1 IS NULL OR b.id = ?1",
        )?;
        let cards: Vec<crate::addressbook::Card> = stmt
            .query_map([id], |r| {
                Ok(crate::addressbook::Card {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    emails: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                    phones: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    company: r.get(4)?,
                    notes: r.get(5)?,
                    google: r.get(6)?,
                    groups: r.get::<_, Option<String>>(7)?.and_then(|g| serde_json::from_str(&g).ok()).unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(cards)
    }

    /// Save a card (new when its id is 0); returns its id.
    pub fn save_card(&self, card: crate::addressbook::Card) -> Result<i64> {
        write_card(&self.db(), card)
    }
    pub fn delete_card(&self, id: i64) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM address_book WHERE id=?1", [id])?;
        db.execute("DELETE FROM card_links WHERE card_id=?1", [id])?;
        db.execute("DELETE FROM card_photos WHERE card_id=?1", [id])?;
        Ok(())
    }

    /// Every address in the address book.
    pub fn address_book_emails(&self) -> Result<Vec<String>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT j.value FROM address_book b, json_each(b.emails) j")?;
        Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn card(&self, id: i64) -> Result<Option<crate::addressbook::Card>> {
        Ok(self.cards_where(Some(id))?.into_iter().next())
    }

    // ---- Google Contacts (crate::gcontacts) --------------------------------------------------

    pub fn google_links(&self) -> Result<Vec<crate::gcontacts::GoogleLink>> {
        let links = self.google_links_only()?;
        links
            .into_iter()
            .map(|mut l| {
                l.groups = self.contact_groups(l.account_id)?;
                Ok(l)
            })
            .collect()
    }

    fn google_links_only(&self) -> Result<Vec<crate::gcontacts::GoogleLink>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT g.account_id, g.google_email, g.sync_token, g.synced, g.error,
                    (SELECT count(*) FROM card_links l WHERE l.account_id = g.account_id)
             FROM google_links g ORDER BY g.account_id",
        )?;
        Ok(stmt
            .query_map([], |r| {
                Ok(crate::gcontacts::GoogleLink {
                    account_id: r.get(0)?,
                    google_email: r.get(1)?,
                    sync_token: r.get(2)?,
                    synced: r.get(3)?,
                    error: r.get(4)?,
                    count: r.get(5)?,
                    groups: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// An account's groups in Google Contacts, in the order shown.
    pub fn contact_groups(&self, account_id: i64) -> Result<Vec<crate::gcontacts::ContactGroup>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT resource, name FROM contact_groups WHERE account_id=?1 ORDER BY position")?;
        Ok(stmt
            .query_map([account_id], |r| Ok(crate::gcontacts::ContactGroup { resource: r.get(0)?, name: r.get(1)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_contact_groups(&self, account_id: i64, groups: &[crate::gcontacts::ContactGroup]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM contact_groups WHERE account_id=?1", [account_id])?;
        for (i, g) in groups.iter().enumerate() {
            tx.execute(
                "INSERT OR REPLACE INTO contact_groups (account_id, position, resource, name) VALUES (?1,?2,?3,?4)",
                params![account_id, i as i64, g.resource, g.name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn google_link(&self, account_id: i64) -> Result<Option<crate::gcontacts::GoogleLink>> {
        Ok(self.google_links()?.into_iter().find(|l| l.account_id == account_id))
    }

    /// Linked (again): a new sign-in starts with a full sync.
    pub fn set_google_link(&self, account_id: i64, google_email: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO google_links (account_id, google_email) VALUES (?1, ?2)
             ON CONFLICT(account_id) DO UPDATE SET google_email=?2, sync_token=NULL, error=NULL",
            params![account_id, google_email],
        )?;
        Ok(())
    }

    /// How a sync went: where it left off (`token`: None keeps the old one), when, what failed.
    pub fn set_google_sync(&self, account_id: i64, token: Option<&str>, synced: Option<i64>, error: Option<&str>) -> Result<()> {
        self.db().execute(
            "UPDATE google_links SET sync_token=coalesce(?2, sync_token), synced=coalesce(?3, synced), error=?4 WHERE account_id=?1",
            params![account_id, token, synced, error],
        )?;
        Ok(())
    }

    /// Unlinked: the link, the cards that came from it, and what was waiting to be sent go.
    /// Nothing is changed in Google.
    pub fn remove_google_link(&self, account_id: i64) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM address_book WHERE id IN (SELECT card_id FROM card_links WHERE account_id=?1)", [account_id])?;
        db.execute("DELETE FROM card_photos WHERE card_id IN (SELECT card_id FROM card_links WHERE account_id=?1)", [account_id])?;
        db.execute("DELETE FROM card_links WHERE account_id=?1", [account_id])?;
        db.execute("DELETE FROM card_tombstones WHERE account_id=?1", [account_id])?;
        db.execute("DELETE FROM contact_groups WHERE account_id=?1", [account_id])?;
        db.execute("DELETE FROM google_links WHERE account_id=?1", [account_id])?;
        Ok(())
    }

    pub fn card_links(&self, account_id: i64) -> Result<Vec<crate::gcontacts::CardLink>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT card_id, account_id, resource, etag, raw, dirty FROM card_links WHERE account_id=?1")?;
        Ok(stmt
            .query_map([account_id], |r| {
                Ok(crate::gcontacts::CardLink {
                    card_id: r.get(0)?,
                    account_id: r.get(1)?,
                    resource: r.get(2)?,
                    etag: r.get(3)?,
                    raw: r.get(4)?,
                    dirty: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn card_link(&self, card_id: i64) -> Result<Option<crate::gcontacts::CardLink>> {
        let db = self.rdb();
        Ok(db
            .query_row("SELECT card_id, account_id, resource, etag, raw, dirty FROM card_links WHERE card_id=?1", [card_id], |r| {
                Ok(crate::gcontacts::CardLink {
                    card_id: r.get(0)?,
                    account_id: r.get(1)?,
                    resource: r.get(2)?,
                    etag: r.get(3)?,
                    raw: r.get(4)?,
                    dirty: r.get(5)?,
                })
            })
            .optional()?)
    }

    /// The Google contacts (cards linked to Google) with this address, oldest first: whose
    /// picture a sender gets.
    pub fn google_cards_with_email(&self, addr: &str) -> Result<Vec<i64>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached(
            "SELECT b.id FROM address_book b JOIN card_links l ON l.card_id = b.id
             WHERE l.raw IS NOT NULL
               AND EXISTS (SELECT 1 FROM json_each(b.emails) j WHERE lower(j.value) = lower(?1))
             ORDER BY b.id",
        )?;
        let ids = stmt.query_map([addr.trim()], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// A contact's picture as fetched: (the url it came from, its type, the image).
    pub fn card_photo(&self, card_id: i64) -> Result<Option<(String, String, Vec<u8>)>> {
        Ok(self
            .rdb()
            .query_row("SELECT url, mime, data FROM card_photos WHERE card_id=?1", [card_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?)
    }

    pub fn save_card_photo(&self, card_id: i64, url: &str, mime: &str, data: &[u8]) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO card_photos (card_id, url, mime, data) VALUES (?1, ?2, ?3, ?4)",
            params![card_id, url, mime, data],
        )?;
        Ok(())
    }

    /// The user changed a Google contact: one more edit to send (`card_links.dirty` counts
    /// them, in one statement, so a sync running meanwhile can tell).
    pub fn mark_card_edited(&self, card_id: i64) -> Result<()> {
        self.db().execute("UPDATE card_links SET dirty = dirty + 1 WHERE card_id=?1", [card_id])?;
        Ok(())
    }

    /// How many edits of a card are still to send.
    pub fn card_link_edits(&self, card_id: i64) -> Result<i64> {
        Ok(self.db().query_row("SELECT dirty FROM card_links WHERE card_id=?1", [card_id], |r| r.get(0)).optional()?.unwrap_or(0))
    }

    /// A sync sent a card as it was after `sent` edits: the person Google keeps now, and those
    /// edits done. Any made while it was being sent stay, to go next time.
    pub fn settle_card_link(&self, l: &crate::gcontacts::CardLink, sent: i64) -> Result<()> {
        self.db().execute(
            "UPDATE card_links SET account_id=?2, resource=?3, etag=?4, raw=?5, dirty=max(dirty - ?6, 0) WHERE card_id=?1",
            params![l.card_id, l.account_id, l.resource, l.etag, l.raw, sent],
        )?;
        Ok(())
    }

    /// Google's version of a person, for a card that had no edits to send when the sync looked:
    /// the card replaced (`card`, when it differs) and the link updated, unless the user has
    /// edited the card since. Then only the link's copy of the person is updated, and the edit
    /// goes at the next sync, on top of it. Checked and written under one lock. Whether the
    /// card was replaced.
    pub fn apply_google_person(&self, card: Option<crate::addressbook::Card>, l: &crate::gcontacts::CardLink) -> Result<bool> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        let edits: i64 = tx.query_row("SELECT dirty FROM card_links WHERE card_id=?1", [l.card_id], |r| r.get(0)).optional()?.unwrap_or(0);
        if edits > 0 {
            tx.execute(
                "UPDATE card_links SET resource=?2, etag=?3, raw=?4 WHERE card_id=?1",
                params![l.card_id, l.resource, l.etag, l.raw],
            )?;
            tx.commit()?;
            return Ok(false);
        }
        let replaced = match card {
            Some(c) => {
                write_card(&tx, crate::addressbook::Card { id: l.card_id, ..c })?;
                true
            }
            None => false,
        };
        tx.execute(
            "INSERT OR REPLACE INTO card_links (card_id, account_id, resource, etag, raw, dirty) VALUES (?1,?2,?3,?4,?5,0)",
            params![l.card_id, l.account_id, l.resource, l.etag, l.raw],
        )?;
        tx.commit()?;
        Ok(replaced)
    }

    /// Delete a card gone from Google, unless the user has edited it since the sync looked
    /// (then it stays, and is made there again). Whether it went.
    pub fn delete_card_if_unedited(&self, card_id: i64) -> Result<bool> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        let edits: i64 = tx.query_row("SELECT dirty FROM card_links WHERE card_id=?1", [card_id], |r| r.get(0)).optional()?.unwrap_or(0);
        if edits > 0 {
            return Ok(false);
        }
        tx.execute("DELETE FROM address_book WHERE id=?1", [card_id])?;
        tx.execute("DELETE FROM card_links WHERE card_id=?1", [card_id])?;
        tx.execute("DELETE FROM card_photos WHERE card_id=?1", [card_id])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn save_card_link(&self, l: &crate::gcontacts::CardLink) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO card_links (card_id, account_id, resource, etag, raw, dirty) VALUES (?1,?2,?3,?4,?5,?6)",
            params![l.card_id, l.account_id, l.resource, l.etag, l.raw, l.dirty],
        )?;
        Ok(())
    }

    pub fn delete_card_link(&self, card_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM card_links WHERE card_id=?1", [card_id])?;
        Ok(())
    }

    pub fn add_card_tombstone(&self, account_id: i64, resource: &str) -> Result<()> {
        self.db().execute("INSERT OR IGNORE INTO card_tombstones (account_id, resource) VALUES (?1, ?2)", params![account_id, resource])?;
        Ok(())
    }

    pub fn card_tombstones(&self, account_id: i64) -> Result<Vec<String>> {
        let db = self.rdb();
        let mut stmt = db.prepare_cached("SELECT resource FROM card_tombstones WHERE account_id=?1")?;
        Ok(stmt.query_map([account_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_card_tombstone(&self, account_id: i64, resource: &str) -> Result<()> {
        self.db().execute("DELETE FROM card_tombstones WHERE account_id=?1 AND resource=?2", params![account_id, resource])?;
        Ok(())
    }

    // ---- signatures -------------------------------------------------------------------------

    pub fn signatures(&self) -> Result<crate::signatures::Signatures> {
        use crate::signatures::{Signature, SignatureDefaults, Signatures};
        let db = self.rdb();
        let signatures = db
            .prepare_cached("SELECT id, name, html FROM signatures ORDER BY position, id")?
            .query_map([], |r| Ok(Signature { id: r.get(0)?, name: r.get(1)?, html: r.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?;
        let defaults = db
            .prepare_cached("SELECT account_id, new_id, reply_id FROM signature_defaults ORDER BY account_id")?
            .query_map([], |r| Ok(SignatureDefaults { account_id: r.get(0)?, new_id: r.get(1)?, reply_id: r.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Signatures { signatures, defaults })
    }

    /// Create (`id` None) or change a signature; returns its id. A new one is listed last.
    pub fn save_signature(&self, id: Option<i64>, name: &str, html: &str) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("A signature needs a name.");
        }
        let db = self.db();
        match id {
            Some(id) => {
                let n = db.execute("UPDATE signatures SET name=?2, html=?3 WHERE id=?1", params![id, name, html])?;
                if n == 0 {
                    anyhow::bail!("That signature no longer exists.");
                }
                Ok(id)
            }
            None => {
                db.execute(
                    "INSERT INTO signatures (name, html, position)
                     VALUES (?1, ?2, (SELECT coalesce(max(position), -1) + 1 FROM signatures))",
                    params![name, html],
                )?;
                Ok(db.last_insert_rowid())
            }
        }
    }

    /// Delete a signature; accounts using it by default then have none.
    /// Delete a signature: the addresses that sent with it send with none (an account's own
    /// address, and another address that had chosen it: None, not its account's, which may be
    /// another signature).
    pub fn delete_signature(&self, id: i64) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("UPDATE signature_defaults SET new_id=NULL WHERE new_id=?1", [id])?;
        tx.execute("UPDATE signature_defaults SET reply_id=NULL WHERE reply_id=?1", [id])?;
        tx.execute("UPDATE identities SET new_sig=NULL, new_none=1 WHERE new_sig=?1", [id])?;
        tx.execute("UPDATE identities SET reply_sig=NULL, reply_none=1 WHERE reply_sig=?1", [id])?;
        tx.execute("DELETE FROM signatures WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// An account's default signatures, for new messages and for replies and forwards.
    pub fn set_signature_defaults(&self, account_id: i64, new_id: Option<i64>, reply_id: Option<i64>) -> Result<()> {
        self.db().execute(
            "INSERT INTO signature_defaults (account_id, new_id, reply_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id) DO UPDATE SET new_id=excluded.new_id, reply_id=excluded.reply_id",
            params![account_id, new_id, reply_id],
        )?;
        Ok(())
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
    fn spam_teaches_no_addresses() {
        let dir = std::env::temp_dir().join(format!("mail-core-spam-contacts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("john@example.com", "John", Provider::Imap, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store.sync_folder_list(acct.id, &[lf("INBOX", Some(FolderRole::Inbox)), lf("Junk", Some(FolderRole::Junk)), lf("Sent", Some(FolderRole::Sent))]).unwrap();
        let id = |n: &str| store.folders(acct.id).unwrap().into_iter().find(|f| f.raw_name == n).unwrap().id;
        let rec = |uid: u32, name: &str, addr: &str| HeaderRecord {
            original_message_id: None,
            uid,
            gm_msgid: None,
            email_id: None,
            gm_labels: None,
            keywords: vec![],
            message_id: Some(format!("{uid}@example.com")),
            in_reply_to: None,
            refs: None,
            subject: "s".into(),
            from_name: name.into(),
            from_addr: addr.into(),
            to_addrs: String::new(),
            date: 1,
            size: 10,
            flags: 0,
            has_attachments: false,
        };
        let none = std::collections::HashMap::new();
        store.upsert_headers(acct.id, id("INBOX"), &[rec(1, "Jane Doe", "jane@example.org")], &none).unwrap();
        store.upsert_headers(acct.id, id("Junk"), &[rec(2, "Jane Doe", "jane@spam.example")], &none).unwrap();
        let from: Vec<String> = store.contact_rows().unwrap().into_iter().map(|r| r.1).collect();
        assert_eq!(from, ["jane@example.org"]);
        // "From me" in the Inbox (anyone can write that) names no one written to; in Sent it does.
        let mine = |uid: u32, to: &str| HeaderRecord { to_addrs: to.into(), ..rec(uid, "John", "john@example.com") };
        store.upsert_headers(acct.id, id("INBOX"), &[mine(3, "lure@forged.example")], &none).unwrap();
        store.upsert_headers(acct.id, id("Sent"), &[mine(4, "ann@example.org")], &none).unwrap();
        let to: Vec<String> = store.contact_rows().unwrap().into_iter().filter(|r| r.1 == "john@example.com").map(|r| r.2).collect();
        assert_eq!(to, ["", "ann@example.org"]);
        let list = crate::contacts::build(store.contact_rows().unwrap(), &["john@example.com".to_string()]);
        assert!(list.iter().all(|c| c.addr != "lure@forged.example"), "not someone written to");
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
    fn rows_carry_their_recipients() {
        // What Sent and Drafts show in place of the sender (who it went to).
        let dir = std::env::temp_dir().join(format!("mail-core-to-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("me@example.com", "Me", Provider::Imap, "h", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true };
        store.sync_folder_list(acct.id, &[lf("Sent", Some(FolderRole::Sent))]).unwrap();
        let sent = store.folders(acct.id).unwrap()[0].id;
        let mine = HeaderRecord {
            gm_msgid: None,
            gm_labels: None,
            message_id: Some("one@example.com".into()),
            from_name: "Me".into(),
            from_addr: "me@example.com".into(),
            to_addrs: "\"Doe, Jane\" <jane@example.org>, bob@example.org".into(),
            ..rec(1, 0, &[])
        };
        store.upsert_headers(acct.id, sent, &[mine], &HashMap::new()).unwrap();
        let want = vec![
            Addr { name: Some("Doe, Jane".into()), addr: "jane@example.org".into() },
            Addr { name: None, addr: "bob@example.org".into() },
        ];
        let m = &store.list_messages(sent, 0, 10).unwrap().messages[0];
        assert_eq!(m.to, want);
        let t = &store.list_threads(sent, ThreadScope::default(), 0, 10).unwrap().threads[0];
        assert_eq!(t.to, want);
        let v = store.thread_messages(t.id, sent, ThreadScope::default()).unwrap();
        assert_eq!(v.messages[0].to, want);
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
    fn sender_messages_across_folders() {
        let dir = std::env::temp_dir().join(format!("mail-core-sender-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let a = store.insert_account("me@x.com", "me", Provider::Imap, "h", 993).unwrap();
        let lf = |n: &str, role| ListedFolder { raw_name: n.into(), path: vec![n.into()], role, selectable: true };
        store
            .sync_folder_list(a.id, &[lf("INBOX", Some(FolderRole::Inbox)), lf("News", None), lf("Trash", Some(FolderRole::Trash))])
            .unwrap();
        let id = |n: &str| store.folders(a.id).unwrap().into_iter().find(|f| f.raw_name == n).unwrap().id;
        let m = |uid: u32, from: &str, mid: &str, reply_to: Option<&str>| HeaderRecord {
            gm_msgid: None,
            gm_labels: None,
            message_id: Some(mid.into()),
            in_reply_to: reply_to.map(Into::into),
            from_addr: from.into(),
            ..rec(uid, 0, &[])
        };
        let none = HashMap::new();
        store.upsert_headers(a.id, id("INBOX"), &[m(1, "news@shop.com", "n1@x", None), m(2, "bob@y.com", "b1@x", None)], &none).unwrap();
        store.upsert_headers(a.id, id("News"), &[m(3, "News@Shop.com", "n2@x", None)], &none).unwrap();
        store.upsert_headers(a.id, id("Trash"), &[m(4, "news@shop.com", "n3@x", None)], &none).unwrap();
        store.upsert_headers(a.id, id("INBOX"), &[m(5, "me@x.com", "r1@x", Some("b1@x"))], &none).unwrap();
        let first = |f: &str| store.list_messages(id(f), 0, 10).unwrap().messages;
        let n1 = first("INBOX").into_iter().find(|x| x.from_addr == "news@shop.com").unwrap();
        let page = store.sender_messages(n1.id, 0, 10).unwrap().unwrap();
        assert_eq!(page.addr, "news@shop.com");
        assert_eq!(page.total, 2, "any case, any folder, not what's only in Trash");
        assert_eq!(page.messages.iter().map(|x| x.folder_id).collect::<Vec<_>>(), [id("News"), id("INBOX")], "newest first, in its folder");
        // Our own reply: whom we replied to.
        let mine = first("INBOX").into_iter().find(|x| x.from_addr == "me@x.com").unwrap();
        let page = store.sender_messages(mine.id, 0, 10).unwrap().unwrap();
        assert_eq!((page.addr.as_str(), page.total), ("bob@y.com", 1));
        assert!(store.sender_messages(999_999, 0, 10).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn address_book_cards() {
        use crate::addressbook::Card;
        let dir = std::env::temp_dir().join(format!("mail-core-cards-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let card = |name: &str, emails: &[&str]| Card { name: name.into(), emails: emails.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        let bob = store.save_card(card("bob", &["bob@x.com", " BOB@x.com"])).unwrap();
        let asa = store.save_card(card("Åsa", &["asa@x.se"])).unwrap();
        assert!(store.save_card(card(" ", &[])).is_err(), "needs something to show");
        let all = store.cards().unwrap();
        assert_eq!(all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["bob", "Åsa"], "by name, any case");
        assert_eq!(all[0].emails, ["bob@x.com"], "the same address once");
        store.save_card(Card { id: asa, company: "Acme".into(), phones: vec!["+46 70".into()], ..card("Åsa B", &["asa@x.se"]) }).unwrap();
        let a = store.cards().unwrap().into_iter().find(|c| c.id == asa).unwrap();
        assert_eq!((a.name.as_str(), a.company.as_str(), a.phones.len()), ("Åsa B", "Acme", 1));
        store.delete_card(bob).unwrap();
        assert_eq!(store.cards().unwrap().len(), 1);
        assert!(store.save_card(Card { id: bob, ..card("x", &[]) }).is_err(), "gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signatures_and_defaults() {
        let dir = std::env::temp_dir().join(format!("mail-core-sigs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let a = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        let work = store.save_signature(None, " Work ", "<p><b>Lars</b></p>").unwrap();
        let home = store.save_signature(None, "Home", "<p>L</p>").unwrap();
        assert!(store.save_signature(None, "  ", "x").is_err(), "needs a name");
        store.save_signature(Some(home), "Home", "<p>Lars</p>").unwrap();
        store.set_signature_defaults(a.id, Some(work), Some(home)).unwrap();
        let s = store.signatures().unwrap();
        assert_eq!(s.signatures.iter().map(|x| (x.name.as_str(), x.html.as_str())).collect::<Vec<_>>(), [("Work", "<p><b>Lars</b></p>"), ("Home", "<p>Lars</p>")]);
        assert_eq!((s.defaults[0].new_id, s.defaults[0].reply_id), (Some(work), Some(home)));
        store.delete_signature(work).unwrap();
        let s = store.signatures().unwrap();
        assert_eq!(s.signatures.len(), 1);
        assert_eq!((s.defaults[0].new_id, s.defaults[0].reply_id), (None, Some(home)), "a deleted default goes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_logos() {
        let dir = std::env::temp_dir().join(format!("mail-core-logos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let a = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        assert!(store.set_account_logo(a.id, Some("data:text/html;base64,PGI+")).is_err(), "only pictures");
        assert!(store.set_account_logo(a.id, Some("https://x.com/a.png")).is_err());
        store.set_account_logo(a.id, Some("data:image/png;base64,iVBORw0KGgo=")).unwrap();
        assert_eq!(store.account_logos().unwrap(), vec![(a.id, "data:image/png;base64,iVBORw0KGgo=".to_string())]);
        store.set_account_logo(a.id, None).unwrap();
        assert!(store.account_logos().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn images_always_from_a_sender() {
        let dir = std::env::temp_dir().join(format!("mail-core-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        store
            .sync_folder_list(acct.id, &[ListedFolder { raw_name: "INBOX".into(), path: vec!["INBOX".into()], role: Some(FolderRole::Inbox), selectable: true }])
            .unwrap();
        let inbox = store.folders(acct.id).unwrap()[0].id;
        let recs: Vec<HeaderRecord> = (1..=3u32)
            .map(|u| HeaderRecord { from_addr: if u == 3 { "other@y.com".into() } else { "News@Shop.com".into() }, message_id: Some(format!("r{u}@x")), ..rec(u, 0, &[]) })
            .collect();
        store.upsert_headers(acct.id, inbox, &recs, &HashMap::new()).unwrap();
        let ids: Vec<i64> = store.list_messages(inbox, 0, 10).unwrap().messages.iter().map(|m| m.id).collect();
        assert!(ids.iter().all(|&id| !store.allow_remote(id, false, true).unwrap()));
        store.set_remote_sender("news@shop.com", true).unwrap();
        let from_shop = |id: i64| store.message_from_addr(id).unwrap().unwrap() == "News@Shop.com";
        for &id in &ids {
            assert_eq!(store.allow_remote(id, false, true).unwrap(), from_shop(id), "case aside, only that sender");
            // Where the provider confirms senders and didn't confirm this one: the rule doesn't count.
            assert!(!store.allow_remote(id, false, false).unwrap(), "unconfirmed sender");
        }
        assert_eq!(store.remote_senders().unwrap(), ["news@shop.com"]);
        store.set_remote_sender("NEWS@shop.com", false).unwrap();
        assert!(store.remote_senders().unwrap().is_empty());
        // Everyone at a domain: its addresses and its subdomains', not a name that ends the same.
        let more: Vec<HeaderRecord> = [(4u32, "ann@Y.com"), (5, "bo@mail.y.com"), (6, "cy@xy.com")]
            .iter()
            .map(|&(u, from)| HeaderRecord { from_addr: from.into(), message_id: Some(format!("r{u}@x")), ..rec(u, 0, &[]) })
            .collect();
        store.upsert_headers(acct.id, inbox, &more, &HashMap::new()).unwrap();
        store.set_remote_sender("@y.com", true).unwrap();
        for m in store.list_messages(inbox, 0, 10).unwrap().messages {
            let from = store.message_from_addr(m.id).unwrap().unwrap();
            let want = matches!(from.as_str(), "other@y.com" | "ann@Y.com" | "bo@mail.y.com");
            assert_eq!(store.allow_remote(m.id, true, true).unwrap(), want, "{from}");
            // Not vouched for: the domain's rule doesn't count (and no one here is allowed alone).
            assert!(!store.allow_remote(m.id, false, true).unwrap(), "{from}, unverified");
        }
        assert_eq!(store.remote_senders().unwrap(), ["@y.com"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sorted_lists_positions_and_groups_agree() {
        let dir = std::env::temp_dir().join(format!("mail-core-sorted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        store
            .sync_folder_list(acct.id, &[ListedFolder { raw_name: "INBOX".into(), path: vec!["INBOX".into()], role: Some(FolderRole::Inbox), selectable: true }])
            .unwrap();
        let inbox = store.folders(acct.id).unwrap()[0].id;
        let names = ["Åsa", "bob", "Alice", "", "BOB", "zed", "\"Alice\""];
        let tos = ["Doe, Jane <j@x>", "", "bob@y", "jane@x, k@x", "Örjan <o@x>"];
        let recs: Vec<HeaderRecord> = (1..=60u32)
            .map(|u| HeaderRecord {
                gm_msgid: None,
                gm_labels: None,
                message_id: Some(format!("s{u}@x")),
                from_name: names[u as usize % names.len()].into(),
                from_addr: format!("p{}@x", u % 4),
                to_addrs: tos[u as usize % tos.len()].into(),
                date: (u / 5) as i64,
                flags: if u % 3 == 0 { 0 } else { flags::SEEN },
                // Sizes across the ranges, with ties (placed by date then).
                size: [300, 30_000, 300_000, 3_000_000, 30_000_000, 30_000][u as usize % 6],
                has_attachments: u % 4 == 0,
                ..rec(u, 0, &[])
            })
            .collect();
        store.upsert_headers(acct.id, inbox, &recs, &HashMap::new()).unwrap();
        let all = ListFilter::default();
        let unread_only = ListFilter { read: Some(false), ..Default::default() };
        let with_files = ListFilter { attachments: true, ..Default::default() };
        // Kept: an unread one without attachments, which the filter would leave out.
        let odd = store.list_messages(inbox, 0, 100).unwrap().messages.into_iter().find(|m| m.unread && !m.has_attachments).unwrap().id;
        let kept = ListFilter { read: Some(true), attachments: true, keep: vec![odd], ..Default::default() };
        for sort in [ListSort::Received, ListSort::From, ListSort::To, ListSort::Size] {
            for filter in [&all, &unread_only, &with_files, &kept] {
                let list = store.list_messages_by(inbox, sort, filter, 0, 100).unwrap().messages;
                let want = |m: &MessageSummary| {
                    filter.keep.contains(&m.id)
                        || (filter.read.is_none_or(|r| r == !m.unread) && (!filter.attachments || m.has_attachments))
                };
                let full = store.list_messages_by(inbox, sort, &all, 0, 100).unwrap().messages;
                assert_eq!(full.len(), 60);
                // The filter keeps the order and takes out what it doesn't want.
                assert_eq!(
                    list.iter().map(|m| m.id).collect::<Vec<_>>(),
                    full.iter().filter(|m| want(m)).map(|m| m.id).collect::<Vec<_>>(),
                    "{sort:?} {filter:?}"
                );
                if filter == &kept {
                    assert!(list.iter().any(|m| m.id == odd), "the kept one shows though unread");
                }
                // Pages line up with the whole, and the total with the rows.
                let page = store.list_messages_by(inbox, sort, filter, 0, 10).unwrap();
                assert_eq!(page.total as usize, list.len(), "{sort:?} {filter:?}");
                let paged: Vec<i64> = (0..6).flat_map(|p| store.list_messages_by(inbox, sort, filter, p * 10, 10).unwrap().messages).map(|m| m.id).collect();
                assert_eq!(paged, list.iter().map(|m| m.id).collect::<Vec<_>>(), "{sort:?} {filter:?}");
                for (i, m) in list.iter().enumerate() {
                    assert_eq!(store.index_of_by(inbox, m.id, false, sort, filter).unwrap(), Some(i as u32), "{sort:?} {filter:?}");
                }
                if let Some(out) = full.iter().find(|m| !want(m)) {
                    assert_eq!(store.index_of_by(inbox, out.id, false, sort, filter).unwrap(), None, "filtered out");
                }
                // The groups cover the list.
                let groups = store.list_groups(inbox, false, sort, filter, &[10, 5]).unwrap();
                assert_eq!(groups.iter().map(|g| g.count).sum::<u32>() as usize, list.len(), "{sort:?} {filter:?}");
                // N walks the unread ones in list order, wrapping; P the other way.
                let unread: Vec<i64> = list.iter().filter(|m| m.unread).map(|m| m.id).collect();
                if filter.read == Some(true) {
                    // Showing read mail only: N has nothing to go to (the kept one is open).
                    assert_eq!(store.next_unread_by(inbox, None, false, false, sort, filter).unwrap(), None);
                    continue;
                }
                let mut at = None;
                for want in unread.iter().chain(unread.first()) {
                    let (id, idx) = store.next_unread_by(inbox, at, false, false, sort, filter).unwrap().unwrap();
                    assert_eq!((id, list[idx as usize].id), (*want, *want), "{sort:?} {filter:?}");
                    at = Some(id);
                }
                let (back, _) = store.next_unread_by(inbox, Some(unread[0]), false, true, sort, filter).unwrap().unwrap();
                assert_eq!(back, *unread.last().unwrap(), "{sort:?} back wraps to the bottom");
            }
        }
        // Every row's id, in order, for ranges and Select All.
        for (sort, filter) in [(ListSort::Received, &all), (ListSort::Size, &unread_only), (ListSort::From, &with_files)] {
            let ids: Vec<i64> = store.list_messages_by(inbox, sort, filter, 0, 100).unwrap().messages.iter().map(|m| m.id).collect();
            assert_eq!(store.list_item_ids(inbox, false, sort, filter, 0, 1000).unwrap(), ids);
            assert_eq!(store.list_item_ids(inbox, false, sort, filter, 2, 3).unwrap(), ids[2..5.min(ids.len())]);
        }
        let threads: Vec<i64> = store.list_threads(inbox, ThreadScope::default(), 0, 100).unwrap().threads.iter().map(|t| t.id).collect();
        assert_eq!(store.list_item_ids(inbox, true, ListSort::Received, &all, 0, 1000).unwrap(), threads);
        let picked = store.selection_messages(inbox, &threads[..2], true).unwrap();
        assert!(picked.len() >= 2, "a conversation's messages here");
        assert_eq!(store.selection_messages(inbox, &[threads[0]], false).unwrap().len(), 1, "a message id is just that one");
        // The senders' folders, most first: all 60 are here, from p0..p3 (15 each).
        let one = store.list_messages(inbox, 0, 1).unwrap().messages[0].id;
        assert_eq!(store.sender_folder_counts(&[one]).unwrap(), vec![(inbox, 15)]);
        // By size: largest first, in ranges.
        let by_size = store.list_messages_by(inbox, ListSort::Size, &all, 0, 100).unwrap().messages;
        assert!(by_size.windows(2).all(|w| w[0].size >= w[1].size));
        let ranges = store.list_groups(inbox, false, ListSort::Size, &all, &[]).unwrap();
        assert_eq!(ranges[0].label.as_deref(), Some("Huge (over 10 MB)"));
        let mut i = 0;
        for g in &ranges {
            for m in &by_size[i..i + g.count as usize] {
                assert_eq!(crate::sort::SIZE_RANGES[crate::sort::size_range(m.size)].1, g.label.as_deref().unwrap());
            }
            i += g.count as usize;
        }
        // Conversations: the filter's ones, placed where index_of says.
        for filter in [&all, &unread_only, &with_files] {
            let threads = store.list_threads_filtered(inbox, ThreadScope::default(), filter, 0, 100).unwrap();
            assert_eq!(threads.total as usize, threads.threads.len());
            for (i, t) in threads.threads.iter().enumerate() {
                assert_eq!(store.index_of_by(inbox, t.id, true, ListSort::Received, filter).unwrap(), Some(i as u32), "{filter:?}");
            }
            let g = store.list_groups(inbox, true, ListSort::Received, filter, &[5]).unwrap();
            assert_eq!(g.iter().map(|g| g.count).sum::<u32>(), threads.total);
        }
        // By sender: one group per sender as shown, case and quotes aside, nobody last; the
        // groups cover the list in order.
        let by_from = store.list_messages_by(inbox, ListSort::From, &all, 0, 100).unwrap().messages;
        let groups = store.list_groups(inbox, false, ListSort::From, &all, &[]).unwrap();
        let keys: Vec<String> = groups.iter().map(|g| crate::sort::key(g.label.as_deref().unwrap())).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "one group per key, in order: {keys:?}");
        assert!(keys.contains(&"alice".to_string()) && !keys.contains(&"\"alice\"".to_string()), "quotes aside");
        assert!(keys.contains(&"bob".to_string()) && keys.iter().filter(|k| *k == "bob").count() == 1, "case aside");
        let mut i = 0;
        for g in &groups {
            let want = crate::sort::key(g.label.as_deref().unwrap());
            for m in &by_from[i..i + g.count as usize] {
                assert_eq!(crate::sort::sender_key(&m.from_name, &m.from_addr), want);
            }
            i += g.count as usize;
        }
        assert_eq!(i, 60);
        let to_groups = store.list_groups(inbox, false, ListSort::To, &all, &[]).unwrap();
        assert_eq!(to_groups.iter().map(|g| g.count).sum::<u32>(), 60);
        assert_eq!(to_groups.last().unwrap().label.as_deref(), Some(""), "no recipients last");
        // By date: periods split at the bounds, newest first.
        let by_date = store.list_groups(inbox, false, ListSort::Received, &all, &[10, 5]).unwrap();
        let count = |f: &dyn Fn(i64) -> bool| recs.iter().filter(|r| f(r.date)).count() as u32;
        assert_eq!(by_date.iter().map(|g| g.count).collect::<Vec<_>>(), [count(&|d| d >= 10), count(&|d| (5..10).contains(&d)), count(&|d| d < 5)]);
        let threads = store.list_groups(inbox, true, ListSort::From, &all, &[5]).unwrap();
        assert_eq!(threads.iter().map(|g| g.count).sum::<u32>(), store.list_threads(inbox, ThreadScope::default(), 0, 100).unwrap().total);
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
        // Where a message is among the results, without loading them: as the page shows it.
        let page = store.search(acct.id, None, "ada", &[], 0, 50).unwrap().messages;
        for (i, m) in page.iter().enumerate() {
            assert_eq!(store.search_index_of(acct.id, None, "ada", &[], m.id).unwrap(), Some(i as u32));
        }
        let draft = store.search(acct.id, None, "draft", &[], 0, 50).unwrap().messages[0].id;
        assert_eq!(store.search_index_of(acct.id, None, "ada", &[], draft).unwrap(), None, "not one of them");
        assert_eq!(store.search_index_of(acct.id, None, "", &[], draft).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_in_the_text() {
        let dir = std::env::temp_dir().join(format!("mail-core-search-text-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        store
            .sync_folder_list(acct.id, &[ListedFolder { raw_name: "INBOX".into(), path: vec!["INBOX".into()], role: Some(FolderRole::Inbox), selectable: true }])
            .unwrap();
        let inbox = store.folders(acct.id).unwrap()[0].id;
        let mk = |uid: u32, subject: &str| HeaderRecord { gm_msgid: None, gm_labels: None, subject: subject.into(), from_addr: "bob@corp.com".into(), ..rec(uid, 0, &[]) };
        store.upsert_headers(acct.id, inbox, &[mk(1, "Lunch"), mk(2, "Newsletter"), mk(3, "Refund policy"), mk(4, "Files")], &HashMap::new()).unwrap();
        let id = |subject: &str| -> i64 { store.db().query_row("SELECT id FROM messages WHERE subject=?1", [subject], |r| r.get(0)).unwrap() };
        let hits = |q: &str| -> Vec<String> {
            store.search(acct.id, None, q, &[], 0, 50).unwrap().messages.into_iter().map(|m| m.subject).collect::<std::collections::BTreeSet<_>>().into_iter().collect()
        };
        store.store_body(id("Lunch"), b"Subject: Lunch\r\nContent-Type: text/plain\r\n\r\nShall we ask about the refund over lunch?\r\n", None, false, None).unwrap();
        store
            .store_body(
                id("Newsletter"),
                b"Subject: Newsletter\r\nContent-Type: text/html\r\n\r\n<html><body><p>Meet the <b>kangaroo</b> of the month</p></body></html>\r\n",
                None,
                false,
                None,
            )
            .unwrap();
        store
            .store_body(
                id("Files"),
                b"Subject: Files\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nAttached.\r\n--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"quarterly-figures.pdf\"\r\n\r\n%PDF\r\n--b--\r\n",
                None,
                true,
                None,
            )
            .unwrap();
        // Not searchable until indexed.
        assert!(hits("kangaroo").is_empty());
        assert_eq!(store.bodies_to_index().unwrap(), 3);
        assert_eq!(store.index_bodies(2).unwrap(), 2);
        assert_eq!(store.index_bodies(100).unwrap(), 1);
        assert_eq!(store.index_bodies(100).unwrap(), 0);

        assert_eq!(hits("kangaroo"), ["Newsletter"], "HTML as text");
        assert_eq!(hits("refund"), ["Lunch", "Refund policy"], "in the text, or in the subject");
        assert_eq!(hits("body:refund"), ["Lunch"]);
        assert_eq!(hits("subject:refund"), ["Refund policy"]);
        assert_eq!(hits("refund -lunch"), ["Refund policy"], "a word in the subject excludes");
        assert_eq!(hits("refund -shall"), ["Refund policy"], "a word in the text excludes");
        assert_eq!(hits("quarterly"), ["Files"], "attachments by name");
        assert_eq!(hits("bob refund"), ["Lunch", "Refund policy"], "terms from either place");
        let only = |fields: &[&str]| store.search(acct.id, None, "refund", fields, 0, 50).unwrap().total;
        assert_eq!((only(&["subject"]), only(&["body"]), only(&["subject", "body"])), (1, 1, 2));

        // A header change (a draft retitled) keeps the text searchable.
        store.db().execute("UPDATE messages SET subject='Lunch!' WHERE subject='Lunch'", []).unwrap();
        assert_eq!(hits("body:refund"), ["Lunch!"]);
        // A new body: the old text goes with the next round.
        store.store_body(id("Lunch!"), b"Subject: Lunch\r\nContent-Type: text/plain\r\n\r\nPizza it is.\r\n", None, false, None).unwrap();
        assert_eq!(store.index_bodies(100).unwrap(), 1);
        assert!(hits("body:refund").is_empty());
        assert_eq!(hits("pizza"), ["Lunch!"]);
        // A message gone: its text too.
        store.db().execute("DELETE FROM messages WHERE subject='Newsletter'", []).unwrap();
        let left: i64 = store.db().query_row("SELECT count(*) FROM body_fts WHERE body_fts MATCH 'kangaroo'", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_in_attachments() {
        let dir = std::env::temp_dir().join(format!("mail-core-search-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let acct = store.insert_account("x@example.com", "x", Provider::Imap, "h", 993).unwrap();
        store
            .sync_folder_list(acct.id, &[ListedFolder { raw_name: "INBOX".into(), path: vec!["INBOX".into()], role: Some(FolderRole::Inbox), selectable: true }])
            .unwrap();
        let inbox = store.folders(acct.id).unwrap()[0].id;
        let mk = |uid: u32, subject: &str| HeaderRecord { gm_msgid: None, gm_labels: None, subject: subject.into(), ..rec(uid, 0, &[]) };
        store.upsert_headers(acct.id, inbox, &[mk(1, "Whole"), mk(2, "Partial")], &HashMap::new()).unwrap();
        store.set_attachment_search(true).unwrap();
        let id = |subject: &str| -> i64 { store.db().query_row("SELECT id FROM messages WHERE subject=?1", [subject], |r| r.get(0)).unwrap() };
        let hits = |q: &str| -> Vec<String> { store.search(acct.id, None, q, &[], 0, 50).unwrap().messages.into_iter().map(|m| m.subject).collect() };
        let b64 = |d: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, d);
        // Stored with its PDF.
        let whole = format!(
            "Subject: Whole\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nHi\r\n--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--b--\r\n",
            b64(&crate::mime::tests::tiny_pdf("wombat lease"))
        );
        store.store_body(id("Whole"), whole.as_bytes(), None, true, None).unwrap();
        // Stored without its PDF, which is downloaded later.
        let omitted = r#"[{"name":"later.pdf","mime":"application/pdf","size":900}]"#;
        store.store_body(id("Partial"), b"Subject: Partial\r\nContent-Type: text/plain\r\n\r\nSee attached\r\n", None, true, Some(omitted)).unwrap();
        while store.index_bodies(100).unwrap() > 0 {}
        assert_eq!(hits("wombat"), ["Whole"], "the text of a stored PDF");
        assert!(hits("echidna").is_empty());
        store.cache_attachment(id("Partial"), 0, &crate::mime::tests::tiny_pdf("echidna census")).unwrap();
        assert_eq!(store.bodies_to_index().unwrap(), 1, "indexed again once downloaded");
        while store.index_bodies(100).unwrap() > 0 {}
        assert_eq!(hits("echidna census"), ["Partial"]);
        assert_eq!(hits("attachment:echidna"), ["Partial"]);
        assert!(hits("body:echidna").is_empty(), "the message's text is apart");
        let only = |q: &str, fields: &[&str]| store.search(acct.id, None, q, fields, 0, 50).unwrap().total;
        assert_eq!((only("echidna", &["attachments"]), only("echidna", &["body"]), only("echidna", &["subject", "attachments"])), (1, 0, 1));
        assert_eq!(only("later", &["attachments"]), 1, "by name");

        // Once, after the upgrade: messages with attachments, indexed before, are done again.
        store.db().execute("DELETE FROM settings WHERE key='attachment_fts_indexed'", []).unwrap();
        assert_eq!(store.index_attachment_text_once().unwrap(), 2);
        assert_eq!(store.index_attachment_text_once().unwrap(), 0, "only once");
        while store.index_bodies(100).unwrap() > 0 {}
        assert_eq!(hits("wombat"), ["Whole"]);

        // Turned off: their text goes from the index, their names stay; on again, it's back.
        store.set_attachment_search(false).unwrap();
        while store.index_bodies(100).unwrap() > 0 {}
        assert!(hits("wombat").is_empty() && hits("echidna").is_empty(), "no text in attachments");
        assert_eq!(hits("attachment:later"), ["Partial"], "names still");
        store.set_attachment_search(true).unwrap();
        while store.index_bodies(100).unwrap() > 0 {}
        assert_eq!((hits("wombat"), hits("echidna")), (vec!["Whole".to_string()], vec!["Partial".to_string()]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Indexing every body of a real database copy, and searching it after:
    /// MAIL_DB_DIR=/path cargo test --release -p mail-core real_db_body_index -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_db_body_index() {
        let dir = std::path::PathBuf::from(std::env::var("MAIL_DB_DIR").unwrap());
        let store = Store::open(&dir).unwrap();
        let size = || std::fs::metadata(dir.join("mail.db")).unwrap().len() / 1_000_000;
        let (todo, before) = (store.bodies_to_index().unwrap(), size());
        let t = std::time::Instant::now();
        let mut done = 0;
        loop {
            let n = store.index_bodies(100).unwrap();
            if n == 0 {
                break;
            }
            done += n;
        }
        let _ = store.db().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        println!("{done} of {todo} bodies in {:?}; mail.db {before} MB -> {} MB", t.elapsed(), size());
        for a in store.accounts().unwrap() {
            for q in ["a", "re", "invoice", "meeting", "body:password", "\"pull request\"", "from:google security"] {
                let t = std::time::Instant::now();
                let r = store.search(a.id, None, q, &[], 0, 200).unwrap();
                println!("{:<28} search {:<24} {:>7} hits  {:>6.1?}", a.email.split('@').nth(1).unwrap_or(""), q, r.total, t.elapsed());
            }
        }
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700, "the user's alone");
        }
        // At the base (or later): kept, and brought up to date by the steps since.
        {
            let db = Connection::open(dir.join("mail.db")).unwrap();
            db.execute_batch(&format!(
                "ALTER TABLE accounts DROP COLUMN keep_attachments; ALTER TABLE accounts DROP COLUMN auth;
                 ALTER TABLE accounts DROP COLUMN keywords; ALTER TABLE accounts DROP COLUMN position;
                 ALTER TABLE accounts DROP COLUMN mark_letter; ALTER TABLE accounts DROP COLUMN mark_color;
                 ALTER TABLE accounts DROP COLUMN graph;
                 ALTER TABLE messages DROP COLUMN attachments_stored;
                 DROP INDEX messages_unindexed; ALTER TABLE messages DROP COLUMN body_indexed;
                 DROP TABLE identities;
                 CREATE TABLE identities (id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL, email TEXT NOT NULL,
                   name TEXT NOT NULL, new_sig INTEGER, reply_sig INTEGER, is_default INTEGER NOT NULL DEFAULT 0);
                 DROP TABLE signature_defaults; DROP TABLE signatures;
                 DROP TABLE remote_senders; DROP TABLE settings;
                 PRAGMA user_version={BASE_VERSION};"
            ))
            .unwrap();
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.accounts().unwrap().len(), 1);
        assert!(!store.keeps_attachments(store.accounts().unwrap()[0].id).unwrap());
        assert!(store.signatures().unwrap().signatures.is_empty());
        assert!(store.identities().unwrap().is_empty(), "other addresses: None can be chosen");
        assert!(!store.domain_rules().unwrap(), "no rule for a domain: domain rules stay off");
        drop(store);
        assert_eq!(version(&dir), SCHEMA_VERSION);
        // Someone who had a rule for a domain before they became a setting keeps them working.
        {
            let db = Connection::open(dir.join("mail.db")).unwrap();
            db.execute_batch(&format!(
                "DROP TABLE settings; INSERT INTO remote_senders (addr) VALUES ('@example.net');
                 PRAGMA user_version={};",
                13 // before step 14 (settings)
            ))
            .unwrap();
        }
        assert!(Store::open(&dir).unwrap().domain_rules().unwrap());
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

