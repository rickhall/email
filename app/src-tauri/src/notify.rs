//! Desktop notifications for new mail in an Inbox, shown while the window isn't the one in use.
//! Clicking one brings the window up on the message (or, for several, on the Inbox).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use mail_core::Core;
use mail_core::model::{FolderRole, NewMessage};
use serde::Serialize;
use tauri::{Emitter, Manager};

/// The UI's setting (Settings › Notifications), pushed at startup and on every change.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Notifications whose click is still awaited, each on a thread of its own. Unattended ones
/// pile up (GNOME keeps them until dismissed), so past this many new ones aren't clickable.
static WAITING: AtomicUsize = AtomicUsize::new(0);
const MAX_WAITING: usize = 32;

#[tauri::command]
pub fn set_notifications(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenPayload {
    folder_id: i64,
    id: Option<i64>,
}

pub fn init() {
    // Without this, macOS attributes the notifications (and their clicks) to another app.
    #[cfg(target_os = "macos")]
    let _ = notify_rust::set_application("dev.mailclient.desktop");
}

pub fn new_mail(app: &tauri::AppHandle, core: &Core, account_id: i64, folder_id: i64, messages: Vec<NewMessage>) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let is_inbox = core
        .folders(account_id)
        .ok()
        .and_then(|fs| fs.into_iter().find(|f| f.id == folder_id))
        .is_some_and(|f| f.role == Some(FolderRole::Inbox));
    if !is_inbox || in_use(app) {
        return;
    }
    let accounts = core.accounts().unwrap_or_default();
    let Some(account) = accounts.iter().find(|a| a.id == account_id) else { return };
    // Mail you sent yourself (to yourself, or Cc'd) isn't news.
    let messages: Vec<NewMessage> =
        messages.into_iter().filter(|m| !m.from_addr.eq_ignore_ascii_case(&account.email)).collect();
    let Some(first) = messages.first() else { return };

    let (summary, mut body, target) = if messages.len() == 1 {
        let subject = if first.subject.trim().is_empty() { "(no subject)" } else { first.subject.trim() };
        (sender(first).to_string(), subject.to_string(), Some(first.id))
    } else {
        (format!("{} new messages", messages.len()), senders(&messages), None)
    };
    if accounts.len() > 1 {
        body = format!("{body}\n{}", account.email);
    }
    tracing::info!(account = account.email, count = messages.len(), "new mail notification");

    let app = app.clone();
    std::thread::spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.appname("Email").summary(&summary).body(&body);
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            n.icon("mail-app")
                .hint(notify_rust::Hint::DesktopEntry("Email".into()))
                .hint(notify_rust::Hint::Category("email.arrived".into()));
        }
        let clickable = WAITING.fetch_add(1, Ordering::SeqCst) < MAX_WAITING;
        if clickable {
            n.action("default", "Open");
        }
        match n.show() {
            Ok(handle) if clickable => handle.wait_for_action(|action| {
                if action == "default" {
                    open(&app, folder_id, target);
                }
            }),
            Ok(_) => {}
            Err(e) => tracing::warn!("showing a notification: {e}"),
        }
        WAITING.fetch_sub(1, Ordering::SeqCst);
    });
}

/// The window is up and in use: the new mail shows in the list, no need to interrupt.
fn in_use(app: &tauri::AppHandle) -> bool {
    app.get_webview_window("main").is_some_and(|w| {
        w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false) && w.is_focused().unwrap_or(false)
    })
}

fn sender(m: &NewMessage) -> &str {
    let name = m.from_name.trim();
    if name.is_empty() { m.from_addr.trim() } else { name }
}

/// "From Ann, Bob and 3 others": distinct senders in arrival order.
fn senders(messages: &[NewMessage]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for m in messages {
        let s = sender(m);
        if !names.contains(&s) {
            names.push(s);
        }
    }
    match names.as_slice() {
        [] => String::new(),
        [a] => format!("From {a}"),
        [a, b] => format!("From {a} and {b}"),
        [a, b, c] => format!("From {a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("From {a}, {b} and {} others", rest.len()),
    }
}

fn open(app: &tauri::AppHandle, folder_id: i64, id: Option<i64>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    let _ = app.emit("open-message", OpenPayload { folder_id, id });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(name: &str) -> NewMessage {
        NewMessage { id: 1, from_name: name.into(), from_addr: format!("{name}@example.com"), subject: String::new() }
    }

    #[test]
    fn sender_lists() {
        assert_eq!(senders(&[msg("Ann"), msg("Ann")]), "From Ann");
        assert_eq!(senders(&[msg("Ann"), msg("Bob")]), "From Ann and Bob");
        assert_eq!(senders(&[msg("Ann"), msg("Bob"), msg("Cy")]), "From Ann, Bob and Cy");
        assert_eq!(senders(&[msg("Ann"), msg("Bob"), msg("Cy"), msg("Di")]), "From Ann, Bob and 2 others");
        assert_eq!(sender(&msg("")), "@example.com");
    }
}
