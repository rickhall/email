//! Mail core: local store, IMAP sync engine and MIME rendering, independent of any UI.

pub mod imap;
pub mod actions;
pub mod mime;
pub mod model;
pub mod partial;
pub mod search;
pub mod store;
pub mod sync;
pub mod tags;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use tokio::sync::broadcast;

use crate::imap::{Credentials, ServerConfig};
use crate::model::*;
use crate::store::Store;
use crate::sync::{AccountSync, HeaderSync, SyncOptions};

const KEYRING_SERVICE: &str = "dev.mail-client.imap";

/// Where account secrets live.
pub enum Secrets {
    /// OS keychain (macOS Keychain, Secret Service on Linux).
    Keyring,
    /// In-memory only; for tests and benchmarks.
    Memory(Mutex<HashMap<String, String>>),
}

impl Secrets {
    pub fn memory() -> Secrets {
        Secrets::Memory(Mutex::new(HashMap::new()))
    }

    fn set(&self, email: &str, password: &str) -> Result<()> {
        match self {
            Secrets::Keyring => keyring::Entry::new(KEYRING_SERVICE, email)?
                .set_password(password)
                .context("saving password to the system keychain"),
            Secrets::Memory(m) => {
                m.lock().unwrap().insert(email.into(), password.into());
                Ok(())
            }
        }
    }

    fn get(&self, email: &str) -> Result<String> {
        match self {
            Secrets::Keyring => keyring::Entry::new(KEYRING_SERVICE, email)?
                .get_password()
                .context("reading password from the system keychain"),
            Secrets::Memory(m) => m
                .lock()
                .unwrap()
                .get(email)
                .cloned()
                .ok_or_else(|| anyhow!("no password stored for {email}")),
        }
    }

    fn delete(&self, email: &str) {
        match self {
            Secrets::Keyring => {
                if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, email) {
                    let _ = e.delete_credential();
                }
            }
            Secrets::Memory(m) => {
                m.lock().unwrap().remove(email);
            }
        }
    }
}

pub struct NewAccount {
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
}

pub struct Core {
    pub store: Arc<Store>,
    events: broadcast::Sender<CoreEvent>,
    secrets: Secrets,
    strategy: Arc<dyn HeaderSync>,
    options: SyncOptions,
    running: Mutex<HashMap<i64, AccountSync>>,
    /// Accounts with no usable stored password (e.g. missing from the keychain).
    needs_password: Mutex<std::collections::HashSet<i64>>,
}

impl Core {
    pub fn open(data_dir: &Path, secrets: Secrets) -> Result<Core> {
        Self::with_strategy(data_dir, secrets, Arc::new(sync::chunked::ChunkedNewestFirst::default()), SyncOptions::default())
    }

