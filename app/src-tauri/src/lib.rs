//! Tauri shell: exposes the mail core to the UI as commands and forwards core events.
//! Commands are `async` so they run on the async runtime, not the main (UI) thread.
//! See `app/src/api-contract.md` for the contract.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use mail_core::model::*;
use mail_core::{Core, NewAccount, Secrets};
use serde::Serialize;
use tauri::{Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

mod window_state;

type Res<T> = Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
async fn list_accounts(core: State<'_, Arc<Core>>) -> Res<Vec<Account>> {
    core.accounts().map_err(err)
}

/// Args arrive as top-level camelCase keys (`displayName`, `imapHost`, ...).
#[tauri::command]
async fn add_account(
    core: State<'_, Arc<Core>>,
    email: String,
    password: String,
    display_name: Option<String>,
    imap_host: Option<String>,
    imap_port: Option<u16>,
) -> Res<Account> {
    core.add_account(NewAccount { email, password, display_name, imap_host, imap_port })
        .await
        .map_err(err)
}

#[tauri::command]
async fn list_folders(core: State<'_, Arc<Core>>, account_id: i64) -> Res<Vec<Folder>> {
    core.folders(account_id).map_err(err)
}

#[tauri::command]
async fn open_folder(core: State<'_, Arc<Core>>, folder_id: i64) -> Res<()> {
    core.open_folder(folder_id).map_err(err)
}

#[tauri::command]
async fn list_messages(core: State<'_, Arc<Core>>, folder_id: i64, offset: u32, limit: u32) -> Res<MessagePage> {
    core.list_messages(folder_id, offset, limit).map_err(err)
}

/// `allow_remote` is enforced by the reader's iframe CSP; the core always returns the same
/// sanitized HTML.
#[tauri::command]
async fn get_message(core: State<'_, Arc<Core>>, id: i64, allow_remote: Option<bool>) -> Res<Option<MessageDetail>> {
    let _ = allow_remote;
    core.get_message(id).map_err(err)
}

#[tauri::command]
async fn set_read(core: State<'_, Arc<Core>>, id: i64, read: bool) -> Res<()> {
    core.set_read(id, read).map_err(err)
}

#[tauri::command]
async fn set_allow_remote(core: State<'_, Arc<Core>>, id: i64, allow: bool) -> Res<()> {
    core.set_allow_remote(id, allow).map_err(err)
}

fn thread_scope(scope: Option<ConversationScope>, include_trash: Option<bool>) -> ThreadScope {
    ThreadScope {
        all_folders: scope == Some(ConversationScope::All),
        include_trash: include_trash.unwrap_or(false),
    }
}

#[tauri::command]
async fn list_threads(
    core: State<'_, Arc<Core>>,
    folder_id: i64,
    offset: u32,
    limit: u32,
    scope: Option<ConversationScope>,
    include_trash: Option<bool>,
) -> Res<ThreadPage> {
    core.list_threads(folder_id, thread_scope(scope, include_trash), offset, limit).map_err(err)
}

#[tauri::command]
async fn get_thread(
    core: State<'_, Arc<Core>>,
    thread_id: i64,
    folder_id: i64,
    scope: Option<ConversationScope>,
    include_trash: Option<bool>,
) -> Res<ThreadView> {
    core.thread_messages(thread_id, folder_id, thread_scope(scope, include_trash)).map_err(err)
}

#[tauri::command]
async fn update_password(core: State<'_, Arc<Core>>, account_id: i64, password: String) -> Res<()> {
    core.update_password(account_id, password).await.map_err(err)
}

#[tauri::command]
async fn search_messages(
    core: State<'_, Arc<Core>>,
    account_id: i64,
    folder_id: Option<i64>,
    query: String,
    offset: u32,
    limit: u32,
    fields: Option<Vec<String>>,
) -> Res<MessagePage> {
    let fields: Vec<&str> = fields.iter().flatten().map(String::as_str).collect();
    core.search(account_id, folder_id, &query, &fields, offset, limit).map_err(err)
}

#[tauri::command]
async fn message_in_folder(core: State<'_, Arc<Core>>, id: i64, folder_id: Option<i64>) -> Res<bool> {
    core.message_in_folder(id, folder_id).map_err(err)
}

/// Where opened attachments are written before handing them to the default app. Cleared at
/// startup; files there are never marked executable.
fn opened_attachments_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    app.path().app_cache_dir().map(|d| d.join("attachments")).map_err(|e| e.to_string())
}

