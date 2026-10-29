//! User actions on messages (archive, delete, move, spam, read/unread, star).
//!
//! An action is applied to the local store at once (the UI never waits) and its server side is
//! queued as [`ServerOp`]s in `pending_ops`, sent in order on the interactive connection. Moves
//! wait [`UNDO_WINDOW_MS`] first, so undo just cancels the queued ops and restores the snapshot.
//!
//! Gmail and other servers differ in what an action means on the server:
//!
//! | action  | Gmail                                             | other IMAP                       |
//! |---------|---------------------------------------------------|----------------------------------|
//! | archive | remove \Inbox and user labels ("unfiled")          | MOVE to the \Archive folder      |
//! | trash   | MOVE to [Gmail]/Trash                              | MOVE to \Trash                   |
//! | move    | remove current label, add target label             | MOVE to target                   |
//! | spam    | MOVE to [Gmail]/Spam                               | MOVE to \Junk                    |
//! | flags   | STORE once (flags are per message)                 | STORE on every copy              |
//!
//! Gmail operations address a message by its All Mail UID when it has one, since that copy
//! carries every label; messages in Trash/Spam (not in All Mail) use their copy there.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::model::{Folder, FolderRole, flags};
use crate::store::{LocalChange, Store};

/// Moves wait this long before being sent, so they can be undone without a server round trip.
pub const UNDO_WINDOW_MS: i64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Action {
    Archive,
    Trash,
    /// Only from Trash or Spam: remove from the server for good.
    DeleteForever,
    #[serde(rename_all = "camelCase")]
    Move { target_folder_id: i64 },
    Spam,
    NotSpam,
    Read,
    Unread,
    Flag,
    Unflag,
    /// Add (`on`) or remove a tag keyword (Thunderbird-compatible IMAP keyword).
    Tag { keyword: String, on: bool },
}

impl Action {
    fn is_flags(&self) -> bool {
        matches!(self, Action::Read | Action::Unread | Action::Flag | Action::Unflag | Action::Tag { .. })
    }
}

/// One server command, addressed by folder raw name and UID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum ServerOp {
    /// Gmail: change labels (`add`/`remove` are X-GM-LABELS values, e.g. `\Inbox`, `Work`).
    Labels { folder: String, uid: u32, add: Vec<String>, remove: Vec<String> },
    Move { folder: String, uid: u32, target: String },
    /// `flag` is an IMAP flag such as `\Seen`.
    Flags { folder: String, uid: u32, flag: String, on: bool },
    /// \Deleted + UID EXPUNGE (permanent).
    Expunge { folder: String, uid: u32 },
    /// Gmail: run `then` on the message's copy in `folder` (All Mail), found by X-GM-MSGID when
    /// sent, because its UID there wasn't known yet. Label changes must go to All Mail: sent to
    /// the Inbox (or a label) copy, removing that folder's own label is ignored (`OK`, no change).
    Find { folder: String, gm_msgid: u64, then: Box<ServerOp> },
}

impl ServerOp {
    /// The same command addressed to `uid` (for `Find`, once the UID is known).
    pub fn with_uid(&self, uid: u32) -> ServerOp {
        let mut op = self.clone();
        match &mut op {
            ServerOp::Labels { uid: u, .. } | ServerOp::Move { uid: u, .. } | ServerOp::Flags { uid: u, .. } | ServerOp::Expunge { uid: u, .. } => *u = uid,
            ServerOp::Find { .. } => {}
        }
        op
    }
}

/// What applying an action produced.
pub struct Planned {
    pub change: LocalChange,
    pub ops: Vec<ServerOp>,
    /// Folders touched on the server (re-synced once the ops are sent).
    pub folders: Vec<i64>,
    pub delay_ms: i64,
}

/// Gmail X-GM-LABELS value for a label-backed folder.
fn gmail_label(f: &Folder) -> Option<String> {
    Some(match f.role {
        Some(FolderRole::Inbox) => "\\Inbox".into(),
        Some(FolderRole::Sent) => "\\Sent".into(),
        Some(FolderRole::Drafts) => "\\Draft".into(),
        Some(FolderRole::Important) => "\\Important".into(),
        Some(FolderRole::Flagged) => "\\Starred".into(),
        Some(FolderRole::All | FolderRole::Trash | FolderRole::Junk) => return None,
        Some(FolderRole::Archive) | None => f.raw_name.clone(),
    })
}

