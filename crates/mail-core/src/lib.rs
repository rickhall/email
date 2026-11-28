//! Mail core: local store, IMAP sync engine and MIME rendering, independent of any UI.

pub mod imap;
pub mod actions;
pub mod addressbook;
pub mod ai;
pub mod gcontacts;
pub mod icons;
pub mod identities;
pub mod mcp;
pub mod compose;
pub mod contacts;
pub mod graph;
pub mod mime;
pub mod oauth;
pub mod model;
pub mod partial;
pub mod pdf_helper;
pub mod search;
pub mod signatures;
pub mod smtp;
pub mod sort;
pub mod store;
pub mod sync;
pub mod tags;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::broadcast;

use crate::imap::{Credentials, ServerConfig};
use crate::model::*;
use crate::store::Store;
use crate::sync::{AccountSync, HeaderSync, SyncOptions};

const KEYRING_SERVICE: &str = "dev.mail-client.imap";
/// The one keychain item holding every secret (a JSON object, name → secret). macOS asks
/// whether the app may read an item once per item for each new build of the app; one item, one
/// question.
const VAULT_SERVICE: &str = "dev.mail-client";
const VAULT_NAME: &str = "secrets";

/// Where account secrets live.
pub enum Secrets {
    /// OS keychain (macOS Keychain, Secret Service on Linux): one item for all of them, read
    /// once and kept in memory. Secrets saved by earlier versions (an item each, under
    /// `KEYRING_SERVICE`) move into it as they're first read; those items stay, for older builds.
    Keyring(Mutex<Option<HashMap<String, String>>>),
    /// In-memory only; for tests and benchmarks.
    Memory(Mutex<HashMap<String, String>>),
}

fn vault_entry() -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(VAULT_SERVICE, VAULT_NAME)?)
}

/// Windows' Credential Manager keeps at most 2,560 bytes (1,280 UTF-16 characters) in an item,
/// less than a couple of Microsoft refresh tokens: there the vault goes in pieces of this many
/// characters. Elsewhere it's one item.
const VAULT_CHUNK: usize = if cfg!(windows) { 1000 } else { usize::MAX };
/// In the main item when the vault is in pieces: how many follow (`secrets.1` …).
const VAULT_PARTS: &str = "#vault-parts";

fn vault_part(i: usize) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(VAULT_SERVICE, &format!("{VAULT_NAME}.{i}"))?)
}

/// What to store for the vault's JSON: the JSON itself when it fits in `chunk` characters, else
/// a marker saying how many parts there are, then the parts.
fn vault_pieces(json: &str, chunk: usize) -> Vec<String> {
    if json.chars().count() <= chunk {
        return vec![json.to_string()];
    }
    let chars: Vec<char> = json.chars().collect();
    let parts: Vec<String> = chars.chunks(chunk).map(|c| c.iter().collect()).collect();
    let mut out = vec![format!("{{\"{VAULT_PARTS}\":{}}}", parts.len())];
    out.extend(parts);
    out
}

/// The vault's JSON from its main item, with the parts it names (`part(i)`, from 1).
fn vault_join(first: &str, mut part: impl FnMut(usize) -> Result<String>) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(first).context("reading the app's keychain item")?;
    let Some(n) = v.get(VAULT_PARTS).and_then(serde_json::Value::as_u64) else {
        return Ok(first.to_string());
    };
    let mut json = String::new();
    for i in 1..=n as usize {
        json.push_str(&part(i)?);
    }
    Ok(json)
}

/// The vault as it's in the keychain now: None when there's no item yet.
fn vault_read() -> Result<Option<HashMap<String, String>>> {
    match vault_entry()?.get_password() {
        Ok(first) => {
            let json = vault_join(&first, |i| {
                vault_part(i)?.get_password().with_context(|| format!("reading part {i} of the app's keychain item"))
            })?;
            Ok(Some(serde_json::from_str(&json).context("reading the app's keychain item")?))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("reading the app's secrets from the system keychain")),
    }
}

/// The vault's secrets: read from the keychain the first time.
fn vault_loaded(cache: &mut Option<HashMap<String, String>>) -> Result<&mut HashMap<String, String>> {
    if cache.is_none() {
        *cache = Some(vault_read()?.unwrap_or_default());
    }
    Ok(cache.as_mut().unwrap())
}

/// What to save when one secret changes: the vault as just read from the keychain (`fresh`,
/// with whatever another copy of the app saved meanwhile), with only that change. Never the copy
/// kept in memory (`cached`) as a whole: an old or incomplete one would erase the others. No
/// item at all while this copy knew of secrets is taken for a keychain that didn't answer
/// properly (locked, restarting): refused, rather than saved over as empty.
fn vault_updated(
    cached: Option<&HashMap<String, String>>,
    fresh: Result<Option<HashMap<String, String>>>,
    change: impl FnOnce(&mut HashMap<String, String>) -> bool,
) -> Result<Option<HashMap<String, String>>> {
    let mut map = match fresh? {
        Some(m) => m,
        None if cached.is_some_and(|c| !c.is_empty()) => bail!("The app's keychain item couldn't be read (is the keychain locked?); nothing was saved."),
        None => HashMap::new(),
    };
    Ok(change(&mut map).then_some(map))
}

fn vault_save(map: &HashMap<String, String>) -> Result<()> {
    let pieces = vault_pieces(&serde_json::to_string(map)?, VAULT_CHUNK);
    // The parts first, the item naming them last.
    for (i, p) in pieces.iter().enumerate().skip(1) {
        vault_part(i)?.set_password(p).context("saving to the system keychain")?;
    }
    vault_entry()?.set_password(&pieces[0]).context("saving to the system keychain")?;
    // Parts left over from a longer vault (only where it's split at all).
    if VAULT_CHUNK != usize::MAX {
        let mut i = pieces.len().max(1);
        while let Ok(e) = vault_part(i) {
            if e.delete_credential().is_err() {
                break;
            }
            i += 1;
        }
    }
    Ok(())
}

impl Secrets {
    pub fn memory() -> Secrets {
        Secrets::Memory(Mutex::new(HashMap::new()))
    }

    pub fn keyring() -> Secrets {
        Secrets::Keyring(Mutex::new(None))
    }

    fn set(&self, email: &str, password: &str) -> Result<()> {
        match self {
            Secrets::Keyring(cache) => {
                let mut cache = cache.lock().unwrap();
                let map = vault_updated(cache.as_ref(), vault_read(), |m| {
                    m.insert(email.into(), password.into());
                    true
                })?
                .expect("changed");
                vault_save(&map)?;
                tracing::info!(name = %secret_name(email), entries = map.len(), "keychain: secret saved");
                *cache = Some(map);
                Ok(())
            }
            Secrets::Memory(m) => {
                m.lock().unwrap().insert(email.into(), password.into());
                Ok(())
            }
        }
    }