#[tauri::command]
async fn open_attachment(
    app: tauri::AppHandle,
    core: State<'_, Arc<Core>>,
    message_id: i64,
    index: usize,
) -> Res<()> {
    let (name, _mime, data) = core.attachment(message_id, index).await.map_err(err)?;
    if !mail_core::mime::is_openable(&name) {
        return Err("This file type can't be opened directly. Save it, and open it yourself only if you trust it.".into());
    }
    let dir = opened_attachments_dir(&app)?.join(format!("{message_id}-{index}"));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(mail_core::mime::safe_file_name(&name));
    std::fs::write(&path, &data).map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(|e| format!("Couldn't open {name}: {e}"))
}

/// Save to the Downloads folder without overwriting: `name (1).ext`, `name (2).ext`, ...
#[tauri::command]
async fn save_attachment(
    app: tauri::AppHandle,
    core: State<'_, Arc<Core>>,
    message_id: i64,
    index: usize,
) -> Res<String> {
    let (name, _mime, data) = core.attachment(message_id, index).await.map_err(err)?;
    let path = write_unique(&downloads_dir(&app)?, &name, &data)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Write `data` as `name` in `dir` without overwriting: `name (1).ext`, `name (2).ext`, ...
fn write_unique(dir: &std::path::Path, name: &str, data: &[u8]) -> Result<std::path::PathBuf, String> {
    use std::io::Write;
    let safe = mail_core::mime::safe_file_name(name);
    let (stem, ext) = match safe.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (safe.clone(), String::new()),
    };
    for n in 0..1000 {
        let candidate = if n == 0 { safe.clone() } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(&candidate);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(data).map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Couldn't save {}: {e}", path.display())),
        }
    }
    Err(format!("Couldn't find a free file name for {name}."))
}

fn downloads_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .download_dir()
        .or_else(|_| app.path().home_dir())
        .map_err(|e| format!("No Downloads folder: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Save every attachment directly in Downloads (`name (1).ext` if taken); returns the paths.
#[tauri::command]
async fn save_all_attachments(app: tauri::AppHandle, core: State<'_, Arc<Core>>, message_id: i64) -> Res<Vec<String>> {
    let (count, _subject) = core.attachment_summary(message_id).map_err(err)?;
    if count == 0 {
        return Err("This message has no attachments.".into());
    }
    let downloads = downloads_dir(&app)?;
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let (name, _mime, data) = core.attachment(message_id, index).await.map_err(err)?;
        paths.push(write_unique(&downloads, &name, &data)?.to_string_lossy().into_owned());
    }
    Ok(paths)
}

/// Show a saved or opened attachment in the file manager (only files this app wrote).
#[tauri::command]
async fn reveal_path(app: tauri::AppHandle, path: String) -> Res<()> {
    let p = std::path::PathBuf::from(&path);
    let allowed = [app.path().download_dir().ok(), app.path().home_dir().ok(), opened_attachments_dir(&app).ok()];
    if !(p.is_file() || p.is_dir()) || !allowed.iter().flatten().any(|d| p.starts_with(d)) {
        return Err("That file isn't available.".into());
    }
    app.opener().reveal_item_in_dir(&p).map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_sync_status(core: State<'_, Arc<Core>>) -> Res<Vec<SyncStatus>> {
    Ok(core.sync_status())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountPayload {
    account_id: i64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FolderIdsPayload {
    folder_ids: Vec<i64>,
}

#[derive(Clone, Serialize)]
struct IdPayload {
    id: i64,
}

/// Forward core events to the webview. `messages-changed` and `folders-changed` can fire
/// thousands of times a second during sync, so they are coalesced into ~4 updates a second.
async fn forward_events(app: tauri::AppHandle, core: Arc<Core>) {
    let mut rx = core.subscribe();
    let mut folders_dirty: BTreeSet<i64> = BTreeSet::new();
    let mut accounts_dirty: BTreeSet<i64> = BTreeSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(CoreEvent::MessagesChanged { folder_ids }) => folders_dirty.extend(folder_ids),
                Ok(CoreEvent::FoldersChanged { account_id }) => { accounts_dirty.insert(account_id); }
                Ok(CoreEvent::MessageReady { id }) => { let _ = app.emit("message-ready", IdPayload { id }); }
                Ok(CoreEvent::Sync(status)) => { let _ = app.emit("sync-status", status); }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!("event forwarder lagged by {n}");
                }
                Err(_) => return,
            },
            _ = tick.tick() => {
                if !folders_dirty.is_empty() {
                    let folder_ids = std::mem::take(&mut folders_dirty).into_iter().collect();
                    let _ = app.emit("messages-changed", FolderIdsPayload { folder_ids });
                }
                for account_id in std::mem::take(&mut accounts_dirty) {
                    let _ = app.emit("folders-changed", AccountPayload { account_id });
                }
            }
        }
    }
}