fn find_role(folders: &[Folder], role: FolderRole, gmail: bool) -> Option<&Folder> {
    let mut matching = folders.iter().filter(|f| f.role == Some(role) && f.selectable);
    if gmail {
        // Prefer Gmail's own system folder if a user folder happens to share the role.
        let all: Vec<&Folder> = matching.collect();
        all.iter()
            .copied()
            .find(|f| f.raw_name.starts_with("[Gmail]/") || f.raw_name.starts_with("[Google Mail]/"))
            .or(all.first().copied())
    } else {
        matching.next()
    }
}

/// Plan an action on `message_ids`, viewed in `context` (the folder on screen).
pub fn plan(
    store: &Store,
    account_id: i64,
    gmail: bool,
    action: Action,
    message_ids: &[i64],
    context: i64,
) -> Result<Planned> {
    let folders = store.folders(account_id)?;
    let by_id: HashMap<i64, &Folder> = folders.iter().map(|f| (f.id, f)).collect();
    let ctx = *by_id.get(&context).ok_or_else(|| anyhow!("unknown folder"))?;
    let role = |r| find_role(&folders, r, gmail);
    let all_mail = if gmail { role(FolderRole::All) } else { None };

    let mut change = LocalChange { message_ids: message_ids.to_vec(), ..Default::default() };
    let mut ops = Vec::new();
    let mut touched: HashSet<i64> = HashSet::from([context]);

    for &id in message_ids {
        let in_folders: Vec<i64> = store.message_folder_ids(id)?;
        let copies = store.message_copies(id)?; // (folder id, raw, uid)
        let copy_in = |fid: i64| copies.iter().find(|c| c.0 == fid).map(|c| (c.1.clone(), c.2));
        // A conversation's revealed messages may live outside the folder on screen: use the
        // copy in view when there is one, else the message's own copy (non-Trash first).
        let any_copy = || {
            let is_bin = |fid: &i64| by_id.get(fid).is_some_and(|f| matches!(f.role, Some(FolderRole::Trash | FolderRole::Junk)));
            copies.iter().find(|c| !is_bin(&c.0)).or(copies.first()).map(|c| (c.0, c.1.clone(), c.2))
        };
        // Gmail: All Mail's copy (see `ServerOp::Find`) with its UID, or found by X-GM-MSGID when
        // sent if that UID isn't known yet (new mail synced through the Inbox first).
        let all_mail_copy = all_mail.and_then(|a| copies.iter().find(|c| c.0 == a.id)).map(|c| (c.0, c.1.clone(), c.2));
        // (Messages in Trash/Spam aren't in All Mail: those use their copy there, a real folder.)
        let in_bin = in_folders.iter().any(|f| by_id.get(f).is_some_and(|f| matches!(f.role, Some(FolderRole::Trash | FolderRole::Junk))));
        let find: Option<u64> = match (gmail, &all_mail_copy, all_mail) {
            (true, None, Some(_)) if !in_bin => store.message_gm_msgid(id)?,
            _ => None,
        };
        // Server commands for a Find are built against All Mail with UID 0, then wrapped.
        let wrap = |op: ServerOp| match (find, all_mail) {
            (Some(gm_msgid), Some(a)) => ServerOp::Find { folder: a.raw_name.clone(), gm_msgid, then: Box::new(op) },
            _ => op,
        };
        // The copy server commands address: (folder id, raw name, uid).
        let primary = if gmail {
            all_mail_copy
                .clone()
                .or_else(|| find.and(all_mail).map(|a| (a.id, a.raw_name.clone(), 0)))
                .or_else(|| copies.iter().find(|c| c.0 == context).map(|c| (c.0, c.1.clone(), c.2)))
                .or_else(any_copy)
        } else {
            copies.iter().find(|c| c.0 == context).map(|c| (c.0, c.1.clone(), c.2)).or_else(any_copy)
        };
        let need_primary = || {
            primary
                .clone()
                .map(|(_, raw, uid)| (raw, uid))
                .ok_or_else(|| anyhow!("This message isn't on the server yet; try again after it syncs."))
        };
        let move_to = |target: &Folder, change: &mut LocalChange, ops: &mut Vec<ServerOp>, touched: &mut HashSet<i64>| -> Result<()> {
            // Already there (e.g. a revealed message that's in Trash): nothing to do.
            if in_folders.contains(&target.id) {
                return Ok(());
            }
            let (source, folder, uid) =
                primary.clone().ok_or_else(|| anyhow!("This message isn't on the server yet; try again after it syncs."))?;
            ops.push(wrap(ServerOp::Move { folder, uid, target: target.raw_name.clone() }));
            if gmail {
                // In Gmail a message lives in one of Inbox/labels/All Mail, Trash or Spam.
                for &f in &in_folders {
                    change.remove.push((f, id));
                    touched.insert(f);
                }
            } else {
                // Only the moved copy leaves; other copies stay where they are.
                change.remove.push((source, id));
                touched.insert(source);
            }
            change.add.push((target.id, id));
            touched.insert(target.id);
            Ok(())
        };

        match action.clone() {
            // Out of Trash/Spam, "unfiled" means back into All Mail with no labels.
            Action::Archive if gmail && in_folders.iter().any(|f| by_id.get(f).is_some_and(|f| matches!(f.role, Some(FolderRole::Trash | FolderRole::Junk)))) => {
                let target = all_mail.ok_or_else(|| anyhow!("This account has no All Mail folder."))?;
                move_to(target, &mut change, &mut ops, &mut touched)?;
            }
            Action::Archive if gmail => {
                // Unfiled: drop \Inbox and user labels; keep Sent/Drafts/Starred/Important.
                let mut remove = Vec::new();
                for &f in &in_folders {
                    let Some(folder) = by_id.get(&f) else { continue };
                    if matches!(folder.role, Some(FolderRole::Inbox) | None | Some(FolderRole::Archive)) {
                        if let Some(label) = gmail_label(folder) {
                            remove.push(label);
                            change.remove.push((f, id));
                            touched.insert(f);
                        }
                    }
                }
                if !remove.is_empty() {
                    let (folder, uid) = need_primary()?;
                    ops.push(wrap(ServerOp::Labels { folder, uid, add: vec![], remove }));
                }
            }
            Action::Archive => {
                let target = role(FolderRole::Archive).ok_or_else(|| anyhow!("This account has no Archive folder."))?;
                if target.id != context {
                    move_to(target, &mut change, &mut ops, &mut touched)?;
                }
            }
            Action::Trash => {
                let target = role(FolderRole::Trash).ok_or_else(|| anyhow!("This account has no Trash folder."))?;
                if target.id == context {
                    bail!("This message is already in the Trash.");
                }
                move_to(target, &mut change, &mut ops, &mut touched)?;
            }
            Action::DeleteForever => {
                if !matches!(ctx.role, Some(FolderRole::Trash | FolderRole::Junk)) {
                    bail!("Messages can only be deleted for good from Trash or Spam.");
                }
                // Only the copy in this folder; revealed messages elsewhere are left alone.
                if let Some((folder, uid)) = copy_in(context) {
                    ops.push(ServerOp::Expunge { folder, uid });
                    change.remove.push((context, id));
                }
            }
            Action::Move { target_folder_id } => {
                let target = *by_id.get(&target_folder_id).ok_or_else(|| anyhow!("unknown folder"))?;
                if target.id == context {
                    continue;
                }
                match (gmail, target.role) {
                    (true, Some(FolderRole::Trash)) => return plan(store, account_id, gmail, Action::Trash, message_ids, context),
                    (true, Some(FolderRole::Junk)) => return plan(store, account_id, gmail, Action::Spam, message_ids, context),
                    (true, Some(FolderRole::All)) => return plan(store, account_id, gmail, Action::Archive, message_ids, context),
                    (true, _) if matches!(ctx.role, Some(FolderRole::Trash | FolderRole::Junk)) => {
                        // Out of Trash/Spam: a real move, then the label applies.
                        move_to(target, &mut change, &mut ops, &mut touched)?;
                    }
                    (true, _) => {
                        let add = gmail_label(target).ok_or_else(|| anyhow!("Can't move to {}.", target.display_name))?;
                        // Drop the label being viewed only from messages that carry it.
                        let remove: Vec<String> =
                            if in_folders.contains(&context) { gmail_label(ctx).into_iter().collect() } else { vec![] };
                        let (folder, uid) = need_primary()?;
                        ops.push(wrap(ServerOp::Labels { folder, uid, add: vec![add], remove }));
                        if ctx.role != Some(FolderRole::All) && in_folders.contains(&context) {
                            change.remove.push((context, id));
                        }
                        change.add.push((target.id, id));
                        touched.insert(target.id);
                    }
                    (false, _) => move_to(target, &mut change, &mut ops, &mut touched)?,
                }
            }
            Action::Spam => {
                let target = role(FolderRole::Junk).ok_or_else(|| anyhow!("This account has no Spam folder."))?;
                if target.id != context {
                    move_to(target, &mut change, &mut ops, &mut touched)?;
                }
            }
            Action::NotSpam => {
                let inbox = role(FolderRole::Inbox).ok_or_else(|| anyhow!("no Inbox"))?;
                if ctx.role != Some(FolderRole::Junk) {
                    bail!("Not spam only applies to messages in Spam.");
                }
                // Revealed messages that aren't in Spam are left alone.
                if let Some((folder, uid)) = copy_in(context) {
                    ops.push(ServerOp::Move { folder, uid, target: inbox.raw_name.clone() });
                    change.remove.push((context, id));
                    change.add.push((inbox.id, id));
                    touched.insert(inbox.id);
                }
            }
            Action::Tag { keyword, on } => {
                let keyword = &keyword;
                let k = crate::tags::normalize(keyword);
                if !crate::tags::is_tag(&k) || k.chars().any(|c| !c.is_ascii_graphic() || "(){%*\"\\]".contains(c)) {
                    bail!("\"{keyword}\" can't be used as a tag.");
                }
                change.tags.push((id, k.clone(), on));
                let flag = k.clone();
                if gmail {
                    let (folder, uid) = need_primary()?;
                    ops.push(wrap(ServerOp::Flags { folder, uid, flag, on }));
                } else {
                    // Each copy has its own keywords.
                    for (fid, folder, uid) in &copies {
                        ops.push(ServerOp::Flags { folder: folder.clone(), uid: *uid, flag: flag.clone(), on });
                        touched.insert(*fid);
                    }
                }
            }
            Action::Read | Action::Unread | Action::Flag | Action::Unflag => {
                let (bit, flag, on) = match action {
                    Action::Read => (flags::SEEN, "\\Seen", true),
                    Action::Unread => (flags::SEEN, "\\Seen", false),
                    Action::Flag => (flags::FLAGGED, "\\Flagged", true),
                    _ => (flags::FLAGGED, "\\Flagged", false),
                };
                change.flags.push((id, if on { bit } else { 0 }, if on { 0 } else { bit }));
                if gmail {
                    let (folder, uid) = need_primary()?;
                    ops.push(wrap(ServerOp::Flags { folder, uid, flag: flag.into(), on }));
                } else {
                    // Each copy has its own flags.
                    for (fid, folder, uid) in &copies {
                        ops.push(ServerOp::Flags { folder: folder.clone(), uid: *uid, flag: flag.into(), on });
                        touched.insert(*fid);
                    }
                }
                // Starred is a Gmail folder backed by \Flagged.
                if bit == flags::FLAGGED {
                    if let Some(starred) = role(FolderRole::Flagged) {
                        if on { change.add.push((starred.id, id)) } else { change.remove.push((starred.id, id)) }
                        touched.insert(starred.id);
                    }
                }
            }
        }
    }

    Ok(Planned {
        change,
        ops,
        folders: touched.into_iter().collect(),
        delay_ms: if action.is_flags() { 0 } else { UNDO_WINDOW_MS },
    })
}

