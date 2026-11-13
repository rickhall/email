//! Making Email the system's mail program: the one `mailto:` links (a browser's "Email link")
//! open. Settings › General shows whether it is, and makes it so.

use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultMail {
    /// It can be set from here on this system.
    pub supported: bool,
    pub is_default: bool,
}

pub fn status(app: &tauri::AppHandle) -> DefaultMail {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return DefaultMail { supported: true, is_default: imp::is_default(app) };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = app;
        DefaultMail { supported: false, is_default: false }
    }
}

pub fn make_default(app: &tauri::AppHandle) -> Result<(), String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return imp::make_default(app);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = app;
        Err("not supported on this system".into())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::PathBuf;
    use std::process::Command;
    use tauri_plugin_deep_link::DeepLinkExt;

    /// The desktop entry the packages install (named after the product).
    const DESKTOP: &str = "Email.desktop";
    const MIME: &str = "x-scheme-handler/mailto";

    fn query() -> Option<String> {
        let out = Command::new("xdg-mime").args(["query", "default", MIME]).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        s.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
    }

    /// The packages' desktop entry is installed (not so for an AppImage or a development build).
    fn installed() -> bool {
        let home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
        let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        home.into_iter()
            .chain(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from))
            .any(|d| d.join("applications").join(DESKTOP).is_file())
    }

    pub fn is_default(app: &tauri::AppHandle) -> bool {
        query().as_deref() == Some(DESKTOP) || app.deep_link().is_registered("mailto").unwrap_or(false)
    }

    pub fn make_default(app: &tauri::AppHandle) -> Result<(), String> {
        if installed() {
            let out = Command::new("xdg-mime").args(["default", DESKTOP, MIME]).output().map_err(|e| format!("xdg-mime: {e}"))?;
            if !out.status.success() {
                return Err(format!("xdg-mime failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            Ok(())
        } else {
            // Its own desktop entry for this executable (AppImage, development build).
            app.deep_link().register("mailto").map_err(|e| e.to_string())
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyDefaultHandlerForURLScheme(scheme: CFStringRef) -> CFStringRef;
        fn LSSetDefaultHandlerForURLScheme(scheme: CFStringRef, bundle_id: CFStringRef) -> i32;
    }

    pub fn is_default(app: &tauri::AppHandle) -> bool {
        let scheme = CFString::new("mailto");
        // SAFETY: a valid CFString in; the result, if any, is ours to release (create rule).
        let handler = unsafe { LSCopyDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef()) };
        if handler.is_null() {
            return false;
        }
        let handler = unsafe { CFString::wrap_under_create_rule(handler) };
        handler.to_string().eq_ignore_ascii_case(&app.config().identifier)
    }

    pub fn make_default(app: &tauri::AppHandle) -> Result<(), String> {
        let scheme = CFString::new("mailto");
        let id = CFString::new(&app.config().identifier);
        // SAFETY: valid CFStrings, borrowed for the call.
        let status = unsafe { LSSetDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef(), id.as_concrete_TypeRef()) };
        if status == 0 { Ok(()) } else { Err(format!("macOS didn't make Email the default (error {status})")) }
    }
}