/// The app's own pages are served from the dev server or the `tauri:` asset scheme.
fn is_app_url(url: &tauri::Url) -> bool {
    matches!(url.scheme(), "tauri" | "about" | "data")
        || matches!(url.host_str(), Some("localhost" | "tauri.localhost" | "127.0.0.1"))
}

/// Links in email open in the system browser, never inside the app.
fn create_main_window(app: &tauri::App) -> tauri::Result<()> {
    let opener = app.handle().clone();
    let nav_opener = app.handle().clone();
    let saved = window_state::load(app);
    let requested = saved.as_ref().map(|s| (s.width, s.height)).unwrap_or((1360.0, 860.0));
    let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
        .title("Email")
        .inner_size(1360.0, 860.0)
        .min_inner_size(900.0, 560.0);
    if let Some(state) = &saved {
        builder = window_state::apply(app, builder, state);
    }
    let window = builder
        .on_new_window(move |url, _features| {
            let _ = opener.opener().open_url(url.as_str(), None::<&str>);
            tauri::webview::NewWindowResponse::Deny
        })
        .on_navigation(move |url| {
            if is_app_url(url) {
                return true;
            }
            let _ = nav_opener.opener().open_url(url.as_str(), None::<&str>);
            false
        })
        .build()?;

    window_state::track(&window, saved, requested);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        // One instance per machine: a second launch focuses the existing window instead of
        // starting another sync engine on the same database and accounts.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            create_main_window(app)?;
            if let Ok(dir) = opened_attachments_dir(app.handle()) {
                let _ = std::fs::remove_dir_all(dir);
            }
            let dir = app.path().app_data_dir()?;
            let core = Arc::new(Core::open(&dir, Secrets::Keyring)?);
            tracing::info!("data directory: {}", dir.display());
            app.manage(core.clone());
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Sync tasks are spawned onto Tauri's tokio runtime from inside it.
                if let Err(e) = core.start() {
                    tracing::error!("starting sync: {e:#}");
                }
                forward_events(handle, core).await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_accounts,
            add_account,
            list_folders,
            open_folder,
            list_messages,
            get_message,
            get_sync_status,
            set_read,
            set_allow_remote,
            list_threads,
            get_thread,
            update_password,
            search_messages,
            message_in_folder,
            open_attachment,
            save_attachment,
            save_all_attachments,
            reveal_path,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
