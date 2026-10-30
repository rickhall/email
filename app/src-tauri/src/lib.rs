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
async fn apply_action(
    core: State<'_, Arc<Core>>,
    action: mail_core::actions::Action,
    message_ids: Vec<i64>,
    folder_id: i64,
) -> Res<ActionResult> {
    core.apply_action(action, &message_ids, folder_id).map_err(err)
}

#[tauri::command]
async fn undo_action(core: State<'_, Arc<Core>>, op_id: i64) -> Res<()> {
    core.undo_action(op_id).map_err(err)
}

#[tauri::command]
async fn index_of(core: State<'_, Arc<Core>>, folder_id: i64, id: i64, threaded: bool) -> Res<Option<u32>> {
    core.index_of(folder_id, id, threaded).map_err(err)
}

#[tauri::command]
async fn next_unread(
    core: State<'_, Arc<Core>>,
    folder_id: i64,
    after: Option<i64>,
    threaded: bool,
    back: Option<bool>,
) -> Res<Option<NextUnread>> {
    core.next_unread(folder_id, after, threaded, back.unwrap_or(false)).map_err(err)
}

#[tauri::command]
async fn list_tags(core: State<'_, Arc<Core>>) -> Res<Vec<mail_core::tags::TagDef>> {
    core.tags().map_err(err)
}

/// Create (keyword omitted) or update a tag definition; returns its keyword.
#[tauri::command]
async fn set_tag(core: State<'_, Arc<Core>>, keyword: Option<String>, name: String, color: String) -> Res<String> {
    core.set_tag(keyword.as_deref(), &name, &color).map_err(err)
}

#[tauri::command]
async fn delete_tag(core: State<'_, Arc<Core>>, keyword: String) -> Res<()> {
    core.delete_tag(&keyword).map_err(err)
}

/// "0.1.44 (8ff8ea1)": the same for a commit on every machine (see build.rs).
fn version_label() -> String {
    match env!("APP_COMMIT") {
        "" => env!("APP_VERSION").to_string(),
        c => format!("{} ({c})", env!("APP_VERSION")),
    }
}

#[tauri::command]
fn app_version() -> String {
    version_label()
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
struct MessagePayload {
    message: String,
}

#[derive(Clone, Serialize)]
struct IdPayload {
    id: i64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SendPayload {
    account_id: i64,
    id: i64,
    message: Option<String>,
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
                Ok(CoreEvent::ActionFailed { message }) => { let _ = app.emit("action-failed", MessagePayload { message }); }
                Ok(CoreEvent::Sync(status)) => { let _ = app.emit("sync-status", status); }
                Ok(CoreEvent::Sent { account_id, id }) => {
                    let _ = app.emit("message-sent", SendPayload { account_id, id, message: None });
                }
                Ok(CoreEvent::SendFailed { account_id, id, message }) => {
                    let _ = app.emit("send-failed", SendPayload { account_id, id, message: Some(message) });
                }
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
    let saved = window_state::load(app, window_state::MAIN);
    let requested = saved.as_ref().map(|s| (s.width, s.height)).unwrap_or((1360.0, 860.0));
    let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
        .title("Email")
        .inner_size(1360.0, 860.0)
        .min_inner_size(900.0, 560.0);
    if let Some(state) = &saved {
        builder = window_state::apply(app, builder, state, window_state::MAIN);
    }
    // macOS: transparent title bar with the traffic lights over the sidebar's top row
    // (like Mail/Notes). The UI reserves room for them and makes the pane tops draggable.
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true)
            .traffic_light_position(tauri::LogicalPosition::new(18.0, 20.0));
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

    window_state::track(&window, saved, requested, window_state::MAIN);

    // macOS: closing the window hides it; the app keeps syncing until ⌘Q, and the Dock icon
    // brings the window back (see the Reopen handler in `run`).
    #[cfg(target_os = "macos")]
    {
        let w = window.clone();
        window.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = w.hide();
            }
        });
    }
    Ok(())
}

