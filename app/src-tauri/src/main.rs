// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // WebKitGTK paces its frames by the vblank of the display the window was first shown on.
    // When that display goes off (a laptop's lid closed, an external monitor in use) each wait
    // times out and the window gets about one frame a second: selecting and scrolling the list
    // turn sluggish until a restart. A timer at the refresh rate instead doesn't depend on it.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_FORCE_VBLANK_TIMER").is_none() {
        // SAFETY: first thing in main, before any other thread exists.
        unsafe { std::env::set_var("WEBKIT_FORCE_VBLANK_TIMER", "1") };
    }
    mail_app_lib::run()
}