    pub fn with_strategy(
        data_dir: &Path,
        secrets: Secrets,
        strategy: Arc<dyn HeaderSync>,
        options: SyncOptions,
    ) -> Result<Core> {
        let (events, _) = broadcast::channel(4096);
        let store = Store::open(data_dir)?;
        // Actions interrupted mid-send (quit, crash) are sent again.
        store.reset_started_ops()?;
        Ok(Core {
            store: Arc::new(store),
            events,
            secrets,
            strategy,
            options,
            running: Mutex::new(HashMap::new()),
            needs_password: Mutex::new(Default::default()),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }

    fn server_config(&self, account: &Account) -> Result<ServerConfig> {
        Ok(ServerConfig {
            host: account.imap_host.clone(),
            port: account.imap_port,
            user: account.email.clone(),
            credentials: Credentials::Password(self.secrets.get(&account.email)?),
        })
    }

    /// Start syncing every stored account. Accounts whose secret is missing are skipped.
    pub fn start(&self) -> Result<()> {
        for account in self.store.accounts()? {
            if let Err(e) = self.start_account(account.clone()) {
                tracing::warn!(account = account.email, "not starting sync: {e:#}");
                self.needs_password.lock().unwrap().insert(account.id);
            }
        }
        Ok(())
    }

    fn start_account(&self, account: Account) -> Result<()> {
        let server = self.server_config(&account)?;
        let sync = AccountSync::start(
            self.store.clone(),
            account.clone(),
            server,
            self.strategy.clone(),
            self.events.clone(),
            self.options.clone(),
        );
        self.running.lock().unwrap().insert(account.id, sync);
        Ok(())
    }

    /// Verify credentials against the server, then save the account and start syncing.
    pub async fn add_account(&self, new: NewAccount) -> Result<Account> {
        let email = new.email.trim().to_string();
        let known = imap::discover_server(&email).await;
        let host = new
            .imap_host
            .filter(|h| !h.trim().is_empty())
            .or_else(|| known.map(|k| k.0.to_string()))
            .ok_or_else(|| anyhow!("Couldn't find the mail server for {email}; enter the IMAP server under Advanced."))?;
        let port = new.imap_port.or(known.map(|k| k.1)).unwrap_or(993);

        let cfg = ServerConfig {
            host: host.clone(),
            port,
            user: email.clone(),
            credentials: Credentials::Password(new.password.clone()),
        };
        let conn = imap::connect(&cfg).await?;
        let provider = if conn.caps.gmail {
            Provider::Gmail
        } else {
            known.map(|k| k.2).unwrap_or(Provider::Imap)
        };
        conn.logout().await;

        self.secrets.set(&email, &new.password)?;
        let display_name = new.display_name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| email.clone());
        let account = self.store.insert_account(&email, &display_name, provider, &host, port)?;
        if let Some(old) = self.running.lock().unwrap().remove(&account.id) {
            old.stop();
        }
        self.start_account(account.clone())?;
        let _ = self.events.send(CoreEvent::FoldersChanged { account_id: account.id });
        Ok(account)
    }

    /// Replace a rejected or missing password: verify it, store it, restart the account's sync.
    pub async fn update_password(&self, account_id: i64, password: String) -> Result<()> {
        let account = self
            .store
            .accounts()?
            .into_iter()
            .find(|a| a.id == account_id)
            .ok_or_else(|| anyhow!("no such account"))?;
        let cfg = ServerConfig {
            host: account.imap_host.clone(),
            port: account.imap_port,
            user: account.email.clone(),
            credentials: Credentials::Password(password.clone()),
        };
        match imap::connect(&cfg).await {
            Ok(conn) => conn.logout().await,
            Err(e) if imap::is_auth_error(&e) => {
                return Err(anyhow!("That password was rejected. Check it and try again."));
            }
            Err(e) if imap::is_limit_error(&e) => {
                return Err(anyhow!(
                    "The server refused the connection because this account has too many simultaneous \
                     connections (other mail apps or devices). The password wasn't checked; close another \
                     mail app or wait a minute and try again."
                ));
            }
            Err(e) => return Err(e),
        }
        self.secrets.set(&account.email, &password)?;
        if let Some(old) = self.running.lock().unwrap().remove(&account_id) {
            old.stop();
        }
        self.needs_password.lock().unwrap().remove(&account_id);
        self.start_account(account)?;
        Ok(())
    }

    pub fn remove_account(&self, account_id: i64) -> Result<()> {
        if let Some(s) = self.running.lock().unwrap().remove(&account_id) {
            s.stop();
        }
        if let Some(a) = self.store.accounts()?.into_iter().find(|a| a.id == account_id) {
            self.secrets.delete(&a.email);
        }
        self.store.delete_account(account_id)
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        self.store.accounts()
    }

    pub fn folders(&self, account_id: i64) -> Result<Vec<Folder>> {
        self.store.folders(account_id)
    }

    pub fn open_folder(&self, folder_id: i64) -> Result<()> {
        let (account_id, _) = self.store.folder(folder_id)?.ok_or_else(|| anyhow!("no such folder"))?;
        if let Some(s) = self.running.lock().unwrap().get(&account_id) {
            s.focus(folder_id);
        }
        Ok(())
    }

    pub fn list_messages(&self, folder_id: i64, offset: u32, limit: u32) -> Result<MessagePage> {
        self.store.list_messages(folder_id, offset, limit)
    }

    /// An attachment's (name, mime, bytes): from the stored message, the attachment cache, or
    /// (for attachments omitted from the body download) fetched from the server and cached.
    pub async fn attachment(&self, message_id: i64, index: usize) -> Result<(String, String, Vec<u8>)> {
        let raw = self
            .store
            .body(message_id)?
            .ok_or_else(|| anyhow!("This message hasn't been downloaded yet."))?;
        let stored = mime::stored_attachment_count(&raw);
        if index < stored {
            return mime::stored_attachment(&raw, index).ok_or_else(|| anyhow!("attachment not found"));
        }
        let omitted: Vec<partial::OmittedPart> = self
            .store
            .omitted(message_id)?
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let ordinal = index - stored;
        let part = omitted.get(ordinal).ok_or_else(|| anyhow!("attachment not found"))?;
        if let Some(data) = self.store.cached_attachment(message_id, index)? {
            return Ok((part.name.clone(), part.mime.clone(), data));
        }
        let account_id = self.store.message_account(message_id)?.ok_or_else(|| anyhow!("no such message"))?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = sync::PartRequest {
            message_id,
            ordinal,
            section: part.section.clone(),
            encoding: part.encoding.clone(),
            reply: tx,
        };
        match self.running.lock().unwrap().get(&account_id) {
            Some(s) => s.fetch_part(req),
            None => return Err(anyhow!("This account isn't connected.")),
        }
        let data = rx.await.map_err(|_| anyhow!("download was cancelled"))??;
        self.store.cache_attachment(message_id, index, &data)?;
        Ok((part.name.clone(), part.mime.clone(), data))
    }

    /// Number of attachments (stored + omitted) and the message subject, for "save all".
    pub fn attachment_summary(&self, message_id: i64) -> Result<(usize, String)> {
        let raw = self.store.body(message_id)?.ok_or_else(|| anyhow!("This message hasn't been downloaded yet."))?;
        let stored = mime::stored_attachment_count(&raw);
        let omitted: Vec<partial::OmittedPart> = self
            .store
            .omitted(message_id)?
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let subject = mail_parser::MessageParser::default()
            .parse_headers(&raw)
            .and_then(|m| m.subject().map(str::to_string))
            .unwrap_or_default();
        Ok((stored + omitted.len(), subject))
    }

    /// Apply a user action to messages viewed in `folder_id`: locally at once, on the server via
    /// the outbox (moves after the undo window). Returns the op id to pass to `undo_action`.
    pub fn apply_action(&self, action: actions::Action, message_ids: &[i64], folder_id: i64) -> Result<ActionResult> {
        let (account_id, _) = self.store.folder(folder_id)?.ok_or_else(|| anyhow!("unknown folder"))?;
        let account = self.store.accounts()?.into_iter().find(|a| a.id == account_id).ok_or_else(|| anyhow!("no such account"))?;
        let gmail = account.provider == Provider::Gmail;
        let planned = actions::plan(&self.store, account_id, gmail, action, message_ids, folder_id)?;
        let snapshot = self.store.snapshot(message_ids)?;
        let mut changed = self.store.apply_local(&planned.change)?;
        // Rows show flags and tags, so every folder holding these messages refreshes, not only
        // those whose counts changed (a tag or a non-Gmail star changes no count).
        for id in message_ids {
            changed.extend(self.store.message_folder_ids(*id)?);
        }
        changed.sort_unstable();
        changed.dedup();
        let _ = self.events.send(CoreEvent::MessagesChanged { folder_ids: changed });
        let _ = self.events.send(CoreEvent::FoldersChanged { account_id });
        if planned.ops.is_empty() {
            return Ok(ActionResult { op_id: None, undoable: false });
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
        let op_id = self.store.enqueue_ops(
            account_id,
            &serde_json::to_string(&planned.ops)?,
            &serde_json::to_string(&snapshot)?,
            &serde_json::to_string(&planned.folders)?,
            now + planned.delay_ms,
        )?;
        if let Some(s) = self.running.lock().unwrap().get(&account_id) {
            s.wake_outbox();
        }
        Ok(ActionResult { op_id: Some(op_id), undoable: planned.delay_ms > 0 })
    }

    /// Undo an action that hasn't been sent yet (within the undo window).
    pub fn undo_action(&self, op_id: i64) -> Result<()> {
        let snapshot = self
            .store
            .cancel_op(op_id)?
            .ok_or_else(|| anyhow!("Too late to undo: the change was already sent to the server."))?;
        let snapshot: store::Snapshot = serde_json::from_str(&snapshot)?;
        let mut changed = self.store.restore(&snapshot)?;
        changed.extend(snapshot.entries.iter().map(|e| e.folder_id));
        changed.sort_unstable();
        changed.dedup();
        let _ = self.events.send(CoreEvent::MessagesChanged { folder_ids: changed.clone() });
        for (account_id, _) in changed.iter().filter_map(|f| self.store.folder(*f).ok().flatten()).collect::<std::collections::HashSet<_>>() {
            let _ = self.events.send(CoreEvent::FoldersChanged { account_id });
        }
        Ok(())
    }

    pub fn index_of(&self, folder_id: i64, id: i64, threaded: bool) -> Result<Option<u32>> {
        self.store.index_of(folder_id, id, threaded)
    }

    /// The next unread item below `after` in a folder list (wrapping to the top).
    pub fn next_unread(&self, folder_id: i64, after: Option<i64>, threaded: bool) -> Result<Option<NextUnread>> {
        Ok(self.store.next_unread(folder_id, after, threaded)?.map(|(id, index)| NextUnread { id, index }))
    }

    pub fn tags(&self) -> Result<Vec<tags::TagDef>> {
        self.store.tags()
    }

    pub fn set_tag(&self, keyword: Option<&str>, name: &str, color: &str) -> Result<String> {
        self.store.set_tag(keyword, name, color)
    }

    pub fn delete_tag(&self, keyword: &str) -> Result<()> {
        self.store.delete_tag(keyword)
    }

    pub fn message_in_folder(&self, id: i64, folder_id: Option<i64>) -> Result<bool> {
        self.store.message_in_folder(id, folder_id)
    }

    pub fn list_threads(&self, folder_id: i64, scope: ThreadScope, offset: u32, limit: u32) -> Result<ThreadPage> {
        self.store.list_threads(folder_id, scope, offset, limit)
    }

    pub fn search(
        &self,
        account_id: i64,
        folder_id: Option<i64>,
        query: &str,
        fields: &[&str],
        offset: u32,
        limit: u32,
    ) -> Result<MessagePage> {
        self.store.search(account_id, folder_id, query, fields, offset, limit)
    }

    pub fn thread_messages(&self, thread_id: i64, folder_id: i64, scope: ThreadScope) -> Result<ThreadView> {
        self.store.thread_messages(thread_id, folder_id, scope)
    }

    /// The rendered message, or `None` after queueing its download at top priority.
    pub fn get_message(&self, id: i64) -> Result<Option<MessageDetail>> {
        match self.store.body(id)? {
            Some(raw) => {
                let mut detail = mime::render(id, &raw, self.store.message_date(id)?);
                if let Some(d) = detail.as_mut() {
                    d.remote_allowed = self.store.allow_remote(id)?;
                    d.tags = self.store.message_tags(id)?;
                }
                // Attachments not downloaded yet are still listed, numbered after the stored ones.
                if let (Some(d), Some(json)) = (detail.as_mut(), self.store.omitted(id)?) {
                    let omitted: Vec<partial::OmittedPart> = serde_json::from_str(&json).unwrap_or_default();
                    let first = d.attachments.len();
                    let cached = self.store.cached_attachment_indices(id)?;
                    d.attachments.extend(omitted.into_iter().enumerate().map(|(k, o)| Attachment {
                        index: first + k,
                        openable: mime::is_openable(&o.name),
                        downloaded: cached.contains(&(first + k)),
                        name: o.name,
                        mime: o.mime,
                        size: o.size,
                    }));
                }
                Ok(detail)
            }
            None => {
                let account_id = self.store.message_account(id)?.ok_or_else(|| anyhow!("no such message"))?;
                if let Some(s) = self.running.lock().unwrap().get(&account_id) {
                    s.request_body(id);
                }
                Ok(None)
            }
        }
    }

    /// Remember the user's "Load images" choice for a message.
    pub fn set_allow_remote(&self, id: i64, allow: bool) -> Result<()> {
        self.store.set_allow_remote(id, allow)
    }

    /// Mark a message read or unread: instantly in the local store, then on the server.
    pub fn set_read(&self, id: i64, read: bool) -> Result<()> {
        let Some(folders) = self.store.set_flag(id, flags::SEEN, read)? else {
            return Ok(());
        };
        let account_id = self.store.message_account(id)?.ok_or_else(|| anyhow!("no such message"))?;
        let _ = self.events.send(CoreEvent::MessagesChanged { folder_ids: folders });
        let _ = self.events.send(CoreEvent::FoldersChanged { account_id });
        if let Some(s) = self.running.lock().unwrap().get(&account_id) {
            s.set_seen(id, read);
        }
        Ok(())
    }

    pub fn sync_status(&self) -> Vec<SyncStatus> {
        let mut out: Vec<SyncStatus> = self.running.lock().unwrap().values().map(|s| s.status()).collect();
        for &account_id in self.needs_password.lock().unwrap().iter() {
            out.push(SyncStatus {
                account_id,
                phase: SyncPhase::Auth,
                folder_id: None,
                done: 0,
                total: 0,
                message: Some("No saved password for this account.".into()),
            });
        }
        out
    }
}