/// macOS menu bar: the standard app/Edit/Window menus (Edit is what makes ⌘C/⌘V work in the
/// webview) plus a View menu whose items are forwarded to the UI as `menu-action` events.
#[cfg(target_os = "macos")]
fn macos_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};
    let app_menu = Submenu::with_items(
        app,
        "Email",
        true,
        &[
            &PredefinedMenuItem::about(app, Some("About Email"), Some(AboutMetadata::default()))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "settings", "Settings…", true, Some("CmdOrCtrl+,"))?,
            &MenuItem::with_id(app, "add-account", "Add Account…", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::show_all(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::quit(app, None)?,
        ],
    )?;
    let file = Submenu::with_items(
        app,
        "File",
        true,
        &[&MenuItem::with_id(app, "new-message", "New Message", true, None::<&str>)?],
    )?;
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;
    let view = Submenu::with_items(
        app,
        "View",
        true,
        &[
            &MenuItem::with_id(app, "search", "Search", true, None::<&str>)?,
            &MenuItem::with_id(app, "toggle-threaded", "Group by Conversation", true, Some("CmdOrCtrl+T"))?,
            &MenuItem::with_id(app, "toggle-scope", "Show All Mail in Conversations", true, Some("CmdOrCtrl+Shift+A"))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::fullscreen(app, None)?,
        ],
    )?;
    let help = Submenu::with_items(
        app,
        "Help",
        true,
        &[
            &MenuItem::with_id(app, "shortcuts", "Keyboard Shortcuts", true, Some("CmdOrCtrl+/"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "show-log", "Show Log File", true, None::<&str>)?,
            &MenuItem::with_id(app, "devtools", "Developer Tools", true, Some("Alt+CmdOrCtrl+I"))?,
        ],
    )?;
    let window = Submenu::with_items(
        app,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, Some("Zoom"))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;
    Menu::with_items(app, &[&app_menu, &file, &edit, &view, &window, &help])
}

#[cfg(target_os = "macos")]
#[derive(Clone, Serialize)]
struct MenuAction {
    id: String,
}

const LOG_FILE: &str = "email.log";
/// Above this, the log is renamed to email.1.log (replacing the previous one) at startup.
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Log to stderr and to `email.log` in the app's log folder (~/Library/Logs/<id> on macOS,
/// ~/.local/share/<id>/logs on Linux), so a release build launched from Finder still leaves
/// a record to look at. Falls back to stderr alone if the file can't be opened.
fn init_logging(dir: Option<std::path::PathBuf>) {
    use tracing_subscriber::prelude::*;
    // html5ever (HTML sanitising) warns about every malformed table in an email: not news.
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,html5ever=error".into());
    let file = dir.and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(LOG_FILE);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
            let _ = std::fs::rename(&path, dir.join("email.1.log"));
        }
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
    });
    let file_layer =
        file.map(|f| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f)));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(file_layer)
        .init();
}

#[tauri::command]
async fn send_message(core: State<'_, Arc<Core>>, request: SendRequest) -> Res<SendQueued> {
    core.send_message(request).await.map_err(err)
}

#[tauri::command]
async fn save_draft(core: State<'_, Arc<Core>>, id: Option<i64>, request: SendRequest) -> Res<i64> {
    core.save_draft(id, request).map_err(err)
}

#[tauri::command]
async fn open_draft(core: State<'_, Arc<Core>>, id: i64) -> Res<serde_json::Value> {
    core.open_draft(id).map_err(err)
}

#[tauri::command]
async fn delete_draft(core: State<'_, Arc<Core>>, id: i64) -> Res<()> {
    core.delete_draft(id).map_err(err)
}