    fn get(&self, email: &str) -> Result<String> {
        match self {
            Secrets::Keyring(cache) => {
                let mut cache = cache.lock().unwrap();
                if let Some(s) = vault_loaded(&mut cache)?.get(email) {
                    return Ok(s.clone());
                }
                // Not in the copy read before: saved since, perhaps (by another copy of the app).
                if let Some(fresh) = vault_read()? {
                    let found = fresh.get(email).cloned();
                    *cache = Some(fresh);
                    if let Some(s) = found {
                        return Ok(s);
                    }
                }
                // Saved by an earlier version (an item of its own): move it in.
                let old = keyring::Entry::new(KEYRING_SERVICE, email)?
                    .get_password()
                    .context("reading password from the system keychain")?;
                match vault_updated(cache.as_ref(), vault_read(), |m| {
                    m.insert(email.into(), old.clone());
                    true
                })
                .and_then(|m| {
                    let m = m.expect("changed");
                    vault_save(&m).map(|_| m)
                }) {
                    Ok(m) => *cache = Some(m),
                    Err(e) => tracing::warn!("couldn't move a secret into the app's keychain item: {e:#}"),
                }
                Ok(old)
            }
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
            Secrets::Keyring(cache) => {
                let mut cache = cache.lock().unwrap();
                match vault_updated(cache.as_ref(), vault_read(), |m| m.remove(email).is_some()) {
                    Ok(Some(map)) => match vault_save(&map) {
                        Ok(()) => {
                            tracing::info!(name = %secret_name(email), entries = map.len(), "keychain: secret removed");
                            *cache = Some(map);
                        }
                        Err(e) => tracing::warn!(name = %secret_name(email), "keychain: couldn't remove a secret: {e:#}"),
                    },
                    Ok(None) => {}
                    Err(e) => tracing::warn!(name = %secret_name(email), "keychain: couldn't remove a secret: {e:#}"),
                }
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

/// A secret's name for the log: what it is, not whose (an account's address isn't written out).
fn secret_name(name: &str) -> String {
    if !name.contains('@') {
        return name.into();
    }
    // "someone@example.com#graph" and the like: what it is, and that it's an account's
    match name.split_once('#') {
        Some((_, kind)) => format!("<account>#{kind}"),
        None => "<account password>".into(),
    }
}

pub struct NewAccount {
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
}

/// The undo-send window: queued mail waits this long before it goes out.
pub const SEND_DELAY_MS: i64 = 10_000;

/// Keychain names: the user's Google OAuth client, and each account's Google sign-in.
const GOOGLE_CLIENT_KEY: &str = "#google:client";

/// A client kept with a sign-in, as it is now: the app's own is this build's (its secret may
/// have been replaced since), any other as kept.
fn current_google_client(kept: oauth::GoogleClient) -> oauth::GoogleClient {
    match oauth::built_in_google_client() {
        Some(b) if b.id == kept.id => b,
        _ => kept,
    }
}
fn google_key(account_id: i64) -> String {
    format!("#google-contacts:{account_id}")
}
/// The client an account's Google contacts were connected with (its refresh token is that
/// client's); missing for those connected before it was kept: Settings' client, then.
fn google_contacts_client_key(account_id: i64) -> String {
    format!("#google-contacts-client:{account_id}")
}

/// Google Contacts as Settings shows it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleStatus {
    /// The user's own OAuth client's ID (None: not set).
    pub client_id: Option<String>,
    /// Whether this build has the app's own client (`oauth::built_in_google_client`): Google
    /// sign-in works without one of the user's.
    pub built_in: bool,
    pub links: Vec<gcontacts::GoogleLink>,
}

/// A picture found is looked up again after this long; a domain without one, sooner.
const ICON_KEEP_SECS: i64 = 30 * 24 * 3600;
const ICON_MISS_SECS: i64 = 7 * 24 * 3600;

pub struct Core {
    /// One Google Contacts sync per account at a time.
    google_locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    /// Sender pictures being looked up (one lookup per domain), and at most a few at a time.
    icon_lookups: Mutex<HashMap<String, Arc<tokio::sync::OnceCell<Option<String>>>>>,
    icon_slots: tokio::sync::Semaphore,
    pub store: Arc<Store>,
    events: broadcast::Sender<CoreEvent>,
    secrets: Arc<Secrets>,
    strategy: Arc<dyn HeaderSync>,
    options: SyncOptions,
    running: Mutex<HashMap<i64, AccountSync>>,
    /// The export under way, if any (one at a time): its status and how to stop it.
    export: Mutex<Option<(ExportStatus, Arc<std::sync::atomic::AtomicBool>)>>,
    /// Accounts with no usable stored password (e.g. missing from the keychain).
    needs_password: Mutex<std::collections::HashSet<i64>>,
    /// Address suggestions (see `contacts`): built from the stored mail, kept current.
    contacts: Arc<Mutex<Contacts>>,
    /// Stops a browser sign-in that's waiting (`cancel_sign_in`).
    sign_in_cancel: tokio::sync::Notify,
}

/// A Microsoft account's refresh token lives in the keychain entry a password would use.
impl oauth::TokenStore for Secrets {
    fn refresh_token(&self, email: &str) -> Result<String> {
        self.get(email).map_err(|e| anyhow!(oauth::SignInNeeded(format!("no saved sign-in ({e:#})"))))
    }
    fn save_refresh_token(&self, email: &str, token: &str) -> Result<()> {
        self.set(email, token)
    }
}

#[derive(Default)]
struct Contacts {
    list: Option<Vec<contacts::Contact>>,
    built: Option<std::time::Instant>,
    building: bool,
}

/// Rebuilt this often (in the background) to take in new correspondents.
const CONTACTS_REFRESH: std::time::Duration = std::time::Duration::from_secs(30 * 60);

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
            google_locks: Mutex::new(HashMap::new()),
            icon_lookups: Mutex::new(HashMap::new()),
            icon_slots: tokio::sync::Semaphore::new(4),
            store: Arc::new(store),
            events,
            secrets: Arc::new(secrets),
            strategy,
            options,
            running: Mutex::new(HashMap::new()),
            export: Mutex::new(None),
            needs_password: Mutex::new(Default::default()),
            contacts: Default::default(),
            sign_in_cancel: tokio::sync::Notify::new(),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }

    /// Where and how an account signs in: its password, or (Microsoft, Google) a session that
    /// supplies current access tokens.
    /// With Graph allowed (`Account::graph`), also a Graph client for its tags and snooze.
    fn server_config(&self, account: &Account) -> Result<(ServerConfig, Option<Arc<oauth::Session>>, Option<Arc<graph::Graph>>)> {
        let (credentials, session, graph) = match account.auth {
            AuthKind::Password => (Credentials::Password(self.secrets.get(&account.email)?), None, None),
            AuthKind::Microsoft => {
                // The token itself must be there; it's read again at each renewal.
                self.secrets.get(&account.email)?;
                let store: Arc<dyn oauth::TokenStore> = self.secrets.clone();
                let graph = account
                    .graph
                    .then(|| Arc::new(graph::Graph::new(Arc::new(oauth::Session::graph(&account.email, store.clone())))));
                // Placeholder until the session supplies a token for each connection.
                (Credentials::OAuth2 { access_token: String::new() }, Some(Arc::new(oauth::Session::new(&account.email, store))), graph)
            }
            AuthKind::Google => {
                self.secrets.get(&account.email)?;
                let client = self.account_google_client(&account.email)?;
                let store: Arc<dyn oauth::TokenStore> = self.secrets.clone();
                (Credentials::OAuth2 { access_token: String::new() }, Some(Arc::new(oauth::Session::google(&account.email, client, store))), None)
            }
        };
        Ok((ServerConfig { host: account.imap_host.clone(), port: account.imap_port, user: account.email.clone(), credentials }, session, graph))
    }

    /// Start syncing every stored account. Accounts whose secret is missing are skipped.
    pub fn start(&self) -> Result<()> {
        imap::spawn_wake_detector();
        self.refresh_contacts();
        self.spawn_body_indexer();
        for account in self.store.accounts()? {
            if let Err(e) = self.start_account(account.clone()) {
                tracing::warn!(account = account.email, "not starting sync: {e:#}");
                self.needs_password.lock().unwrap().insert(account.id);
            }
        }
        Ok(())
    }

    /// Makes the text of downloaded messages searchable, in the background, for the app's life:
    /// those already here (after the upgrade that added it, all of them, newest first), then
    /// each body as it arrives, within seconds.
    fn spawn_body_indexer(&self) {
        let store = self.store.clone();
        let spawned = std::thread::Builder::new().name("body-index".into()).spawn(move || {
            match store.index_attachment_text_once() {
                Ok(0) => {}
                Ok(n) => tracing::info!(messages = n, "making the text in attachments searchable"),
                Err(e) => tracing::warn!("making the text in attachments searchable: {e:#}"),
            }
            let mut started: Option<(std::time::Instant, usize)> = None;
            loop {
                match store.index_bodies(100) {
                    Ok(0) => {
                        if let Some((t, n)) = started.take()
                            && n > 1000
                        {
                            tracing::info!(messages = n, secs = t.elapsed().as_secs(), "message text made searchable");
                        }
                        std::thread::sleep(std::time::Duration::from_secs(3));
                    }
                    Ok(n) => {
                        let s = started.get_or_insert((std::time::Instant::now(), 0));
                        s.1 += n;
                        // Gentle: the window's queries and the sync share the database.
                        std::thread::sleep(std::time::Duration::from_millis(30));
                    }
                    Err(e) => {
                        tracing::warn!("making message text searchable: {e:#}");
                        std::thread::sleep(std::time::Duration::from_secs(60));
                    }
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("no search in message text: {e}");
        }
    }

    fn start_account(&self, account: Account) -> Result<()> {
        let (server, microsoft, graph) = self.server_config(&account)?;
        let sync = AccountSync::start(
            self.store.clone(),
            account.clone(),
            server,
            microsoft,
            graph,
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

    /// Add a Microsoft account (outlook.com or Microsoft 365) by signing in in the browser:
    /// `open` shows Microsoft's sign-in page (`login_hint`: the address typed, filled in there).
    /// Waits for the browser (up to 5 minutes, or `cancel_sign_in`), checks the account against
    /// Microsoft's IMAP server, then saves it and starts syncing.
    pub async fn add_microsoft_account(&self, login_hint: Option<String>, display_name: Option<String>, open: impl FnOnce(String)) -> Result<Account> {
        let (email, refresh) = self.microsoft_sign_in(login_hint.as_deref(), open, false).await?;
        let host = "outlook.office365.com";
        self.secrets.set(&email, &refresh)?;
        let display_name = display_name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| email.clone());
        let account = self.store.insert_account(&email, &display_name, Provider::Outlook, host, 993)?;
        self.store.set_account_auth(account.id, AuthKind::Microsoft)?;
        self.store.set_smtp_server(account.id, Some(oauth::microsoft_smtp(&email)))?;
        let account = self.store.accounts()?.into_iter().find(|a| a.id == account.id).ok_or_else(|| anyhow!("account vanished"))?;
        if let Some(old) = self.running.lock().unwrap().remove(&account.id) {
            old.stop();
        }
        self.needs_password.lock().unwrap().remove(&account.id);
        self.start_account(account.clone())?;
        let _ = self.events.send(CoreEvent::FoldersChanged { account_id: account.id });
        Ok(account)
    }

    /// A Microsoft account's sign-in expired or was revoked: sign in again (as the same address)
    /// and restart its sync.
    pub async fn microsoft_sign_in_again(&self, account_id: i64, open: impl FnOnce(String)) -> Result<()> {
        let account = self.store.accounts()?.into_iter().find(|a| a.id == account_id).ok_or_else(|| anyhow!("no such account"))?;
        let (email, refresh) = self.microsoft_sign_in(Some(&account.email), open, false).await?;
        if !email.eq_ignore_ascii_case(&account.email) {
            bail!("You signed in as {email}, not {}. Sign in with that account instead.", account.email);
        }
        self.secrets.set(&account.email, &refresh)?;
        self.restart_account(account_id)
    }

    fn restart_account(&self, account_id: i64) -> Result<()> {
        let account = self.account(account_id)?;
        if let Some(old) = self.running.lock().unwrap().remove(&account_id) {
            old.stop();
        }
        self.needs_password.lock().unwrap().remove(&account_id);
        self.start_account(account)?;
        Ok(())
    }

    /// Let a Microsoft account keep tags and snooze through Microsoft Graph (`graph`): a sign-in
    /// in the browser that also asks for Graph, checked against Graph, then the account starts
    /// over with it. A work account whose organisation needs an administrator's consent fails
    /// at Microsoft's page, and the account stays as it was.
    pub async fn enable_graph(&self, account_id: i64, open: impl FnOnce(String)) -> Result<()> {
        let account = self.account(account_id)?;
        if account.auth != AuthKind::Microsoft {
            bail!("Only Microsoft accounts need this.");
        }
        let (email, refresh) = self.microsoft_sign_in(Some(&account.email), open, true).await?;
        if !email.eq_ignore_ascii_case(&account.email) {
            bail!("You signed in as {email}, not {}. Sign in with that account instead.", account.email);
        }
        self.secrets.set(&account.email, &refresh)?;
        // Graph's own refresh token, kept apart from the mail one (see `oauth::graph_key`).
        let graph_token = oauth::microsoft_refresh_for(&refresh, oauth::GRAPH_SCOPES)
            .await
            .map_err(|e| anyhow!("Signed in, but Microsoft Graph isn't available for this account: {e:#}"))?;
        self.secrets.set(&oauth::graph_key(&account.email), graph_token.refresh_token.as_deref().unwrap_or(&refresh))?;
        let store: Arc<dyn oauth::TokenStore> = self.secrets.clone();
        let check = graph::Graph::new(Arc::new(oauth::Session::graph(&account.email, store)));
        if let Err(e) = check.check().await {
            bail!("Signed in, but Microsoft Graph isn't available for this account: {e:#}");
        }
        self.store.set_account_graph(account_id, true)?;
        tracing::info!(account = account.email, "tags and snooze through Microsoft Graph");
        self.restart_account(account_id)?;
        let _ = self.events.send(CoreEvent::FoldersChanged { account_id });
        Ok(())
    }

    /// Stop keeping tags and snooze through Graph (they stay on the messages in Outlook).
    pub fn disable_graph(&self, account_id: i64) -> Result<()> {
        self.store.set_account_graph(account_id, false)?;
        let account = self.account(account_id)?;
        self.secrets.delete(&oauth::graph_key(&account.email));
        self.restart_account(account_id)?;
        let _ = self.events.send(CoreEvent::FoldersChanged { account_id });
        Ok(())
    }

    /// Whose mail service an address is on (known domains, else its MX records), for the
    /// add-account form: Microsoft accounts sign in in the browser instead of with a password.
    pub async fn detect_provider(&self, email: &str) -> Option<Provider> {
        imap::discover_server(email.trim()).await.map(|k| k.2)
    }

    /// Stop waiting for a browser sign-in.
    pub fn cancel_sign_in(&self) {
        self.sign_in_cancel.notify_waiters();
    }

    /// Add a Gmail or Google Workspace account by signing in in the browser, with the Google
    /// client in Settings (the user's own), else the app's. Like `add_microsoft_account`. An account
    /// already here with an app password switches to the sign-in.
    pub async fn add_google_account(&self, login_hint: Option<String>, display_name: Option<String>, open: impl FnOnce(String)) -> Result<Account> {
        let client = self.google_client().ok_or_else(|| anyhow!("Add your Google client ID and secret first."))?;
        let (email, refresh) = self.google_mail_sign_in(&client, login_hint.as_deref(), open).await?;
        let display_name = display_name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| email.clone());
        let account = self.store.insert_account(&email, &display_name, Provider::Gmail, "imap.gmail.com", 993)?;
        self.keep_google_sign_in(account.id, &email, &client, &refresh)?;
        let account = self.account(account.id)?;
        let _ = self.events.send(CoreEvent::FoldersChanged { account_id: account.id });
        Ok(account)
    }

    /// Sign a Gmail account in with Google (as the same address), and restart its sync: one
    /// signed in with Google whose sign-in expired or was revoked, or one on an app password
    /// switching to the sign-in (the sign-in takes the password's place in the keychain).
    pub async fn google_sign_in(&self, account_id: i64, open: impl FnOnce(String)) -> Result<()> {
        let account = self.account(account_id)?;
        if account.provider != Provider::Gmail {
            bail!("Only Gmail and Google Workspace accounts sign in with Google.");
        }
        // An account signed in already keeps its own client; else (or if that's gone) Settings',
        // or the app's.
        let client = match account.auth {
            AuthKind::Google => self.account_google_client(&account.email).ok().or_else(|| self.google_client()),
            _ => self.google_client(),
        }
        .ok_or_else(|| anyhow!("Add your Google client ID and secret first."))?;
        let (email, refresh) = self.google_mail_sign_in(&client, Some(&account.email), open).await?;
        if !email.eq_ignore_ascii_case(&account.email) {
            bail!("You signed in as {email}, not {}. Sign in with that account instead.", account.email);
        }
        self.keep_google_sign_in(account_id, &account.email, &client, &refresh)?;
        tracing::info!(account = account.email, "signed in with Google");
        Ok(())
    }

    /// Save a Google sign-in for an account (its refresh token where a password would be, and
    /// the client it belongs to), make it the account's way in, and start its sync over.
    fn keep_google_sign_in(&self, account_id: i64, email: &str, client: &oauth::GoogleClient, refresh: &str) -> Result<()> {
        self.secrets.set(&oauth::google_client_key(email), &serde_json::to_string(client)?)?;
        self.secrets.set(email, refresh)?;
        self.store.set_account_auth(account_id, AuthKind::Google)?;
        self.restart_account(account_id)
    }

    /// The client an account signed in to Google with (`oauth::google_client_key`).
    fn account_google_client(&self, email: &str) -> Result<oauth::GoogleClient> {
        let json = self.secrets.get(&oauth::google_client_key(email))?;
        Ok(current_google_client(serde_json::from_str(&json)?))
    }

    /// The browser sign-in for Google mail: (address, refresh token), checked by signing in to
    /// Gmail's IMAP server with it.
    async fn google_mail_sign_in(&self, client: &oauth::GoogleClient, login_hint: Option<&str>, open: impl FnOnce(String)) -> Result<(String, String)> {
        let redirect = oauth::Redirect::listen().await?;
        let uri = oauth::google_redirect(&redirect.uri);
        let pkce = oauth::Pkce::new();
        let hint = login_hint.map(str::trim).filter(|h| !h.is_empty());
        open(oauth::google_authorize_url(client, oauth::GOOGLE_MAIL_SCOPES, &uri, redirect.state(), &pkce.challenge, hint));
        let code = tokio::select! {
            code = redirect.code() => code?,
            _ = self.sign_in_cancel.notified() => bail!("Sign-in cancelled."),
        };
        let tokens = oauth::google_exchange(client, &code, &pkce.verifier, &uri).await?;
        if !oauth::grants_google_mail(&tokens) {
            bail!(
                "Google didn't give access to your mail: on Google's page, tick \"Read, compose, send, and \
                 permanently delete all your email from Gmail\", and try again."
            );
        }
        let email = tokens
            .id_token
            .as_deref()
            .and_then(oauth::email_from_id_token)
            .ok_or_else(|| anyhow!("Google didn't say which address you signed in with."))?;
        let refresh = tokens.refresh_token.clone().ok_or_else(|| anyhow!("Google didn't allow staying signed in (no refresh token)."))?;
        let cfg = ServerConfig {
            host: "imap.gmail.com".into(),
            port: 993,
            user: email.clone(),
            credentials: Credentials::OAuth2 { access_token: tokens.access_token },
        };
        match imap::connect(&cfg).await {
            Ok(conn) => conn.logout().await,
            Err(e) if imap::is_auth_error(&e) => bail!(
                "You're signed in, but Gmail's mail server refused this account ({e:#}). On Google's \
                 page, allow access to your mail; for a Workspace account, your administrator may \
                 need to allow IMAP or this app."
            ),
            Err(e) => return Err(e),
        }
        Ok((email, refresh))
    }

    /// The browser sign-in: (address, refresh token), checked by signing in to IMAP with it.
    async fn microsoft_sign_in(&self, login_hint: Option<&str>, open: impl FnOnce(String), graph: bool) -> Result<(String, String)> {
        let redirect = oauth::Redirect::listen().await?;
        let pkce = oauth::Pkce::new();
        let uri = redirect.uri.clone();
        open(oauth::microsoft_authorize_url(&uri, redirect.state(), &pkce.challenge, login_hint.map(str::trim).filter(|h| !h.is_empty()), graph));
        let code = tokio::select! {
            code = redirect.code() => code?,
            _ = self.sign_in_cancel.notified() => bail!("Sign-in cancelled."),
        };
        let tokens = oauth::microsoft_exchange(&code, &pkce.verifier, &uri).await?;
        let email = tokens
            .id_token
            .as_deref()
            .and_then(oauth::email_from_id_token)
            .ok_or_else(|| anyhow!("Microsoft didn't say which address you signed in with."))?;
        let refresh = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow!("Microsoft didn't allow staying signed in (no refresh token)."))?;
        let cfg = ServerConfig {
            host: "outlook.office365.com".into(),
            port: 993,
            user: email.clone(),
            credentials: Credentials::OAuth2 { access_token: tokens.access_token },
        };
        match imap::connect(&cfg).await {
            Ok(conn) => conn.logout().await,
            Err(e) if imap::is_auth_error(&e) => bail!(
                "You're signed in, but Microsoft's mail server refused this account ({e:#}). For a work or \
                 school account, IMAP may be turned off for your mailbox; your administrator can turn it on."
            ),
            Err(e) => return Err(e),
        }
        Ok((email, refresh))
    }

    /// What exporting an account involves: its messages, and how many must be downloaded.
    pub fn export_plan(&self, account_id: i64) -> Result<ExportPlan> {
        let (messages, to_download, download_bytes) = self.store.export_plan(account_id)?;
        Ok(ExportPlan { messages, to_download, download_bytes })
    }

    /// The export under way, if any.
    pub fn export_status(&self) -> Option<ExportStatus> {
        self.export.lock().unwrap().as_ref().map(|e| e.0.clone())
    }

    /// Stop the export under way (between messages; what's written stays, marked incomplete).
    pub fn cancel_export(&self) {
        if let Some((_, cancel)) = self.export.lock().unwrap().as_ref() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Export an account's mail into a new folder in `dir` (see `sync::export`): progress and
    /// the end as events (`ExportProgress`, `ExportFinished`). Runs until done; one at a time.
    pub async fn export_account(&self, account_id: i64, dir: &Path) -> Result<()> {
        let exporter = self
            .running
            .lock()
            .unwrap()
            .get(&account_id)
            .map(|s| s.exporter())
            .ok_or_else(|| anyhow!("This account isn't connected; check its sign-in in Settings › Accounts."))?;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut export = self.export.lock().unwrap();
            if export.is_some() {
                bail!("An export is already under way.");
            }
            *export = Some((ExportStatus { account_id, done: 0, total: 0 }, cancel.clone()));
        }
        let events = self.events.clone();
        let last = Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(1));
        let progress = |done: u32, total: u32| {
            if let Some(e) = self.export.lock().unwrap().as_mut() {
                e.0.done = done;
                e.0.total = total;
            }
            // At most five a second, and the last one.
            let mut last = last.lock().unwrap();
            if done == total || last.elapsed() >= std::time::Duration::from_millis(200) {
                *last = std::time::Instant::now();
                let _ = events.send(CoreEvent::ExportProgress { account_id, done, total });
            }
        };
        let result = exporter.run(dir, &cancel, progress).await;
        *self.export.lock().unwrap() = None;
        let finished = match &result {
            Ok(s) => ExportResult {
                account_id,
                path: Some(s.path.display().to_string()),
                written: s.written,
                partial: s.partial,
                failed: s.failed,
                cancelled: s.cancelled,
                error: None,
            },
            Err(e) => ExportResult { account_id, path: None, written: 0, partial: 0, failed: 0, cancelled: false, error: Some(format!("{e:#}")) },
        };
        tracing::info!(account_id, ?finished, "export finished");
        let _ = self.events.send(CoreEvent::ExportFinished(finished));
        result.map(|_| ())
    }

    /// Remove an account from this computer: its sync stops, its sign-in leaves the keychain,
    /// and everything stored for it goes (the space given back in the background). Nothing
    /// changes on the server.
    pub fn remove_account(&self, account_id: i64) -> Result<()> {
        if self.export_status().is_some_and(|e| e.account_id == account_id) {
            self.cancel_export();
        }
        if let Some(s) = self.running.lock().unwrap().remove(&account_id) {
            s.stop();
        }
        if let Some(a) = self.store.accounts()?.into_iter().find(|a| a.id == account_id) {
            self.secrets.delete(&a.email);
            self.secrets.delete(&oauth::graph_key(&a.email));
            self.secrets.delete(&oauth::google_client_key(&a.email));
        }
        self.needs_password.lock().unwrap().remove(&account_id);
        // Its Google contacts here go with it (nothing changes in Google).
        self.store.remove_google_link(account_id)?;
        self.secrets.delete(&google_key(account_id));
        self.secrets.delete(&google_contacts_client_key(account_id));
        self.store.delete_account(account_id)?;
        tracing::info!(account_id, "account removed");
        let store = self.store.clone();
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            match store.compact() {
                Ok(()) => tracing::info!(ms = t.elapsed().as_millis() as u64, "space of the removed account given back"),
                Err(e) => tracing::warn!("compacting after removing an account: {e:#}"),
            }
        });
        Ok(())
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

    /// Check every account for new mail now (the user asked: a refresh button, F5). Returns
    /// once every account has (or after a while: see `sync::CHECK_WAIT`).
    pub async fn check_now(&self) {
        let checks: Vec<_> = self.running.lock().unwrap().values().map(|s| s.check_now()).collect();
        futures::future::join_all(checks).await;
    }

    pub fn list_messages(&self, folder_id: i64, offset: u32, limit: u32) -> Result<MessagePage> {
        self.store.list_messages(folder_id, offset, limit)
    }

    /// A page of the folder's messages in `sort`'s order.
    pub fn list_messages_by(&self, folder_id: i64, sort: sort::ListSort, filter: &sort::ListFilter, offset: u32, limit: u32) -> Result<MessagePage> {
        self.store.list_messages_by(folder_id, sort, filter, offset, limit)
    }

    /// The groups of a folder's list (see `Store::list_groups`).
    pub fn list_groups(&self, folder_id: i64, threaded: bool, sort: sort::ListSort, filter: &sort::ListFilter, bounds: &[i64]) -> Result<Vec<sort::ListGroup>> {
        self.store.list_groups(folder_id, threaded, sort, filter, bounds)
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
            whole: false,
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

    /// Which of a message's attachments (their `index`) have one of `words` (lowercase, as
    /// typed in search) in their name or, with `attachment_search` on, their text: those stored
    /// with the body and those downloaded since.
    /// Read now, not from the index (which only knows the message): a large PDF can take a few
    /// seconds.
    pub fn attachment_matches(&self, message_id: i64, words: &[String]) -> Result<Vec<usize>> {
        let Some(raw) = self.store.body(message_id)? else { return Ok(Vec::new()) };
        // Their text only if the user turned that on (PDFs through the helper, like search).
        let read = self.store.attachment_search()?;
        let stored = mime::stored_attachment_parts(&raw);
        let omitted: Vec<partial::OmittedPart> = self.store.omitted(message_id)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let mut files: Vec<(usize, String, String, Vec<u8>)> = Vec::new();
        let n = stored.len();
        for (i, (name, mime, data)) in stored.into_iter().enumerate() {
            files.push((i, name, mime, data));
        }
        for idx in self.store.cached_attachment_indices(message_id)? {
            if let (Some(part), Some(data)) = (idx.checked_sub(n).and_then(|o| omitted.get(o)), self.store.cached_attachment(message_id, idx)?) {
                files.push((idx, part.name.clone(), part.mime.clone(), data));
            }
        }
        let mut out: Vec<usize> = files
            .into_iter()
            .filter(|(_, name, mime, data)| {
                mime::has_word_prefix(name, words) || read && mime::attachment_text(name, mime, data).is_some_and(|t| mime::has_word_prefix(&t, words))
            })
            .map(|(i, ..)| i)
            .collect();
        out.sort_unstable();
        Ok(out)
    }

    /// The whole message as sent (RFC 822 bytes) and its subject: the stored copy when it's
    /// complete, else downloaded (bodies kept without their attachments). For forwarding as an
    /// attachment.
    pub async fn whole_message(&self, message_id: i64) -> Result<(Vec<u8>, String)> {
        let subject = self.store.message_subject(message_id)?.unwrap_or_default();
        let complete = self.store.omitted(message_id)?.is_none_or(|j| j.trim().is_empty() || j.trim() == "[]");
        if complete && let Some(raw) = self.store.body(message_id)? {
            return Ok((raw, subject));
        }
        let account_id = self.store.message_account(message_id)?.ok_or_else(|| anyhow!("no such message"))?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = sync::PartRequest { message_id, whole: true, ordinal: 0, section: None, encoding: None, reply: tx };
        match self.running.lock().unwrap().get(&account_id) {
            Some(s) => s.fetch_part(req),
            None => return Err(anyhow!("This account isn't connected.")),
        }
        let raw = rx.await.map_err(|_| anyhow!("download was cancelled"))??;
        Ok((raw, subject))
    }

    /// A message's source as text (the whole message as sent; downloaded if it's kept here
    /// without its attachments).
    pub async fn message_source(&self, message_id: i64) -> Result<String> {
        let (raw, _) = self.whole_message(message_id).await?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
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
            .and_then(|m| m.subject().map(crate::mime::clean_subject))
            .unwrap_or_default();
        Ok((stored + omitted.len(), subject))
    }

    /// Apply a user action to messages viewed in `folder_id`: locally at once, on the server via
    /// the outbox (moves after the undo window). Returns the op id to pass to `undo_action`.
    pub fn apply_action(&self, action: actions::Action, message_ids: &[i64], folder_id: i64) -> Result<ActionResult> {
        let (account_id, _) = self.store.folder(folder_id)?.ok_or_else(|| anyhow!("unknown folder"))?;
        // A draft written here (in the composer or by an AI app) is only here, never on the
        // server: deleting its copy in Drafts deletes the draft. Not `discard_draft`: that's
        // the composer's "never mind", which puts back a server draft this one replaced; one
        // deleted on purpose leaves that in Trash, or it would come back to Drafts.
        let mut kept = Vec::with_capacity(message_ids.len());
        if matches!(action, actions::Action::Trash | actions::Action::DeleteForever) {
            for &id in message_ids {
                match self.store.message_draft(id)? {
                    Some(draft) => {
                        self.delete_draft(draft)?;
                    }
                    None => kept.push(id),
                }
            }
            if kept.is_empty() {
                return Ok(ActionResult { op_id: None, undoable: false });
            }
        }
        let message_ids = if kept.is_empty() { message_ids } else { &kept };
        let result = queue_action(&self.store, &self.events, action, message_ids, folder_id)?;
        if result.op_id.is_some() {
            if let Some(s) = self.running.lock().unwrap().get(&account_id) {
                s.wake_outbox();
            }
        }
        Ok(result)
    }

    /// The picture for a sender's address (a data: URL): their own when they're a Google
    /// contact with a photo, else their domain's, from what's kept or looked up now (see
    /// `icons`). None: no picture (a shared mail service, or the domain has none).
    pub async fn sender_icon(&self, addr: &str, scope: icons::PictureScope) -> Result<Option<String>> {
        match self.contact_photo(addr).await {
            Ok(Some(photo)) => return Ok(Some(photo)),
            Ok(None) => {}
            Err(e) => tracing::warn!("contact photo for a sender: {e:#}"),
        }
        let Some(domain) = icons::icon_domain(addr) else { return Ok(None) };
        match scope {
            icons::PictureScope::Contacts => return Ok(None),
            icons::PictureScope::Known if !self.known_domain(&domain)? => return Ok(None),
            _ => {}
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
        if let Some((icon, fetched)) = self.store.sender_icon(&domain)? {
            let fresh = now - fetched < if icon.is_some() { ICON_KEEP_SECS } else { ICON_MISS_SECS };
            if fresh {
                return Ok(icon.map(|i| i.data_url()));
            }
        }
        let cell = self.icon_lookups.lock().unwrap().entry(domain.clone()).or_default().clone();
        let url = cell
            .get_or_init(|| async {
                let _slot = self.icon_slots.acquire().await.ok();
                let found = match tokio::time::timeout(std::time::Duration::from_secs(25), icons::fetch(&domain)).await {
                    Ok(Ok(found)) => found,
                    Ok(Err(e)) => {
                        tracing::debug!(domain, "sender picture: {e:#}");
                        None
                    }
                    Err(_) => None,
                };
                if let Err(e) = self.store.save_sender_icon(&domain, found.as_ref(), now) {
                    tracing::warn!("keeping a sender picture: {e:#}");
                }
                found.map(|i| i.data_url())
            })
            .await
            .clone();
        self.icon_lookups.lock().unwrap().remove(&domain);
        Ok(url)
    }

    /// Where these messages' senders' mail is kept: (folder, count), most first.
    pub fn sender_folders(&self, message_ids: &[i64]) -> Result<Vec<(i64, u32)>> {
        self.store.sender_folder_counts(message_ids)
    }

    /// The list's row ids from `offset` (see `Store::list_item_ids`).
    pub fn list_item_ids(
        &self,
        folder_id: i64,
        threaded: bool,
        sort: sort::ListSort,
        filter: &sort::ListFilter,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<i64>> {
        self.store.list_item_ids(folder_id, threaded, sort, filter, offset, limit)
    }

    /// The messages behind several picked rows, and their state for the action bar.
    pub fn selection_info(&self, folder_id: i64, items: &[i64], threaded: bool) -> Result<SelectionInfo> {
        let rows = self.store.selection_messages(folder_id, items, threaded)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
        Ok(SelectionInfo {
            any_unread: rows.iter().any(|r| r.1 & flags::SEEN == 0),
            all_flagged: !rows.is_empty() && rows.iter().all(|r| r.1 & flags::FLAGGED != 0),
            tag_sets: rows.iter().map(|r| tags::visible(&r.2)).collect(),
            snoozed_until: rows.iter().filter_map(|r| tags::snoozed_until(&r.2)).filter(|&t| t > now).max(),
            size: rows.iter().map(|r| r.3).sum(),
            sizes: rows.iter().map(|r| r.3).collect(),
            subjects: rows.iter().map(|r| r.4.clone()).collect(),
            ids: rows.into_iter().map(|r| r.0).collect(),
        })
    }

    /// Mark every message in a folder read (on the server too, in one command per folder).
    /// Returns how many were unread.
    pub fn mark_folder_read(&self, folder_id: i64) -> Result<usize> {
        let ids = self.store.unread_in_folder(folder_id)?;
        if !ids.is_empty() {
            self.apply_action(actions::Action::Read, &ids, folder_id)?;
        }
        Ok(ids.len())
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

    pub fn index_of(&self, folder_id: i64, id: i64, threaded: bool, sort: sort::ListSort, filter: &sort::ListFilter) -> Result<Option<u32>> {
        self.store.index_of_by(folder_id, id, threaded, sort, filter)
    }

    /// Where a message shows in the folder's list: its row (the message, or with `threaded` its
    /// conversation) and that row's position; None if it isn't in the folder.
    pub fn locate_message(
        &self,
        folder_id: i64,
        message_id: i64,
        threaded: bool,
        sort: sort::ListSort,
        filter: &sort::ListFilter,
    ) -> Result<Option<(i64, u32)>> {
        let item = if threaded { self.store.message_thread_id(message_id)? } else { Some(message_id) };
        let Some(item) = item else { return Ok(None) };
        Ok(self.store.index_of_by(folder_id, item, threaded, sort, filter)?.map(|i| (item, i)))
    }

    /// A message's detail, downloading its body first if it isn't local yet (waits up to 30 s).
    pub async fn detail_now(&self, id: i64) -> Result<MessageDetail> {
        let mut rx = self.events.subscribe();
        if let Some(d) = self.get_message(id)? {
            return Ok(d);
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => bail!("Couldn't download the original message."),
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => bail!("shutting down"),
                Ok(_) => {}
            }
            if let Some(d) = self.get_message(id)? {
                return Ok(d);
            }
        }
    }

    /// Prefill a reply, reply-all or forward of message `id`: recipients (Reply-To honoured; our
    /// own address left out), subject, and the quoted original in Gmail's markup.
    pub async fn reply_context(&self, id: i64, mode: ReplyMode) -> Result<ReplyContext> {
        let account_id = self.store.message_account(id)?.ok_or_else(|| anyhow!("no such message"))?;
        let account = self.store.accounts()?.into_iter().find(|a| a.id == account_id).ok_or_else(|| anyhow!("no such account"))?;
        let d = self.detail_now(id).await?;
        let raw = self.store.body(id)?.unwrap_or_default();
        // Answered from the address it came to (an alias, a catch-all's), else the default.
        let ids: Vec<identities::Identity> = self.store.identities()?.into_iter().filter(|i| i.account_id == account_id).collect();
        let came_to: Vec<String> = d.to.iter().chain(&d.cc).map(|a| a.addr.clone()).chain(mime::delivered_to(&raw)).collect();
        let from_identity = identities::for_reply(&account.email, &ids, &came_to);
        let original = d.html.clone().unwrap_or_else(|| compose::text_to_html(d.text.as_deref().unwrap_or("")));
        let when = chrono::DateTime::from_timestamp(d.date, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%a, %b %-d, %Y at %-I:%M %p").to_string())
            .unwrap_or_default();
        if mode == ReplyMode::Forward {
            let list = |l: &[Addr]| l.iter().map(compose::display_addr).collect::<Vec<_>>().join(", ");
            let headers = [
                ("From", compose::display_addr(&d.from)),
                ("Date", compose::escape_html(&when)),
                ("Subject", compose::escape_html(&d.subject)),
                ("To", list(&d.to)),
                ("Cc", list(&d.cc)),
            ];
            return Ok(ReplyContext {
                account_id,
                to: vec![],
                cc: vec![],
                subject: compose::forward_subject(&d.subject),
                quote_html: compose::forward_quote(&headers, &original),
                replying_to: None,
                forwarding: Some(id),
                attachments: d.attachments,
                from_identity,
            });
        }
        let orig = compose::Original {
            from: vec![d.from.clone()],
            reply_to: mime::reply_to(&raw),
            to: d.to.clone(),
            cc: d.cc.clone(),
            ..Default::default()
        };
        let own: Vec<String> = std::iter::once(account.email.clone()).chain(ids.iter().map(|i| i.email.clone())).collect();
        let (to, cc) = compose::reply_recipients(&orig, &own, mode == ReplyMode::ReplyAll);
        let attribution = format!("On {}, {} wrote:", compose::escape_html(&when), compose::display_addr(&d.from));
        Ok(ReplyContext {
            account_id,
            to,
            cc,
            subject: compose::reply_subject(&d.subject),
            quote_html: compose::reply_quote(&attribution, &original),
            replying_to: Some(id),
            forwarding: None,
            attachments: vec![],
            from_identity,
        })
    }

    /// Every account's other addresses to send as.
    pub fn identities(&self) -> Result<Vec<identities::Identity>> {
        self.store.identities()
    }

    /// Add (`id` 0) or change an address to send as. Returns its id.
    pub fn save_identity(&self, mut i: identities::Identity) -> Result<i64> {
        let account = self.account(i.account_id)?;
        let others: Vec<identities::Identity> = self.store.identities()?.into_iter().filter(|o| o.account_id == i.account_id).collect();
        if i.id != 0 && !others.iter().any(|o| o.id == i.id) {
            bail!("That address is no longer there.");
        }
        (i.email, i.name) = identities::check(&i, &account.email, &others).map_err(|e| anyhow!(e))?;
        self.store.save_identity(&i)
    }

    pub fn delete_identity(&self, id: i64) -> Result<()> {
        self.store.delete_identity(id)
    }

    /// Queue a message: built now, sent after the undo window (`SEND_DELAY_MS`) unless taken
    /// back with `cancel_send`.
    pub async fn send_message(&self, req: SendRequest) -> Result<SendQueued> {
        let account = self.account(req.account_id)?;
        if account.smtp().is_none() {
            bail!("No outgoing (SMTP) server is set for {}.", account.email);
        }
        let draft_id = req.draft_id;
        let mut msg = self.outgoing_of(&account, &req)?;
        let recipients = msg.recipients();
        if recipients.is_empty() {
            bail!("Add at least one recipient.");
        }
        self.note_sent_to(&msg.to.iter().chain(&msg.cc).chain(&msg.bcc).cloned().collect::<Vec<_>>());
        if let Some(fwd) = req.forwarding {
            for &index in &req.forward_attachments {
                let (name, content_type, data) = self.attachment(fwd, index).await?;
                msg.attachments.push(compose::OutAttachment { name, content_type, data });
            }
        }
        for &id in &req.forward_messages {
            let (data, subject) = self.whole_message(id).await?;
            msg.attachments.push(compose::OutAttachment { name: eml_name(&subject), content_type: "message/rfc822".into(), data });
        }
        for path in &req.attachments {
            let p = std::path::Path::new(path);
            let data = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into());
            msg.attachments.push(compose::OutAttachment { content_type: compose::content_type_for(&name).into(), name, data });
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
        let raw = compose::build(&msg, &compose::new_message_id(&account.email), now.as_secs() as i64)?;
        let send_at = now.as_millis() as i64 + SEND_DELAY_MS;
        let id = self.store.enqueue_outgoing(&store::NewOutgoing {
            account_id: account.id,
            raw,
            envelope_from: account.email.clone(),
            recipients,
            not_before_ms: send_at,
            replied: req.replying_to,
            forwarded: req.forwarding,
            draft: req.draft.map(|d| d.to_string()),
        })?;
        if let Some(s) = self.running.lock().unwrap().get(&account.id) {
            s.wake_sender();
        }
        if let Some(d) = draft_id {
            self.delete_draft(d)?;
        }
        if let Some(old) = req.replaces {
            self.retire_replaced_draft(old);
        }
        // Shown in Sent (and its conversation) right away, marked as sending.
        let sent = self.store.folders(account.id)?.into_iter().find(|f| f.role == Some(FolderRole::Sent));
        if let Some(sent) = sent {
            let raw = self.store.outgoing_raw(id)?.unwrap_or_default();
            let copy = self.store.insert_local_copy(account.id, sent.id, &raw, id)?;
            let _ = self.events.send(CoreEvent::MessagesChanged { folder_ids: self.store.message_folder_ids(copy)? });
            let _ = self.events.send(CoreEvent::FoldersChanged { account_id: account.id });
        }
        Ok(SendQueued { id, send_at })
    }

    fn account(&self, id: i64) -> Result<Account> {
        self.store.accounts()?.into_iter().find(|a| a.id == id).ok_or_else(|| anyhow!("no such account"))
    }

    /// The message a composer request describes (without attachments): From, recipients,
    /// subject, body, and threading headers for a reply.
    fn outgoing_of(&self, account: &Account, req: &SendRequest) -> Result<compose::Outgoing> {
        let ids: Vec<identities::Identity> = self.store.identities()?.into_iter().filter(|i| i.account_id == account.id).collect();
        let from = identities::sender(&account.email, &account.display_name, &ids, req.from_identity)
            .ok_or_else(|| anyhow!("The address to send from is no longer one of {}'s.", account.email))?;
        let mut msg = compose::Outgoing {
            from: Some(from),
            to: req.to.clone(),
            cc: req.cc.clone(),
            bcc: req.bcc.clone(),
            subject: req.subject.clone(),
            html: req.html.clone(),
            ..Default::default()
        };
        if let Some(orig) = req.replying_to.map(|id| self.store.threading_of(id)).transpose()?.flatten() {
            (msg.in_reply_to, msg.references) = compose::reply_threading(&orig);
        }
        Ok(msg)
    }

    /// Save the composer's state as draft `id` (created when None): kept in the core, and
    /// shown in Drafts with "Edit draft". Returns the draft id.
    pub fn save_draft(&self, id: Option<i64>, req: SendRequest) -> Result<i64> {
        let account = self.account(req.account_id)?;
        let state = req.draft.as_ref().map(|d| d.to_string()).unwrap_or_else(|| "{}".into());
        let (id, message_id) = match id.map(|i| self.store.draft(i)).transpose()?.flatten() {
            Some((_, _, mid)) => {
                let id = id.unwrap();
                self.store.update_draft(id, account.id, &state)?;
                (id, mid)
            }
            None => {
                let mid = compose::new_message_id(&account.email);
                (self.store.create_draft(account.id, &state, &mid)?, mid)
            }
        };
        // A draft from an address since removed is kept, as from the account's own.
        let msg = match self.outgoing_of(&account, &req) {
            Err(_) if req.from_identity.is_some() => self.outgoing_of(&account, &SendRequest { from_identity: None, ..req.clone() })?,
            m => m?,
        };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
        let raw = compose::build(&msg, &message_id, now)?;
        // A just-opened, still empty composer isn't a draft worth listing.
        let empty = !req.listed
            || msg.recipients().is_empty()
            && msg.subject.trim().is_empty()
            && mail_parser::decoders::html::html_to_text(&req.html).trim().is_empty();
        if !empty {
            if let Some(old) = req.replaces {
                self.retire_replaced_draft(old);
            }
        }
        if let Some(drafts) = self.store.folders(account.id)?.into_iter().find(|f| f.role == Some(FolderRole::Drafts)) {
            let folders = if empty {
                self.store.delete_draft_copy(id)?
            } else {
                let (folders, rewritten) = self.store.upsert_draft_copy(id, account.id, drafts.id, &raw)?;
                // Its new text, for windows showing (or keeping) the one saved before.
                if let Some(copy) = rewritten {
                    let _ = self.events.send(CoreEvent::MessageReady { id: copy });
                }
                folders
            };
            let _ = self.events.send(CoreEvent::MessagesChanged { folder_ids: folders });
            let _ = self.events.send(CoreEvent::FoldersChanged { account_id: account.id });
        }
        Ok(id)
    }

    /// A server draft (written in another app) that a draft here was opened from: to Trash, so
    /// Drafts doesn't show both. Only while it's still in Drafts (nothing to do the second time);
    /// a failure is logged, not passed on: the draft here is saved either way.
    fn retire_replaced_draft(&self, message_id: i64) {
        let in_drafts = (|| -> Result<Option<i64>> {
            let folders = self.store.message_folder_ids(message_id)?;
            let Some(&first) = folders.first() else { return Ok(None) };
            let Some((account_id, _)) = self.store.folder(first)? else { return Ok(None) };
            let drafts = self.store.folders(account_id)?.into_iter().find(|f| f.role == Some(FolderRole::Drafts));
            Ok(drafts.map(|d| d.id).filter(|d| folders.contains(d)))
        })()
        .unwrap_or(None);
        if let Some(folder) = in_drafts {
            tracing::info!("a draft opened from a server draft was changed: the server's goes to Trash");
            if let Err(e) = self.apply_action(actions::Action::Trash, &[message_id], folder) {
                tracing::warn!("moving the replaced draft to Trash: {e:#}");
            }
        }
    }

    /// Throw a draft away (the composer's Discard, or closing it without saving). One opened
    /// from a server draft that has already gone to Trash (`retire_replaced_draft`) puts that
    /// back in Drafts, so what was written elsewhere isn't left only in Trash.
    pub fn discard_draft(&self, id: i64) -> Result<()> {
        if let Some((account_id, state, _)) = self.store.draft(id)? {
            let replaces = serde_json::from_str::<serde_json::Value>(&state).ok().and_then(|v| v.get("replaces")?.as_i64());
            if let Some(old) = replaces {
                if let Err(e) = self.restore_replaced_draft(account_id, old) {
                    tracing::warn!("putting the replaced draft back in Drafts: {e:#}");
                }
            }
        }
        self.delete_draft(id)
    }

    fn restore_replaced_draft(&self, account_id: i64, message_id: i64) -> Result<()> {
        let folders = self.store.folders(account_id)?;
        let role = |r: FolderRole| folders.iter().find(|f| f.role == Some(r)).map(|f| f.id);
        let (Some(trash), Some(drafts)) = (role(FolderRole::Trash), role(FolderRole::Drafts)) else { return Ok(()) };
        // The same message, or, once the move reached the server and came back, the copy in
        // Trash with its Message-ID.
        let in_trash = if self.store.message_folder_ids(message_id)?.contains(&trash) {
            Some(message_id)
        } else {
            match self.store.message_header_id(message_id)? {
                Some(h) => self.store.message_in_folder_by_header(trash, &h)?,
                None => None,
            }
        };
        if let Some(m) = in_trash {
            tracing::info!("discarded: the server draft it replaced goes back to Drafts");
            self.apply_action(actions::Action::Move { target_folder_id: drafts }, &[m], trash)?;
        }
        Ok(())
    }

    /// The files of drafts opened from server drafts (`draft-files/` in the data folder) that no
    /// draft or queued message uses any more: what to clean up.
    pub fn unused_draft_file_dirs(&self, root: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
        let Ok(entries) = std::fs::read_dir(root) else { return Ok(Vec::new()) };
        let states = self.store.draft_states()?;
        Ok(entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                !states.iter().any(|s| s.contains(&name))
            })
            .collect())
    }

    /// The composer state of a draft (to open it in a window).
    pub fn open_draft(&self, id: i64) -> Result<serde_json::Value> {
        let (_, state, _) = self.store.draft(id)?.ok_or_else(|| anyhow!("That draft no longer exists."))?;
        Ok(serde_json::from_str(&state)?)
    }

    /// Discard a draft (and its copy in Drafts).
    pub fn delete_draft(&self, id: i64) -> Result<()> {
        let folders = self.store.delete_draft(id)?;
        if !folders.is_empty() {
            let _ = self.events.send(CoreEvent::MessagesChanged { folder_ids: folders.clone() });
            for (account_id, _) in folders.iter().filter_map(|f| self.store.folder(*f).ok().flatten()).collect::<std::collections::HashSet<_>>() {
                let _ = self.events.send(CoreEvent::FoldersChanged { account_id });
            }
        }
        Ok(())
    }

    /// Take back a queued message before it's sent (undo send): its draft, to reopen.
    pub fn cancel_send(&self, id: i64) -> Result<Option<serde_json::Value>> {
        let draft = self.store.cancel_outgoing(id)?.ok_or_else(|| anyhow!("Too late to undo: the message was already sent."))?;
        for s in self.running.lock().unwrap().values() {
            s.wake_sender();
        }
        let folders = self.store.drop_local_copy(id)?;
        if !folders.is_empty() {
            let _ = self.events.send(CoreEvent::MessagesChanged { folder_ids: folders.clone() });
            for (account_id, _) in folders.iter().filter_map(|f| self.store.folder(*f).ok().flatten()).collect::<std::collections::HashSet<_>>() {
                let _ = self.events.send(CoreEvent::FoldersChanged { account_id });
            }
        }
        Ok(serde_json::from_str(&draft).ok())
    }

    /// Every account with its servers (Settings › Accounts).
    pub fn account_details(&self) -> Result<Vec<AccountDetails>> {
        self.store
            .accounts()?
            .into_iter()
            .map(|a| {
                let smtp = a.smtp();
                Ok(AccountDetails {
                    keep_attachments: self.store.keeps_attachments(a.id)?,
                    id: a.id,
                    smtp_default: a.smtp_host.as_deref().is_none_or(|h| h.trim().is_empty()),
                    smtp_host: smtp.as_ref().map(|s| s.0.clone()),
                    smtp_port: smtp.map(|s| s.1),
                    email: a.email,
                    display_name: a.display_name,
                    provider: a.provider,
                    imap_host: a.imap_host,
                    imap_port: a.imap_port,
                })
            })
            .collect()
    }

    /// Send `account_id`'s mail through `server` (host, port), or with `None` through the default
    /// for its IMAP server. Its sync restarts to pick it up.
    pub fn set_smtp_server(&self, account_id: i64, server: Option<(&str, u16)>) -> Result<()> {
        self.store.set_smtp_server(account_id, server)?;
        let account = self.store.accounts()?.into_iter().find(|a| a.id == account_id).ok_or_else(|| anyhow!("no such account"))?;
        if let Some(old) = self.running.lock().unwrap().remove(&account_id) {
            old.stop();
            self.start_account(account)?;
        }
        Ok(())
    }

    /// Remove the attachments stored for `account_id` (they're fetched again when opened) and
    /// give the space back: (messages changed, bytes freed). Takes a while (compacts the body
    /// database): call off the UI's path. Refused while the account keeps attachments.
    pub fn remove_attachments(&self, account_id: i64) -> Result<(u64, u64)> {
        if self.store.keeps_attachments(account_id)? {
            return Err(anyhow!("Turn off keeping attachments first."));
        }
        let done = self.store.remove_attachments(account_id)?;
        tracing::info!(account_id, messages = done.0, freed = done.1, "removed stored attachments");
        if let Some(folders) = self.store.folders(account_id).ok() {
            let _ = self.events.send(CoreEvent::MessagesChanged { folder_ids: folders.iter().map(|f| f.id).collect() });
        }
        Ok(done)
    }

    /// Per account, messages with attachments still to download and already stored.
    pub fn attachment_counts(&self) -> Result<Vec<AttachmentCounts>> {
        self.store.attachment_counts()
    }

    /// Messages and space used per account (slow on a cold disk: call off the UI's path).
    pub fn account_storage(&self) -> Result<Vec<AccountStorage>> {
        self.store.account_storage()
    }

    /// The name mail from `account_id` is sent under. Empty means none: the address alone (as
    /// when an account is added without a name).
    pub fn set_display_name(&self, account_id: i64, name: &str) -> Result<()> {
        let account = self.account(account_id)?;
        let name = name.trim();
        self.store.set_display_name(account_id, if name.is_empty() { &account.email } else { name })?;
        Ok(())
    }

    /// The order accounts are listed in: `ids` first to last (any others after them).
    pub fn set_account_order(&self, ids: &[i64]) -> Result<()> {
        self.store.set_account_order(ids)
    }

    /// The letter and colour of an account's marker; None (or empty) for automatic. The letter
    /// is one character, the colour a name from the app's palette (lowercase letters).
    pub fn set_account_mark(&self, account_id: i64, letter: Option<&str>, color: Option<&str>) -> Result<()> {
        self.account(account_id)?;
        let letter = letter.map(str::trim).filter(|l| !l.is_empty()).map(|l| l.to_uppercase());
        if letter.as_ref().is_some_and(|l| l.chars().count() > 1) {
            bail!("a marker is one letter");
        }
        let color = color.map(str::trim).filter(|c| !c.is_empty());
        if color.is_some_and(|c| c.len() > 16 || !c.chars().all(|ch| ch.is_ascii_lowercase())) {
            bail!("not a marker colour");
        }
        self.store.set_account_mark(account_id, letter.as_deref(), color)
    }

    /// Download whole messages for `account_id` (attachments for offline use), or stop doing so.
    /// Turning it on also fetches, in the background, the messages already stored without them.
    pub fn set_keep_attachments(&self, account_id: i64, keep: bool) -> Result<()> {
        self.store.set_keep_attachments(account_id, keep)?;
        if keep {
            if let Some(s) = self.running.lock().unwrap().get(&account_id) {
                s.wake_bodies();
            }
        }
        Ok(())
    }

    /// Addresses for the composer's To/Cc/Bcc matching what's typed, best first.
    pub fn suggest_addresses(&self, query: &str, limit: usize) -> Result<Vec<Addr>> {
        self.ensure_contacts()?;
        // The address book first (its names), then the people from the mail not in it.
        let mut out = addressbook::suggest(&self.store.cards()?, query, limit);
        let c = self.contacts.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        for a in contacts::suggest(c.list.as_deref().unwrap_or_default(), query, limit, now) {
            if out.len() >= limit {
                break;
            }
            if !out.iter().any(|x| x.addr.eq_ignore_ascii_case(&a.addr)) {
                out.push(a);
            }
        }
        Ok(out)
    }

    pub fn cards(&self) -> Result<Vec<addressbook::Card>> {
        self.store.cards()
    }

    /// Save a card the user changed or made. A Google contact is marked to send at the next
    /// sync; a new card with `google` set is made in that account's Google contacts.
    pub fn save_card(&self, card: addressbook::Card) -> Result<i64> {
        let new_in = (card.id == 0).then_some(card.google).flatten();
        if let Some(acct) = new_in
            && self.store.google_link(acct)?.is_none()
        {
            bail!("Google Contacts isn't connected for that account.");
        }
        let groups = card.groups.clone();
        let id = self.store.save_card(card)?;
        if let Some(account_id) = new_in {
            // In the group it was made in (the one being looked at), here and there.
            self.store.save_card_link(&gcontacts::new_link(id, account_id, &groups))?;
        } else {
            // One more edit to send (counted, so a sync running now doesn't take it as sent).
            self.store.mark_card_edited(id)?;
        }
        Ok(id)
    }

    /// Delete a card; a Google contact is deleted there too at the next sync.
    pub fn delete_card(&self, id: i64) -> Result<()> {
        if let Some(l) = self.store.card_link(id)?
            && let Some(res) = &l.resource
        {
            self.store.add_card_tombstone(l.account_id, res)?;
        }
        self.store.delete_card(id)
    }

    // ---- Google Contacts ---------------------------------------------------------------------

    /// The client a new Google sign-in uses: the user's own (Settings), else the app's.
    fn google_client(&self) -> Option<oauth::GoogleClient> {
        self.own_google_client().or_else(oauth::built_in_google_client)
    }

    fn own_google_client(&self) -> Option<oauth::GoogleClient> {
        self.secrets.get(GOOGLE_CLIENT_KEY).ok().and_then(|j| serde_json::from_str(&j).ok())
    }

    /// The OAuth client to sign in with (None: forget it).
    pub fn set_google_client(&self, client: Option<oauth::GoogleClient>) -> Result<()> {
        match client {
            Some(c) => {
                let c = oauth::GoogleClient { id: c.id.trim().to_string(), secret: c.secret.trim().to_string() };
                if !c.id.ends_with(".apps.googleusercontent.com") {
                    bail!("That isn't a Google client ID (it ends in .apps.googleusercontent.com).");
                }
                self.secrets.set(GOOGLE_CLIENT_KEY, &serde_json::to_string(&c)?)
            }
            None => {
                self.secrets.delete(GOOGLE_CLIENT_KEY);
                Ok(())
            }
        }
    }

    /// Whether the client is set, and the accounts linked to their Google contacts.
    pub fn google_status(&self) -> Result<GoogleStatus> {
        Ok(GoogleStatus {
            client_id: self.own_google_client().map(|c| c.id),
            built_in: oauth::built_in_google_client().is_some(),
            links: self.store.google_links()?,
        })
    }

    /// Connect an account to its Google contacts: a browser sign-in (`open` shows the page),
    /// then a first sync (reading only). Returns the Google address signed in with.
    pub async fn google_connect(&self, account_id: i64, open: impl FnOnce(String)) -> Result<String> {
        let client = self.google_client().ok_or_else(|| anyhow!("Add your Google client ID and secret first."))?;
        let account = self.account(account_id)?;
        let redirect = oauth::Redirect::listen().await?;
        let uri = oauth::google_redirect(&redirect.uri);
        let pkce = oauth::Pkce::new();
        open(oauth::google_authorize_url(&client, oauth::GOOGLE_CONTACTS_SCOPES, &uri, redirect.state(), &pkce.challenge, Some(&account.email)));
        let code = tokio::select! {
            code = redirect.code() => code?,
            _ = self.sign_in_cancel.notified() => bail!("Sign-in cancelled."),
        };
        let tokens = oauth::google_exchange(&client, &code, &pkce.verifier, &uri).await?;
        let refresh = tokens.refresh_token.clone().ok_or_else(|| anyhow!("Google didn't allow staying signed in (no refresh token)."))?;
        let email = tokens.id_token.as_deref().and_then(oauth::email_from_id_token).unwrap_or_else(|| account.email.clone());
        self.secrets.set(&google_key(account_id), &refresh)?;
        self.secrets.set(&google_contacts_client_key(account_id), &serde_json::to_string(&client)?)?;
        self.store.set_google_link(account_id, &email)?;
        Ok(email)
    }

    /// Stop syncing an account's Google contacts: its cards here go; Google is left as it is.
    pub fn google_disconnect(&self, account_id: i64) -> Result<()> {
        self.store.remove_google_link(account_id)?;
        self.secrets.delete(&google_key(account_id));
        self.secrets.delete(&google_contacts_client_key(account_id));
        Ok(())
    }

    /// What Google has for a contact beyond what the address book edits (None: not a Google
    /// contact, or not synced yet). Its picture is fetched the first time (and again when it
    /// changes there), then kept.
    pub async fn contact_details(&self, card_id: i64) -> Result<Option<gcontacts::Details>> {
        let Some(raw) = self.store.card_link(card_id)?.and_then(|l| l.raw) else {
            return Ok(None);
        };
        let person: serde_json::Value = serde_json::from_str(&raw)?;
        let mut d = gcontacts::details(&person);
        d.photo = self.card_photo(card_id, &person).await?.map(|i| i.data_url());
        Ok(Some(d))
    }

    /// A Google contact's own picture: kept once fetched (`card_photos`), fetched again when
    /// Google's address for it changes. None: no picture of their own, or it can't be had now.
    async fn card_photo(&self, card_id: i64, person: &serde_json::Value) -> Result<Option<icons::Icon>> {
        let Some(url) = gcontacts::photo_url(person) else { return Ok(None) };
        if let Some((u, mime, data)) = self.store.card_photo(card_id)?
            && u == url
        {
            return Ok(Some(icons::Icon { mime, data }));
        }
        let _slot = self.icon_slots.acquire().await.ok();
        match tokio::time::timeout(std::time::Duration::from_secs(25), icons::get_image(&url)).await {
            Ok(Ok(icon)) => {
                self.store.save_card_photo(card_id, &url, &icon.mime, &icon.data)?;
                Ok(Some(icon))
            }
            Ok(Err(e)) => {
                tracing::warn!(card_id, "contact photo: {e:#}");
                Ok(None)
            }
            Err(_) => {
                tracing::warn!(card_id, "contact photo: timed out");
                Ok(None)
            }
        }
    }

    /// The picture of the Google contact with this address, if they have one of their own.
    async fn contact_photo(&self, addr: &str) -> Result<Option<String>> {
        for card_id in self.store.google_cards_with_email(addr)? {
            let Some(raw) = self.store.card_link(card_id)?.and_then(|l| l.raw) else { continue };
            let person: serde_json::Value = serde_json::from_str(&raw)?;
            if let Some(icon) = self.card_photo(card_id, &person).await? {
                return Ok(Some(icon.data_url()));
            }
        }
        Ok(None)
    }

    /// The client an account's Google contacts were connected with. One connected before that
    /// was kept used Settings' client: kept now, so forgetting that client later doesn't stop it.
    fn google_contacts_client(&self, account_id: i64) -> Option<oauth::GoogleClient> {
        let key = google_contacts_client_key(account_id);
        if let Some(c) = self.secrets.get(&key).ok().and_then(|j| serde_json::from_str(&j).ok()) {
            return Some(current_google_client(c));
        }
        let c = self.google_client()?;
        if let Ok(j) = serde_json::to_string(&c) {
            let _ = self.secrets.set(&key, &j);
        }
        Some(c)
    }

    /// Sync one account's Google contacts (one sync per account at a time); what it did.
    pub async fn google_sync(&self, account_id: i64) -> Result<gcontacts::SyncReport> {
        let lock = self.google_locks.lock().unwrap().entry(account_id).or_default().clone();
        let _held = lock.lock().await;
        let result = async {
            let client = self.google_contacts_client(account_id).ok_or_else(|| anyhow!("Add your Google client ID and secret first."))?;
            let refresh = self.secrets.get(&google_key(account_id)).map_err(|_| anyhow!(gcontacts::PeopleError::SignIn("no saved sign-in".into())))?;
            let tokens = oauth::google_refresh(&client, &refresh).await?;
            let api = gcontacts::GoogleApi { access_token: tokens.access_token };
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
            gcontacts::sync(&self.store, account_id, &api, now).await
        }
        .await;
        match &result {
            Ok(r) => tracing::info!(account_id, sent = r.sent, added = r.added, changed = r.changed, removed = r.removed, "google contacts synced"),
            Err(e) => {
                tracing::warn!(account_id, "google contacts: {e:#}");
                let why = if e.downcast_ref::<gcontacts::PeopleError>().is_some_and(|p| matches!(p, gcontacts::PeopleError::SignIn(_))) {
                    "Google needs you to connect again.".to_string()
                } else {
                    format!("{e:#}")
                };
                let _ = self.store.set_google_sync(account_id, None, None, Some(&why));
            }
        }
        result
    }

    /// Sync every linked account; whether anything changed here.
    pub async fn google_sync_all(&self) -> bool {
        let mut any = false;
        for l in self.store.google_links().unwrap_or_default() {
            if let Ok(r) = self.google_sync(l.account_id).await {
                // Sent counts too: a new contact comes back with Google's groups (My Contacts).
                any |= r.any();
            }
        }
        any
    }

    /// The correspondents list, built (now, if the background build hasn't finished) and
    /// refreshed in the background when it's old.
    fn ensure_contacts(&self) -> Result<()> {
        let stale = {
            let c = self.contacts.lock().unwrap();
            c.list.is_none() || c.built.is_some_and(|t| t.elapsed() > CONTACTS_REFRESH)
        };
        if stale {
            self.refresh_contacts();
        }
        if self.contacts.lock().unwrap().list.is_none() {
            // First use before the background build finished: build it now.
            let list = self.build_contacts()?;
            let mut c = self.contacts.lock().unwrap();
            c.list.get_or_insert(list);
            c.built.get_or_insert_with(std::time::Instant::now);
        }
        Ok(())
    }

    /// A domain whose picture may be looked up: one the user corresponds with, where they've
    /// written to someone or someone in the address book has an address. Looking a domain up
    /// contacts it, so a stranger's mail (a spammer's, which can name any domain it owns) never
    /// makes this computer tell them it's here and reading.
    fn known_domain(&self, domain: &str) -> Result<bool> {
        let at = |addr: &str| icons::icon_domain(addr).as_deref() == Some(domain);
        if self.store.address_book_emails()?.iter().any(|e| at(e)) {
            return Ok(true);
        }
        self.ensure_contacts()?;
        let c = self.contacts.lock().unwrap();
        Ok(c.list.iter().flatten().any(|p| p.sent > 0.0 && at(&p.addr)))
    }

    fn build_contacts(&self) -> Result<Vec<contacts::Contact>> {
        Ok(contacts::build(self.store.contact_rows()?, &not_suggested(&self.store)?))
    }

    /// Take `addr` out of the address suggestions, now and from then on.
    pub fn forget_address(&self, addr: &str) -> Result<()> {
        self.store.forget_address(addr)?;
        let addr = addr.trim().to_ascii_lowercase();
        if let Some(list) = self.contacts.lock().unwrap().list.as_mut() {
            list.retain(|c| !c.addr.eq_ignore_ascii_case(&addr));
        }
        Ok(())
    }

    /// Rebuild the suggestions in the background (once at a time).
    fn refresh_contacts(&self) {
        {
            let mut c = self.contacts.lock().unwrap();
            if c.building {
                return;
            }
            c.building = true;
        }
        let (store, contacts) = (self.store.clone(), self.contacts.clone());
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let list = not_suggested(&store).and_then(|me| Ok(contacts::build(store.contact_rows()?, &me)));
            let mut c = contacts.lock().unwrap();
            c.building = false;
            match list {
                Ok(list) => {
                    tracing::info!(n = list.len(), ms = t.elapsed().as_millis() as u64, "address suggestions built");
                    c.list = Some(list);
                    c.built = Some(std::time::Instant::now());
                }
                Err(e) => tracing::warn!("address suggestions: {e:#}"),
            }
        });
    }

    /// Mail just sent: its recipients go to the top of the suggestions now.
    fn note_sent_to(&self, recipients: &[Addr]) {
        let now = chrono::Utc::now().timestamp();
        let mut c = self.contacts.lock().unwrap();
        let Some(list) = c.list.as_mut() else { return };
        for r in recipients {
            let addr = r.addr.trim().to_ascii_lowercase();
            match list.iter_mut().find(|x| x.addr == addr) {
                Some(x) => {
                    x.sent += contacts::weight(now);
                    x.last = now;
                    if r.name.is_some() {
                        x.name = r.name.clone();
                    }
                }
                None => list.push(contacts::Contact { name: r.name.clone(), addr, sent: contacts::weight(now), received: 0.0, last: now }),
            }
        }
    }

    /// The next unread item below `after` in a folder list (wrapping to the top), or with `back`
    /// the previous one above it (wrapping to the bottom).
    pub fn next_unread(
        &self,
        folder_id: i64,
        after: Option<i64>,
        threaded: bool,
        back: bool,
        sort: sort::ListSort,
        filter: &sort::ListFilter,
    ) -> Result<Option<NextUnread>> {
        Ok(self.store.next_unread_by(folder_id, after, threaded, back, sort, filter)?.map(|(id, index)| NextUnread { id, index }))
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

    /// The messages from a message's sender (see `Store::sender_messages`).
    pub fn sender_messages(&self, message_id: i64, offset: u32, limit: u32) -> Result<Option<SenderPage>> {
        self.store.sender_messages(message_id, offset, limit)
    }

    /// The messages from an address in an account (see `Store::messages_from`).
    pub fn messages_from(&self, account_id: i64, addr: &str, offset: u32, limit: u32) -> Result<SenderPage> {
        self.store.messages_from(account_id, "", addr, offset, limit)
    }

    // ---- writing help (see `ai`) ---------------------------------------------------------

    /// Keep (Some) or forget (None) the Anthropic API key, in the system keychain.
    pub fn set_ai_key(&self, key: Option<&str>) -> Result<()> {
        match key.map(str::trim).filter(|k| !k.is_empty()) {
            Some(k) => self.secrets.set(ai::SECRET_NAME, k),
            None => {
                self.secrets.delete(ai::SECRET_NAME);
                Ok(())
            }
        }
    }

    /// Is there an API key to use?
    pub fn ai_configured(&self) -> bool {
        self.secrets.get(ai::SECRET_NAME).is_ok_and(|k| !k.is_empty())
    }

    /// Run a writing task with `model`, its text streamed to `on_text` as it comes.
    pub async fn ai_run(
        &self,
        model: &str,
        task: &ai::Task,
        request: &ai::Request,
        cancel: tokio::sync::watch::Receiver<bool>,
        on_text: impl FnMut(&str),
    ) -> Result<ai::Outcome> {
        let key = self
            .secrets
            .get(ai::SECRET_NAME)
            .map_err(|_| anyhow!("Add your Anthropic API key in Settings › AI first."))?;
        ai::run(&key, model, task, request, cancel, on_text).await
    }

    pub fn signatures(&self) -> Result<signatures::Signatures> {
        self.store.signatures()
    }

    pub fn save_signature(&self, id: Option<i64>, name: &str, html: &str) -> Result<i64> {
        self.store.save_signature(id, name, &mime::sanitize_signature(html))
    }

    pub fn delete_signature(&self, id: i64) -> Result<()> {
        self.store.delete_signature(id)
    }

    pub fn set_signature_defaults(&self, account_id: i64, new_id: Option<i64>, reply_id: Option<i64>) -> Result<()> {
        self.store.set_signature_defaults(account_id, new_id, reply_id)
    }

    pub fn message_in_folder(&self, id: i64, folder_id: Option<i64>) -> Result<bool> {
        self.store.message_in_folder(id, folder_id)
    }

    pub fn list_threads(&self, folder_id: i64, scope: ThreadScope, filter: &sort::ListFilter, offset: u32, limit: u32) -> Result<ThreadPage> {
        self.store.list_threads_filtered(folder_id, scope, filter, offset, limit)
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

    /// Where message `id` is among a search's results, or None if it isn't one.
    pub fn search_index_of(&self, account_id: i64, folder_id: Option<i64>, query: &str, fields: &[&str], id: i64) -> Result<Option<u32>> {
        self.store.search_index_of(account_id, folder_id, query, fields, id)
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
                    // Rules trust the From address only as far as the account's server checks
                    // it (`from_believed`); a whole domain's only when the user turned those on.
                    let believed = self.from_believed(id, d.auth_server.as_deref(), d.sender_verified)?;
                    if !believed {
                        d.sender_domain = None;
                    }
                    let domains = d.sender_domain.is_some() && self.store.domain_rules()?;
                    d.remote_allowed = self.store.allow_remote(id, domains, believed)?;
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

    /// The pictures accounts have as their markers: (account, data: URL).
    pub fn account_logos(&self) -> Result<Vec<(i64, String)>> {
        self.store.account_logos()
    }

    pub fn set_account_logo(&self, account_id: i64, data_url: Option<&str>) -> Result<()> {
        self.store.set_account_logo(account_id, data_url)?;
        let _ = self.events.send(CoreEvent::FoldersChanged { account_id });
        Ok(())
    }

    /// Always load remote images in mail from this message's sender; returns the address.
    pub fn allow_remote_from_sender(&self, id: i64) -> Result<String> {
        let addr = self.store.message_from_addr(id)?.filter(|a| !a.trim().is_empty()).ok_or_else(|| anyhow!("This message has no sender."))?;
        self.store.set_remote_sender(&addr, true)?;
        Ok(addr)
    }

    /// Whether picture rules may believe message `id` is from its From address. Where the
    /// account's server checks senders, only when that check vouched for the sender's domain
    /// (`vouched`, `mime::sender_vouched`); elsewhere the address is taken as written, and a
    /// rule is only as safe as that. A server checks when it's Gmail or Microsoft (they write
    /// their own Authentication-Results on top of every message they receive), or when the
    /// topmost one carries the name of the account's own server (`auth_server`): then that's
    /// the one it wrote. A server that writes none leaves the sender free to write one, so a
    /// name that doesn't match proves nothing, and only ever makes this stricter.
    fn from_believed(&self, id: i64, auth_server: Option<&str>, vouched: bool) -> Result<bool> {
        let Some(account_id) = self.store.message_account(id)? else { return Ok(false) };
        let Some(account) = self.store.accounts()?.into_iter().find(|a| a.id == account_id) else { return Ok(false) };
        let own = |host: &str| icons::registrable_domain(&format!("x@{host}"));
        let checks = matches!(account.provider, Provider::Gmail | Provider::Outlook)
            || auth_server.is_some_and(|s| {
                [Some(account.imap_host.as_str()), account.smtp_host.as_deref()].into_iter().flatten().any(|h| own(h).as_deref() == Some(s))
            });
        Ok(!checks || vouched)
    }

    /// Always load remote images in mail from everyone at this message's sender's domain;
    /// returns it ("@papercut.com"). Only when the user turned such rules on (Settings ›
    /// General › Privacy), not for gmail.com and the like (anyone can write from there), and
    /// where the account's server checks senders, only when it vouched for this one.
    pub fn allow_remote_from_domain(&self, id: i64) -> Result<String> {
        if !self.store.domain_rules()? {
            bail!("Rules for everyone at a domain are off: allow this sender instead.");
        }
        let raw = self.store.body(id)?.ok_or_else(|| anyhow!("This message isn't downloaded yet."))?;
        let msg = mail_parser::MessageParser::default().parse(&raw).ok_or_else(|| anyhow!("This message can't be read."))?;
        let addr = self.store.message_from_addr(id)?.filter(|a| !a.trim().is_empty()).ok_or_else(|| anyhow!("This message has no sender."))?;
        let domain = icons::icon_domain(&addr).ok_or_else(|| anyhow!("Anyone can have an address there: allow this sender instead."))?;
        if !self.from_believed(id, mime::auth_server(&msg).as_deref(), mime::sender_vouched(&msg, &addr))? {
            bail!("Nothing shows this message is really from {domain}: allow this sender instead.");
        }
        let entry = format!("@{domain}");
        self.store.set_remote_sender(&entry, true)?;
        Ok(entry)
    }

    /// Whether "Always load…" can cover everyone at a domain (off unless the user turns it on).
    pub fn domain_rules(&self) -> Result<bool> {
        self.store.domain_rules()
    }

    /// Whether search reads the text in attachments (off unless turned on).
    pub fn attachment_search(&self) -> Result<bool> {
        self.store.attachment_search()
    }

    pub fn set_attachment_search(&self, on: bool) -> Result<()> {
        self.store.set_attachment_search(on)
    }

    pub fn set_domain_rules(&self, on: bool) -> Result<()> {
        self.store.set_domain_rules(on)
    }

    /// Whether a picture in a message that only loads over plain http may be fetched that way
    /// (`icons::mail_image`), its address visible on the network. On unless turned off.
    pub fn http_pictures(&self) -> Result<bool> {
        self.store.choice("http_pictures", true)
    }

    pub fn set_http_pictures(&self, on: bool) -> Result<()> {
        self.store.set_choice("http_pictures", on)
    }

    pub fn remote_senders(&self) -> Result<Vec<String>> {
        self.store.remote_senders()
    }

    pub fn set_remote_sender(&self, addr: &str, on: bool) -> Result<()> {
        self.store.set_remote_sender(addr, on)
    }

    /// Remember the user's "Load images" choice for a message.
    pub fn set_allow_remote(&self, id: i64, allow: bool) -> Result<()> {
        self.store.set_allow_remote(id, allow)
    }

    /// Mark a message read or unread: instantly in the local store, then on the server.
    /// Read or unread, as an action (opening a message marks it read): through the same queue
    /// as "Mark unread" and the rest, so the server gets the changes in the order they were made.
    /// (A separate fast path for "read" could reach the server after a later "unread", and the
    /// next sync then put "read" back.) Already in that state: nothing to send.
    pub fn set_read(&self, id: i64, read: bool) -> Result<()> {
        let seen = self.store.message_flags(id)? & flags::SEEN != 0;
        if seen == read {
            return Ok(());
        }
        let Some(&folder) = self.store.message_folder_ids(id)?.first() else { return Ok(()) };
        let action = if read { actions::Action::Read } else { actions::Action::Unread };
        self.apply_action(action, &[id], folder).map(|_| ())
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
                message: Some(match self.store.accounts().ok().and_then(|l| l.into_iter().find(|a| a.id == account_id)).map(|a| a.auth) {
                    Some(AuthKind::Password) | None => "No saved password for this account.".into(),
                    Some(_) => "No saved sign-in for this account.".into(),
                }),
            });
        }
        out
    }
}

/// A file name for a message forwarded as an attachment: its subject, made safe.
fn eml_name(subject: &str) -> String {
    let clean: String = subject
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let short: String = clean.chars().take(80).collect();
    format!("{}.eml", if short.is_empty() { "message".to_string() } else { short })
}

/// Apply an action locally and queue its server commands (the caller wakes the outbox). Shared
/// by `Core::apply_action` and the sync's snooze waker.
pub(crate) fn queue_action(
    store: &Store,
    events: &broadcast::Sender<CoreEvent>,
    action: actions::Action,
    message_ids: &[i64],
    folder_id: i64,
) -> Result<ActionResult> {
    let (account_id, _) = store.folder(folder_id)?.ok_or_else(|| anyhow!("unknown folder"))?;
    let account = store.accounts()?.into_iter().find(|a| a.id == account_id).ok_or_else(|| anyhow!("no such account"))?;
    // Exchange keeps no keywords; with Graph allowed, tags and snooze go there instead.
    if action.needs_keywords() && account.keywords == Some(false) && !account.graph {
        bail!("This account's mail server doesn't keep tags or snooze times.");
    }
    let gmail = account.provider == Provider::Gmail;
    let snooze = matches!(action, actions::Action::Snooze { .. } | actions::Action::Unsnooze | actions::Action::Wake);
    let planned = actions::plan(store, account_id, gmail, action, message_ids, folder_id)?;
    let snapshot = store.snapshot(message_ids)?;
    let mut changed = store.apply_local(&planned.change)?;
    if snooze {
        // Gone off: at the top of the Inbox by that time; snoozed again: back to its own date.
        store.settle_inbox_dates(message_ids)?;
    }
    // Rows show flags and tags, so every folder holding these messages refreshes, not only
    // those whose counts changed (a tag or a non-Gmail star changes no count).
    for id in message_ids {
        changed.extend(store.message_folder_ids(*id)?);
    }
    changed.sort_unstable();
    changed.dedup();
    let _ = events.send(CoreEvent::MessagesChanged { folder_ids: changed });
    let _ = events.send(CoreEvent::FoldersChanged { account_id });
    if planned.ops.is_empty() {
        return Ok(ActionResult { op_id: None, undoable: false });
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    // Ops run in queue order among those due, so an op that looks its message up (a copy an
    // earlier queued move put there) waits until everything queued before it is due too:
    // a star right after a move otherwise reaches the server before the move does.
    let mut not_before = now + planned.delay_ms;
    if planned.ops.iter().any(|op| op.is_lookup()) {
        not_before = not_before.max(store.last_op_due(account_id)?.unwrap_or(0));
    }
    let op_id = store.enqueue_ops(
        account_id,
        &serde_json::to_string(&planned.ops)?,
        &serde_json::to_string(&snapshot)?,
        &serde_json::to_string(&planned.folders)?,
        not_before,
    )?;
    Ok(ActionResult { op_id: Some(op_id), undoable: planned.delay_ms > 0 })
}

/// Addresses never suggested: the user's own, and those they removed from the suggestions.
fn not_suggested(store: &Store) -> Result<Vec<String>> {
    let mut out: Vec<String> = store.accounts()?.into_iter().map(|a| a.email.to_ascii_lowercase()).collect();
    out.extend(store.forgotten_addresses()?);
    Ok(out)
}

#[cfg(test)]
mod picture_tests {
    use super::*;
    use crate::store::{HeaderRecord, ListedFolder};

    #[tokio::test]
    async fn pictures_only_for_domains_the_user_corresponds_with() {
        let dir = std::env::temp_dir().join(format!("mail-core-pictures-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::open(&dir, Secrets::memory()).unwrap();
        let acct = core.store.insert_account("john.doe@example.com", "John Doe", Provider::Imap, "imap.example.com", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role: Some(role), selectable: true };
        core.store.sync_folder_list(acct.id, &[lf("INBOX", FolderRole::Inbox), lf("Sent", FolderRole::Sent)]).unwrap();
        let folder = |n: &str| core.store.folders(acct.id).unwrap().into_iter().find(|f| f.raw_name == n).unwrap().id;
        let (inbox, sent) = (folder("INBOX"), folder("Sent"));
        let mail = |uid: u32, from: &str, to: &str| HeaderRecord {
            uid,
            subject: format!("m{uid}"),
            from_addr: from.into(),
            to_addrs: to.into(),
            date: 1_800_000_000 + uid as i64,
            ..Default::default()
        };
        let none = std::collections::HashMap::new();
        // Written to someone at corp.example.org (in Sent); only heard from news.example.net; and
        // a message in the Inbox that claims to be from John, "to" lure.example.
        core.store.upsert_headers(acct.id, sent, &[mail(1, "john.doe@example.com", "Jane <jane@corp.example.org>")], &none).unwrap();
        core.store
            .upsert_headers(acct.id, inbox, &[
                mail(2, "deals@news.example.net", "john.doe@example.com"),
                mail(3, "john.doe@example.com", "x@lure.example"),
            ], &none)
            .unwrap();
        assert!(core.known_domain("example.org").unwrap(), "written to");
        assert!(!core.known_domain("lure.example").unwrap(), "a forged 'from me' isn't writing to them");
        assert!(!core.known_domain("example.net").unwrap(), "only mail from them: never contacted");
        assert!(!core.known_domain("spam.example").unwrap());
        // Nothing is looked up for a stranger: no picture, and no network (the domain doesn't
        // exist, so a lookup would only fail; the cache shows none was tried).
        assert_eq!(core.sender_icon("deals@news.example.net", icons::PictureScope::Known).await.unwrap(), None);
        assert!(core.store.sender_icon("example.net").unwrap().is_none(), "not even tried");
        // Contacts' photos only: not even a domain the user writes to is looked up.
        assert_eq!(core.sender_icon("jane@corp.example.org", icons::PictureScope::Contacts).await.unwrap(), None);
        assert!(core.store.sender_icon("example.org").unwrap().is_none(), "not tried");
        // Someone in the address book: their domain is known.
        core.store.save_card(addressbook::Card { name: "Sam".into(), emails: vec!["sam@shop.example.com".into()], ..Default::default() }).unwrap();
        assert!(core.known_domain("example.com").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod remote_rule_tests {
    use super::*;
    use crate::store::{HeaderRecord, ListedFolder};

    /// Picture rules believe a From address as far as the account's server checks it, and
    /// cover a whole domain only when the user turned that on.
    #[test]
    fn rules_trust_senders_as_far_as_the_server_checks() {
        let dir = std::env::temp_dir().join(format!("mail-core-remote-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::open(&dir, Secrets::memory()).unwrap();
        let none = std::collections::HashMap::new();
        // One message in an account's Inbox, with the headers given above From; its id.
        let deliver = |provider: Provider, host: &str, headers: &str| {
            let n = core.store.accounts().unwrap().len();
            let acct = core.store.insert_account(&format!("john.doe{n}@example.com"), "John Doe", provider, host, 993).unwrap();
            let lf = ListedFolder { raw_name: "INBOX".into(), path: vec!["INBOX".into()], role: Some(FolderRole::Inbox), selectable: true };
            core.store.sync_folder_list(acct.id, &[lf]).unwrap();
            let inbox = core.store.folders(acct.id).unwrap()[0].id;
            let rec = HeaderRecord { uid: 1, subject: "Hi".into(), from_addr: "news@shop.example.net".into(), date: 1_800_000_000, ..Default::default() };
            core.store.upsert_headers(acct.id, inbox, &[rec], &none).unwrap();
            let id = core.store.message_ids_for_uids(inbox, &[1]).unwrap()[0];
            let raw = format!("{headers}From: News <news@shop.example.net>\r\nTo: john.doe@example.com\r\nSubject: Hi\r\nContent-Type: text/html\r\n\r\n<img src=\"https://shop.example.net/a.png\">\r\n");
            core.store.store_body(id, raw.as_bytes(), None, false, None).unwrap();
            id
        };
        let pass = "Authentication-Results: mx.example.com; dmarc=pass header.from=shop.example.net\r\n";
        let fail = "Authentication-Results: mx.example.com; dmarc=fail header.from=shop.example.net\r\n";
        let elsewhere = "Authentication-Results: mx.other.example; dmarc=fail header.from=shop.example.net\r\n";
        let detail = |id| core.get_message(id).unwrap().unwrap();

        let gmail_pass = deliver(Provider::Gmail, "imap.gmail.com", pass);
        let gmail_fail = deliver(Provider::Gmail, "imap.gmail.com", fail);
        let own_fail = deliver(Provider::Imap, "imap.example.com", fail); // its own server's name
        let other_fail = deliver(Provider::Imap, "imap.example.com", elsewhere); // anyone's
        let plain = deliver(Provider::Imap, "imap.example.com", "");

        // Off by default: no domain rule is offered or added, and a saved one doesn't count.
        assert!(!core.domain_rules().unwrap());
        assert!(core.allow_remote_from_domain(gmail_pass).is_err());
        core.set_remote_sender("@example.net", true).unwrap();
        assert!(!detail(gmail_pass).remote_allowed);
        assert!(!detail(plain).remote_allowed);

        core.set_domain_rules(true).unwrap();
        // The server checked and vouched; or it doesn't check, so the address is taken as written.
        for id in [gmail_pass, other_fail, plain] {
            let d = detail(id);
            assert_eq!(d.sender_domain.as_deref(), Some("example.net"), "{id}");
            assert!(d.remote_allowed, "{id}");
        }
        // The server checked and it failed: neither rule counts, and none can be added.
        for id in [gmail_fail, own_fail] {
            let d = detail(id);
            assert_eq!(d.sender_domain, None, "{id}");
            assert!(!d.remote_allowed, "{id}");
            assert!(core.allow_remote_from_domain(id).is_err(), "{id}");
        }
        core.set_remote_sender("@example.net", false).unwrap();
        core.set_remote_sender("news@shop.example.net", true).unwrap();
        assert!(detail(plain).remote_allowed && detail(gmail_pass).remote_allowed);
        assert!(!detail(own_fail).remote_allowed, "a sender's rule needs the check too");
        assert_eq!(core.allow_remote_from_domain(plain).unwrap(), "@example.net");
        // Pictures over plain http: on until turned off, and kept.
        assert!(core.http_pictures().unwrap());
        core.set_http_pictures(false).unwrap();
        assert!(!core.http_pictures().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod vault_tests {
    use super::*;

    #[test]
    fn the_vault_in_pieces_and_back() {
        let mut map = HashMap::new();
        map.insert("me@x.com".to_string(), "pässwörd-ü".repeat(300));
        map.insert("#ai:anthropic".to_string(), "sk-ant-".to_string() + &"k".repeat(120));
        let json = serde_json::to_string(&map).unwrap();
        // Small enough: one item, as on macOS and Linux.
        assert_eq!(vault_pieces(&json, usize::MAX), vec![json.clone()]);
        assert_eq!(vault_pieces("{}", 2), vec!["{}".to_string()], "exactly at the limit");
        // Windows-sized pieces: a marker, then parts of at most 1000 characters, multi-byte safe.
        let pieces = vault_pieces(&json, 1000);
        assert!(pieces.len() > 2);
        assert!(pieces[1..].iter().all(|p| p.chars().count() <= 1000));
        let back = vault_join(&pieces[0], |i| Ok(pieces[i].clone())).unwrap();
        assert_eq!(back, json);
        let again: HashMap<String, String> = serde_json::from_str(&back).unwrap();
        assert_eq!(again, map);
        // A vault saved whole (before, or elsewhere) reads as it is.
        assert_eq!(vault_join(&json, |_| panic!("no parts")).unwrap(), json);
        // A missing part is an error, not an empty vault.
        assert!(vault_join(&pieces[0], |_| anyhow::bail!("gone")).is_err());
    }

    /// A save changes only its own secret in the vault as it's in the keychain now: never puts
    /// back an old copy over what another copy of the app saved, nor saves over a keychain that
    /// didn't answer.
    #[test]
    fn vault_saves_change_only_their_secret() {
        let m = |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
        let cached = m(&[("a@example.com", "pw")]);
        let fresh = m(&[("a@example.com", "pw"), ("#google-contacts:5", "token")]);
        let add = |map: &mut HashMap<String, String>| {
            map.insert("b@example.com".into(), "pw2".into());
            true
        };
        // Saved by another copy since this one read it: kept.
        let saved = vault_updated(Some(&cached), Ok(Some(fresh.clone())), add).unwrap().unwrap();
        assert_eq!(saved, m(&[("a@example.com", "pw"), ("#google-contacts:5", "token"), ("b@example.com", "pw2")]));
        // Removing: only that one; nothing to remove, nothing saved.
        let saved = vault_updated(Some(&cached), Ok(Some(fresh.clone())), |map| map.remove("a@example.com").is_some()).unwrap().unwrap();
        assert_eq!(saved, m(&[("#google-contacts:5", "token")]));
        assert_eq!(vault_updated(Some(&cached), Ok(Some(fresh)), |map| map.remove("x").is_some()).unwrap(), None);
        // The keychain not answering: refused, not taken as empty.
        assert!(vault_updated(Some(&cached), Err(anyhow!("locked")), add).is_err());
        assert!(vault_updated(Some(&cached), Ok(None), add).is_err(), "no item, yet there were secrets");
        // A first secret ever: saved.
        assert_eq!(vault_updated(None, Ok(None), add).unwrap().unwrap(), m(&[("b@example.com", "pw2")]));
        // Names in the log: no addresses.
        assert_eq!(secret_name("a@example.com"), "<account password>");
        assert_eq!(secret_name("a@example.com#graph"), "<account>#graph");
        assert_eq!(secret_name("#google-contacts:5"), "#google-contacts:5");
    }
}

#[cfg(test)]
mod local_draft_tests {
    use super::*;
    use crate::store::ListedFolder;

    /// A draft written here is only here: Delete on its copy in Drafts discards it, rather than
    /// failing for want of a server copy ("isn't on the server yet").
    #[test]
    fn deleting_a_local_draft_discards_it() {
        let dir = std::env::temp_dir().join(format!("mail-core-local-draft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::open(&dir, Secrets::memory()).unwrap();
        let acct = core.store.insert_account("john.doe@example.com", "John Doe", Provider::Imap, "imap.example.com", 993).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role: Some(role), selectable: true };
        core.store.sync_folder_list(acct.id, &[lf("INBOX", FolderRole::Inbox), lf("Drafts", FolderRole::Drafts), lf("Trash", FolderRole::Trash)]).unwrap();
        let drafts = core.store.folders(acct.id).unwrap().into_iter().find(|f| f.raw_name == "Drafts").unwrap().id;
        let req = |subject: &str| -> SendRequest {
            serde_json::from_value(serde_json::json!({
                "accountId": acct.id, "to": [{ "addr": "jane@example.org" }], "subject": subject,
                "html": "<p>Hi</p>", "replyingTo": null, "forwarding": null, "draft": null, "listed": true,
            }))
            .unwrap()
        };
        let (a, b) = (core.save_draft(None, req("One")).unwrap(), core.save_draft(None, req("Two")).unwrap());
        let rows = core.store.list_messages(drafts, 0, 10).unwrap().messages;
        assert_eq!(rows.len(), 2);
        let copy_of = |d: i64| rows.iter().find(|m| m.draft_id == Some(d)).unwrap().id;
        for action in [actions::Action::Trash, actions::Action::DeleteForever] {
            let id = copy_of(if matches!(action, actions::Action::Trash) { a } else { b });
            let r = core.apply_action(action, &[id], drafts).unwrap();
            assert_eq!(r.op_id, None, "nothing for the server");
        }
        assert!(core.store.list_messages(drafts, 0, 10).unwrap().messages.is_empty());
        assert!(core.store.draft(a).unwrap().is_none() && core.store.draft(b).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod send_as_tests {
    use super::*;
    use crate::identities::Identity;
    use crate::store::{HeaderRecord, ListedFolder};

    fn identity(account_id: i64, email: &str, name: &str, default: bool) -> Identity {
        Identity { id: 0, account_id, email: email.into(), name: name.into(), new_signature_id: None, reply_signature_id: None, is_default: default }
    }

    /// Another address of the account: kept and checked; mail goes out with it in From (and its
    /// name), through the account's server with the account's own address as the envelope
    /// sender; replies go out from the address the mail came to, and leave every one of the
    /// account's addresses out of the recipients.
    #[tokio::test]
    async fn sending_as_another_address() {
        let dir = std::env::temp_dir().join(format!("mail-core-send-as-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::open(&dir, Secrets::memory()).unwrap();
        let acct = core.store.insert_account("lars@pehrsson.info", "Lars Pehrsson", Provider::Imap, "imap.example.com", 993).unwrap();
        core.store.set_smtp_server(acct.id, Some(("smtp.example.com", 587))).unwrap();
        let lf = |name: &str, role| ListedFolder { raw_name: name.into(), path: vec![name.into()], role: Some(role), selectable: true };
        core.store.sync_folder_list(acct.id, &[lf("INBOX", FolderRole::Inbox), lf("Sent", FolderRole::Sent), lf("Drafts", FolderRole::Drafts)]).unwrap();

        // Kept and checked.
        let au = core.save_identity(identity(acct.id, " lars@pehrsson.au ", "Lars", true)).unwrap();
        let shop = core.save_identity(identity(acct.id, "shop@store.example", "", false)).unwrap();
        for bad in ["lars@pehrsson.info", "LARS@pehrsson.au", "nobody"] {
            assert!(core.save_identity(identity(acct.id, bad, "", false)).is_err(), "{bad}");
        }
        let list = core.identities().unwrap();
        assert_eq!(list.iter().map(|i| (i.email.as_str(), i.is_default)).collect::<Vec<_>>(), [("lars@pehrsson.au", true), ("shop@store.example", false)]);
        // One default per account: making another the default takes it from the first.
        let mut s = list[1].clone();
        s.is_default = true;
        core.save_identity(s).unwrap();
        assert_eq!(core.identities().unwrap().iter().filter(|i| i.is_default).map(|i| i.id).collect::<Vec<_>>(), [shop]);
        // Its signatures: the account's (None), one of its own, or none at all (0), kept apart.
        let sig = core.store.save_signature(None, "Shop", "<p>The shop</p>").unwrap();
        let mut s = core.identities().unwrap().into_iter().find(|i| i.id == shop).unwrap();
        (s.new_signature_id, s.reply_signature_id) = (Some(sig), Some(0));
        core.save_identity(s.clone()).unwrap();
        let sigs_of = |id| core.identities().unwrap().into_iter().find(|i| i.id == id).map(|i| (i.new_signature_id, i.reply_signature_id)).unwrap();
        assert_eq!(sigs_of(shop), (Some(sig), Some(0)));
        assert_eq!(sigs_of(au), (None, None));
        (s.new_signature_id, s.reply_signature_id) = (Some(0), None);
        core.save_identity(s).unwrap();
        assert_eq!(sigs_of(shop), (Some(0), None));
        // A signature deleted: an address that had chosen it has none (not its account's).
        let other = core.store.save_signature(None, "Other", "<p>Other</p>").unwrap();
        let mut s = core.identities().unwrap().into_iter().find(|i| i.id == shop).unwrap();
        s.reply_signature_id = Some(other);
        core.save_identity(s).unwrap();
        core.store.set_signature_defaults(acct.id, Some(sig), Some(sig)).unwrap();
        core.delete_signature(other).unwrap();
        assert_eq!(sigs_of(shop), (Some(0), Some(0)));

        // Sent as it: From has it and its name; the envelope is the account's.
        let req = |from_identity: Option<i64>| -> SendRequest {
            let mut r: SendRequest = serde_json::from_value(serde_json::json!({
                "accountId": acct.id, "to": [{ "addr": "bob@example.org" }], "subject": "Hi", "html": "<p>Hi</p>",
                "replyingTo": null, "forwarding": null, "draft": null,
            }))
            .unwrap();
            r.from_identity = from_identity;
            r
        };
        core.send_message(req(Some(au))).await.unwrap();
        core.send_message(req(Some(shop))).await.unwrap();
        core.send_message(req(None)).await.unwrap();
        // (What the sender would take, past the undo window, oldest first.)
        let sent: Vec<(String, String)> = std::iter::from_fn(|| core.store.claim_due_outgoing(acct.id, i64::MAX / 2).unwrap())
            .map(|o| (String::from_utf8_lossy(&o.raw).into_owned(), o.envelope_from))
            .collect();
        assert_eq!(sent.len(), 3);
        let from = |raw: &str| raw.lines().find(|l| l.starts_with("From:")).unwrap_or_default().to_string();
        assert!(from(&sent[0].0).contains("Lars") && from(&sent[0].0).contains("<lars@pehrsson.au>"), "{}", from(&sent[0].0));
        assert!(from(&sent[1].0).contains("Lars Pehrsson") && from(&sent[1].0).contains("<shop@store.example>"), "no name of its own: the account's: {}", from(&sent[1].0));
        assert!(from(&sent[2].0).contains("<lars@pehrsson.info>"), "{}", from(&sent[2].0));
        assert!(sent.iter().all(|(_, env)| env == "lars@pehrsson.info"), "the account's own address as the envelope sender");
        // An address that isn't the account's (or no longer is): not sent.
        assert!(core.send_message(req(Some(9999))).await.is_err());

        // A reply goes out from the address the mail came to, and leaves all of them out.
        let inbox = core.store.folders(acct.id).unwrap().into_iter().find(|f| f.raw_name == "INBOX").unwrap().id;
        let none = std::collections::HashMap::new();
        let mail = |uid: u32, to: &str, delivered: &str| {
            let rec = HeaderRecord { uid, subject: format!("m{uid}"), from_addr: "bob@example.org".into(), to_addrs: to.into(), date: 1_800_000_000, ..Default::default() };
            core.store.upsert_headers(acct.id, inbox, &[rec], &none).unwrap();
            let id = core.store.message_ids_for_uids(inbox, &[uid]).unwrap()[0];
            let raw = format!("{delivered}From: Bob <bob@example.org>\r\nTo: {to}\r\nCc: lars@pehrsson.au, ann@example.org\r\nSubject: m{uid}\r\nContent-Type: text/plain\r\n\r\nHello\r\n");
            core.store.store_body(id, raw.as_bytes(), None, false, None).unwrap();
            id
        };
        let to_shop = mail(1, "shop@store.example", "");
        let ctx = core.reply_context(to_shop, ReplyMode::ReplyAll).await.unwrap();
        assert_eq!(ctx.from_identity, Some(shop));
        let all: Vec<String> = ctx.to.iter().chain(&ctx.cc).map(|a| a.addr.clone()).collect();
        assert_eq!(all, ["bob@example.org", "ann@example.org"], "none of the account's addresses");
        // Came to the account's own address: answered from it.
        let to_me = mail(2, "lars@pehrsson.info", "");
        assert_eq!(core.reply_context(to_me, ReplyMode::Reply).await.unwrap().from_identity, Some(au), "Cc has .au: that one");
        // Only the server's note says where it came to (a catch-all, Bcc): that domain's.
        let bcc = mail(3, "list@lists.example", "Delivered-To: webshop@pehrsson.au\r\n");
        assert_eq!(core.reply_context(bcc, ReplyMode::Reply).await.unwrap().from_identity, Some(au));

        // Removing the account removes its addresses.
        core.delete_identity(au).unwrap();
        assert_eq!(core.identities().unwrap().len(), 1);
        core.store.delete_account(acct.id).unwrap();
        assert!(core.identities().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