/// IMAP quoting for a label or mailbox name in a command.
pub fn quote(s: &str) -> String {
    if s.starts_with('\\') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '\\') {
        return s.to_string(); // system label: \Inbox
    }
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;
    use crate::store::{HeaderRecord, ListedFolder};

    fn rec(uid: u32, gm: Option<u64>, labels: Option<&[&str]>, mid: &str, flags: i64) -> HeaderRecord {
        HeaderRecord {
            uid,
            gm_msgid: gm,
            email_id: None,
            gm_labels: labels.map(|l| l.iter().map(|s| s.to_string()).collect()),
            keywords: vec![],
            message_id: Some(mid.into()),
            in_reply_to: None,
            refs: None,
            subject: format!("m{uid}"),
            from_name: String::new(),
            from_addr: "a@b.c".into(),
            to_addrs: String::new(),
            date: uid as i64,
            size: 10,
            flags,
            has_attachments: false,
        }
    }

    fn lf(name: &str, role: Option<FolderRole>) -> ListedFolder {
        ListedFolder { raw_name: name.into(), path: vec![name.into()], role, selectable: true }
    }

    struct Env {
        store: Store,
        acct: i64,
        dir: std::path::PathBuf,
    }
    impl Env {
        fn new(tag: &str, provider: Provider, folders: &[ListedFolder]) -> Env {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("mail-core-actions-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let store = Store::open(&dir).unwrap();
            let acct = store.insert_account("x@example.com", "x", provider, "h", 993).unwrap().id;
            store.sync_folder_list(acct, folders).unwrap();
            Env { store, acct, dir }
        }
        fn id(&self, name: &str) -> i64 {
            self.store.folders(self.acct).unwrap().into_iter().find(|f| f.raw_name == name).unwrap().id
        }
        fn folder_names(&self, msg: i64) -> Vec<String> {
            let mut v: Vec<String> = self
                .store
                .message_folder_ids(msg)
                .unwrap()
                .into_iter()
                .map(|f| self.store.folders(self.acct).unwrap().into_iter().find(|x| x.id == f).unwrap().raw_name)
                .collect();
            v.sort();
            v
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn gmail_env() -> (Env, i64) {
        let env = Env::new(
            "gmail",
            Provider::Gmail,
            &[
                lf("INBOX", Some(FolderRole::Inbox)),
                lf("[Gmail]/All Mail", Some(FolderRole::All)),
                lf("[Gmail]/Trash", Some(FolderRole::Trash)),
                lf("[Gmail]/Spam", Some(FolderRole::Junk)),
                lf("[Gmail]/Sent Mail", Some(FolderRole::Sent)),
                lf("Work", None),
                lf("Travel", None),
            ],
        );
        let folders = env.store.folders(env.acct).unwrap();
        let labels = crate::sync::gmail_label_map(&folders);
        let r = rec(900, Some(1), Some(&["\\Inbox", "Work", "\\Sent"]), "a@x", 0);
        env.store.upsert_headers(env.acct, env.id("[Gmail]/All Mail"), &[r.clone()], &labels).unwrap();
        env.store.upsert_headers(env.acct, env.id("INBOX"), &[HeaderRecord { uid: 5, ..r }], &labels).unwrap();
        let msg = env.store.message_ids_for_uids(env.id("INBOX"), &[5]).unwrap()[0];
        (env, msg)
    }

    #[test]
    fn gmail_archive_unfiles_and_undo_restores() {
        let (env, msg) = gmail_env();
        let inbox = env.id("INBOX");
        assert_eq!(env.folder_names(msg), ["INBOX", "Work", "[Gmail]/All Mail", "[Gmail]/Sent Mail"]);
        let before = env.store.snapshot(&[msg]).unwrap();

        let p = plan(&env.store, env.acct, true, Action::Archive, &[msg], inbox).unwrap();
        assert_eq!(
            p.ops,
            vec![ServerOp::Labels {
                folder: "[Gmail]/All Mail".into(),
                uid: 900,
                add: vec![],
                remove: vec!["\\Inbox".into(), "Work".into()]
            }]
        );
        assert_eq!(p.delay_ms, UNDO_WINDOW_MS);
        env.store.apply_local(&p.change).unwrap();
        assert_eq!(env.folder_names(msg), ["[Gmail]/All Mail", "[Gmail]/Sent Mail"]);

        env.store.restore(&before).unwrap();
        assert_eq!(env.folder_names(msg), ["INBOX", "Work", "[Gmail]/All Mail", "[Gmail]/Sent Mail"]);
    }

    #[test]
    fn gmail_trash_move_and_flags() {
        let (env, msg) = gmail_env();
        let inbox = env.id("INBOX");

        let p = plan(&env.store, env.acct, true, Action::Trash, &[msg], inbox).unwrap();
        assert_eq!(p.ops, vec![ServerOp::Move { folder: "[Gmail]/All Mail".into(), uid: 900, target: "[Gmail]/Trash".into() }]);
        let mut c = p.change.clone();
        env.store.apply_local(&c).unwrap();
        assert_eq!(env.folder_names(msg), ["[Gmail]/Trash"]);
        c.remove.clear();

        let (env, msg) = gmail_env();
        let p = plan(&env.store, env.acct, true, Action::Move { target_folder_id: env.id("Travel") }, &[msg], env.id("INBOX")).unwrap();
        assert_eq!(
            p.ops,
            vec![ServerOp::Labels { folder: "[Gmail]/All Mail".into(), uid: 900, add: vec!["Travel".into()], remove: vec!["\\Inbox".into()] }]
        );
        env.store.apply_local(&p.change).unwrap();
        assert_eq!(env.folder_names(msg), ["Travel", "Work", "[Gmail]/All Mail", "[Gmail]/Sent Mail"]);

        let p = plan(&env.store, env.acct, true, Action::Read, &[msg], env.id("Travel")).unwrap();
        assert_eq!(p.ops, vec![ServerOp::Flags { folder: "[Gmail]/All Mail".into(), uid: 900, flag: "\\Seen".into(), on: true }]);
        assert_eq!(p.delay_ms, 0);
        env.store.apply_local(&p.change).unwrap();
        assert_eq!(env.store.message_flags(msg).unwrap() & flags::SEEN, flags::SEEN);
    }

    #[test]
    fn gmail_new_mail_known_only_in_inbox_is_addressed_in_all_mail() {
        // Synced through the Inbox (IDLE) before All Mail: no All Mail UID yet. Removing \Inbox
        // from the Inbox copy is ignored by Gmail, so the command must find the All Mail copy.
        let env = gmail_env().0;
        let folders = env.store.folders(env.acct).unwrap();
        let labels = crate::sync::gmail_label_map(&folders);
        let r = rec(77, Some(42), Some(&["\\Inbox", "Work"]), "new@x", 0);
        env.store.upsert_headers(env.acct, env.id("INBOX"), &[r], &labels).unwrap();
        let msg = env.store.message_ids_for_uids(env.id("INBOX"), &[77]).unwrap()[0];
        let find = |then: ServerOp| ServerOp::Find { folder: "[Gmail]/All Mail".into(), gm_msgid: 42, then: Box::new(then) };

        let p = plan(&env.store, env.acct, true, Action::Archive, &[msg], env.id("INBOX")).unwrap();
        assert_eq!(
            p.ops,
            vec![find(ServerOp::Labels { folder: "[Gmail]/All Mail".into(), uid: 0, add: vec![], remove: vec!["\\Inbox".into(), "Work".into()] })]
        );
        let p = plan(&env.store, env.acct, true, Action::Trash, &[msg], env.id("INBOX")).unwrap();
        assert_eq!(p.ops, vec![find(ServerOp::Move { folder: "[Gmail]/All Mail".into(), uid: 0, target: "[Gmail]/Trash".into() })]);
        let p = plan(&env.store, env.acct, true, Action::Read, &[msg], env.id("INBOX")).unwrap();
        assert_eq!(p.ops, vec![find(ServerOp::Flags { folder: "[Gmail]/All Mail".into(), uid: 0, flag: "\\Seen".into(), on: true })]);

        // Queued as JSON in pending_ops; the UID is filled in when it's sent.
        let op = &p.ops[0];
        assert_eq!(&serde_json::from_str::<ServerOp>(&serde_json::to_string(op).unwrap()).unwrap(), op);
        let ServerOp::Find { then, .. } = op else { unreachable!() };
        assert_eq!(then.with_uid(9), ServerOp::Flags { folder: "[Gmail]/All Mail".into(), uid: 9, flag: "\\Seen".into(), on: true });
    }

    #[test]
    fn sync_step_from_before_an_action_cannot_undo_it() {
        let (env, msg) = gmail_env();
        let inbox = env.id("INBOX");
        let labels = crate::sync::gmail_label_map(&env.store.folders(env.acct).unwrap());
        // A sync step starts, then the user deletes; the step's server data still has \Inbox.
        let epoch = env.store.local_epoch();
        let p = plan(&env.store, env.acct, true, Action::Trash, &[msg], inbox).unwrap();
        env.store.apply_local(&p.change).unwrap();
        let stale = rec(900, Some(1), Some(&["\\Inbox", "Work", "\\Sent"]), "a@x", 0);
        let all = env.id("[Gmail]/All Mail");
        let e = env.store.upsert_headers_since(Some(epoch), env.acct, all, &[stale.clone()], &labels).unwrap_err();
        assert!(e.is::<crate::store::SyncSuperseded>());
        let change = crate::store::FlagChange { uid: 900, flags: 0, keywords: vec![], labels: Some(vec!["\\Inbox".into()]) };
        let e = env.store.update_flags_since(Some(epoch), inbox, &[change], &labels).unwrap_err();
        assert!(e.is::<crate::store::SyncSuperseded>());
        assert_eq!(env.folder_names(msg), ["[Gmail]/Trash"]);
        // Queuing the server op counts too (a step may start between the two).
        let epoch = env.store.local_epoch();
        env.store.enqueue_ops(env.acct, "[]", "{}", "[]", 0).unwrap();
        assert!(env.store.upsert_headers_since(Some(epoch), env.acct, all, &[stale.clone()], &labels).is_err());
        // A step that starts afterwards writes normally.
        let epoch = env.store.local_epoch();
        env.store.upsert_headers_since(Some(epoch), env.acct, all, &[stale], &labels).unwrap();
    }

    #[test]
    fn next_unread_goes_down_then_wraps() {
        let env = Env::new("unread", Provider::Imap, &[lf("INBOX", Some(FolderRole::Inbox))]);
        let inbox = env.id("INBOX");
        // List order is newest first: uids 5,4,3,2,1; 4 and 1 unread.
        let recs: Vec<HeaderRecord> =
            (1..=5).map(|u| rec(u, None, None, &format!("m{u}@x"), if u == 4 || u == 1 { 0 } else { flags::SEEN })).collect();
        env.store.upsert_headers(env.acct, inbox, &recs, &HashMap::new()).unwrap();
        let id = |u: u32| env.store.message_ids_for_uids(inbox, &[u]).unwrap()[0];
        // Each message is its own conversation; the list shows uid u at row 5 - u.
        let threads = env.store.list_threads(inbox, crate::model::ThreadScope::default(), 0, 10).unwrap().threads;
        for threaded in [false, true] {
            let key = |u: u32| if threaded { threads[5 - u as usize].id } else { id(u) };
            let next = |after: Option<u32>| env.store.next_unread(inbox, after.map(key), threaded).unwrap();
            assert_eq!(next(None), Some((key(4), 1)));
            assert_eq!(next(Some(5)), Some((key(4), 1)));
            assert_eq!(next(Some(4)), Some((key(1), 4)));
            assert_eq!(next(Some(2)), Some((key(1), 4)));
            assert_eq!(next(Some(1)), Some((key(4), 1)), "wraps to the top");
        }
        let seen = Env::new("noneunread", Provider::Imap, &[lf("INBOX", Some(FolderRole::Inbox))]);
        seen.store.upsert_headers(seen.acct, seen.id("INBOX"), &[rec(1, None, None, "a@x", flags::SEEN)], &HashMap::new()).unwrap();
        assert_eq!(seen.store.next_unread(seen.id("INBOX"), None, false).unwrap(), None);
    }

    #[test]
    fn imap_moves_the_copy_in_view_and_flags_every_copy() {
        let env = Env::new(
            "imap",
            Provider::Imap,
            &[lf("INBOX", Some(FolderRole::Inbox)), lf("Archive", Some(FolderRole::Archive)), lf("Trash", Some(FolderRole::Trash)), lf("Projects", None)],
        );
        let none = HashMap::new();
        let r = rec(7, None, None, "b@x", 0);
        env.store.upsert_headers(env.acct, env.id("INBOX"), &[r.clone()], &none).unwrap();
        env.store.upsert_headers(env.acct, env.id("Projects"), &[HeaderRecord { uid: 3, ..r }], &none).unwrap();
        let msg = env.store.message_ids_for_uids(env.id("INBOX"), &[7]).unwrap()[0];
        assert_eq!(env.folder_names(msg), ["INBOX", "Projects"]);

        let p = plan(&env.store, env.acct, false, Action::Archive, &[msg], env.id("INBOX")).unwrap();
        assert_eq!(p.ops, vec![ServerOp::Move { folder: "INBOX".into(), uid: 7, target: "Archive".into() }]);
        env.store.apply_local(&p.change).unwrap();
        assert_eq!(env.folder_names(msg), ["Archive", "Projects"]);

        let p = plan(&env.store, env.acct, false, Action::Flag, &[msg], env.id("Projects")).unwrap();
        // Archive's copy has no UID yet (arrives on its next sync); Projects' copy is flagged.
        assert_eq!(p.ops, vec![ServerOp::Flags { folder: "Projects".into(), uid: 3, flag: "\\Flagged".into(), on: true }]);

        let none_archive = Env::new("noarch", Provider::Imap, &[lf("INBOX", Some(FolderRole::Inbox))]);
        none_archive.store.upsert_headers(none_archive.acct, none_archive.id("INBOX"), &[rec(1, None, None, "c@x", 0)], &none).unwrap();
        let m = none_archive.store.message_ids_for_uids(none_archive.id("INBOX"), &[1]).unwrap()[0];
        assert!(plan(&none_archive.store, none_archive.acct, false, Action::Archive, &[m], none_archive.id("INBOX")).is_err());
    }

    #[test]
    fn revealed_messages_in_other_folders() {
        // Gmail: trashing a conversation that includes a message already in Trash.
        let (env, msg) = gmail_env();
        let folders = env.store.folders(env.acct).unwrap();
        let labels = crate::sync::gmail_label_map(&folders);
        env.store.upsert_headers(env.acct, env.id("[Gmail]/Trash"), &[rec(44, Some(2), Some(&[]), "t@x", 0)], &labels).unwrap();
        let trashed = env.store.message_ids_for_uids(env.id("[Gmail]/Trash"), &[44]).unwrap()[0];
        let p = plan(&env.store, env.acct, true, Action::Trash, &[msg, trashed], env.id("INBOX")).unwrap();
        assert_eq!(p.ops, vec![ServerOp::Move { folder: "[Gmail]/All Mail".into(), uid: 900, target: "[Gmail]/Trash".into() }]);

        // IMAP: archiving a conversation viewed in INBOX that includes a message from Sent.
        let env = Env::new(
            "reveal",
            Provider::Imap,
            &[lf("INBOX", Some(FolderRole::Inbox)), lf("Sent", Some(FolderRole::Sent)), lf("Archive", Some(FolderRole::Archive))],
        );
        let none = HashMap::new();
        env.store.upsert_headers(env.acct, env.id("INBOX"), &[rec(1, None, None, "a@x", 0)], &none).unwrap();
        env.store.upsert_headers(env.acct, env.id("Sent"), &[rec(9, None, None, "b@x", 0)], &none).unwrap();
        let a = env.store.message_ids_for_uids(env.id("INBOX"), &[1]).unwrap()[0];
        let b = env.store.message_ids_for_uids(env.id("Sent"), &[9]).unwrap()[0];
        let p = plan(&env.store, env.acct, false, Action::Archive, &[a, b], env.id("INBOX")).unwrap();
        assert_eq!(
            p.ops,
            vec![
                ServerOp::Move { folder: "INBOX".into(), uid: 1, target: "Archive".into() },
                ServerOp::Move { folder: "Sent".into(), uid: 9, target: "Archive".into() },
            ]
        );
        env.store.apply_local(&p.change).unwrap();
        assert_eq!(env.folder_names(a), ["Archive"]);
        assert_eq!(env.folder_names(b), ["Archive"]);
    }

    #[test]
    fn gmail_archive_from_trash_restores_to_all_mail() {
        let (env, _) = gmail_env();
        let folders = env.store.folders(env.acct).unwrap();
        let labels = crate::sync::gmail_label_map(&folders);
        env.store.upsert_headers(env.acct, env.id("[Gmail]/Trash"), &[rec(44, Some(2), Some(&[]), "t@x", 0)], &labels).unwrap();
        let m = env.store.message_ids_for_uids(env.id("[Gmail]/Trash"), &[44]).unwrap()[0];
        let p = plan(&env.store, env.acct, true, Action::Archive, &[m], env.id("[Gmail]/Trash")).unwrap();
        assert_eq!(p.ops, vec![ServerOp::Move { folder: "[Gmail]/Trash".into(), uid: 44, target: "[Gmail]/All Mail".into() }]);
        env.store.apply_local(&p.change).unwrap();
        assert_eq!(env.folder_names(m), ["[Gmail]/All Mail"]);
    }

    #[test]
    fn tags_sync_act_undo_search() {
        let (env, msg) = gmail_env();
        let folders = env.store.folders(env.acct).unwrap();
        let labels = crate::sync::gmail_label_map(&folders);
        let all = env.id("[Gmail]/All Mail");
        // Thunderbird tagged it "Work" plus a custom tag elsewhere; junk markers aren't tags.
        env.store
            .update_flags(
                all,
                &[crate::store::FlagChange { uid: 900, flags: 0, keywords: vec!["$Label2".into(), "projectx".into(), "$Forwarded".into()], labels: Some(vec!["\\Inbox".into()]) }],
                &labels,
            )
            .unwrap();
        let row = env.store.list_messages(env.id("INBOX"), 0, 10).unwrap().messages.remove(0);
        assert_eq!(row.tags, vec!["$label2", "projectx"]);

        // Definitions: Thunderbird's five, plus the undefined custom keyword.
        let defs = env.store.tags().unwrap();
        assert_eq!(defs.iter().filter(|d| d.defined).count(), 5);
        assert!(defs.iter().any(|d| d.keyword == "projectx" && !d.defined));

        // Tag as To Do: one STORE on the All Mail copy, sent immediately.
        let before = env.store.snapshot(&[msg]).unwrap();
        let p = plan(&env.store, env.acct, true, Action::Tag { keyword: "$label4".into(), on: true }, &[msg], env.id("INBOX")).unwrap();
        assert_eq!(p.ops, vec![ServerOp::Flags { folder: "[Gmail]/All Mail".into(), uid: 900, flag: "$label4".into(), on: true }]);
        assert_eq!(p.delay_ms, 0);
        env.store.apply_local(&p.change).unwrap();
        assert_eq!(env.store.message_tags(msg).unwrap(), vec!["$label2", "$label4", "projectx"]);

        // Search by tag name, by keyword, and combined with text.
        let hits = |q: &str| env.store.search(env.acct, None, q, &[], 0, 10).unwrap().total;
        assert_eq!(hits("tag:work"), 1);
        assert_eq!(hits("tag:\"to do\""), 1);
        assert_eq!(hits("tag:projectx m900"), 1);
        assert_eq!(hits("tag:later"), 0);

        env.store.restore(&before).unwrap();
        assert_eq!(env.store.message_tags(msg).unwrap(), vec!["$label2", "projectx"]);

        // Custom tag definition gets Thunderbird's keyword for its name.
        assert_eq!(env.store.set_tag(None, "Waiting On", "#123456").unwrap(), "waiting_on");
        assert!(plan(&env.store, env.acct, true, Action::Tag { keyword: "bad tag".into(), on: true }, &[msg], env.id("INBOX")).is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("\\Inbox"), "\\Inbox");
        assert_eq!(quote("Work/Q \"1\""), "\"Work/Q \\\"1\\\"\"");
    }
}