/// Open draft `draft_id` in its own compose window (or bring that window forward). The page
/// finds its draft from the window label, `compose-<id>`.
#[tauri::command]
async fn open_compose(app: tauri::AppHandle, draft_id: i64) -> Res<()> {
    let label = format!("compose-{draft_id}");
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let opener = app.clone();
    let nav_opener = app.clone();
    // The size the last compose window was left at (see window_state; same GNOME care).
    let saved = window_state::load(&app, window_state::COMPOSE);
    let requested = saved.as_ref().map(|s| (s.width, s.height)).unwrap_or((760.0, 720.0));
    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::default())
        .title("New Message")
        .inner_size(requested.0, requested.1)
        .min_inner_size(520.0, 420.0);
    if let Some(state) = &saved {
        builder = window_state::apply(&app, builder, state, window_state::COMPOSE);
    }
    let window = builder
        // Links in a quoted message open in the browser, as in the main window.
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
        .build()
        .map_err(|e| e.to_string())?;
    window_state::track(&window, saved, requested, window_state::COMPOSE);
    Ok(())
}

#[tauri::command]
async fn suggest_addresses(core: State<'_, Arc<Core>>, query: String, limit: Option<usize>) -> Res<Vec<Addr>> {
    core.suggest_addresses(&query, limit.unwrap_or(8)).map_err(err)
}

#[tauri::command]
async fn account_details(core: State<'_, Arc<Core>>) -> Res<Vec<AccountDetails>> {
    core.account_details().map_err(err)
}

/// Reveal the app's data folder (mail.db, bodies.db) in the file manager.
#[tauri::command]
async fn show_data_dir(app: tauri::AppHandle) -> Res<()> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    app.opener().reveal_item_in_dir(dir.join("mail.db")).map_err(|e| e.to_string())
}

/// The web inspector for the calling window.
#[tauri::command]
async fn open_devtools(window: tauri::WebviewWindow) -> Res<()> {
    window.open_devtools();
    Ok(())
}

#[derive(Serialize)]
struct FileInfo {
    path: String,
    name: String,
    size: u64,
}

/// Name and size of files chosen to attach (picker or drop). Folders and missing files are left
/// out.
#[tauri::command]
async fn file_info(paths: Vec<String>) -> Res<Vec<FileInfo>> {
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
            let name = std::path::Path::new(&path).file_name()?.to_string_lossy().into_owned();
            Some(FileInfo { path, name, size: meta.len() })
        })
        .collect())
}

#[tauri::command]
async fn reply_context(core: State<'_, Arc<Core>>, id: i64, mode: ReplyMode) -> Res<ReplyContext> {
    core.reply_context(id, mode).await.map_err(err)
}

#[tauri::command]
async fn cancel_send(core: State<'_, Arc<Core>>, id: i64) -> Res<Option<serde_json::Value>> {
    core.cancel_send(id).map_err(err)
}

#[tauri::command]
async fn show_log(app: tauri::AppHandle) -> Res<()> {
    let path = app.path().app_log_dir().map_err(|e| e.to_string())?.join(LOG_FILE);
    app.opener().reveal_item_in_dir(&path).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {

    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder.menu(macos_menu).on_menu_event(|app, event| {
        let id = event.id().as_ref().to_string();
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.show();
            let _ = w.set_focus();
            if id == "devtools" {
                w.open_devtools();
                return;
            }
        }
        let _ = app.emit("menu-action", MenuAction { id });
    });
    builder
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
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            init_logging(app.path().app_log_dir().ok());
            tracing::info!(version = version_label(), "starting");
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
            app_version,
            apply_action,
            undo_action,
            index_of,
            next_unread,
            show_log,
            send_message,
            reply_context,
            save_draft,
            open_draft,
            delete_draft,
            open_compose,
            file_info,
            account_details,
            show_data_dir,
            open_devtools,
            suggest_addresses,
            cancel_send,
            list_tags,
            set_tag,
            delete_tag,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, _event| {
            // macOS: clicking the Dock icon with the window hidden brings it back.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { has_visible_windows: false, .. } = _event {
                if let Some(w) = _app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        });
}
