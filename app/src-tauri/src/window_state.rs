//! Remember the main window's size, position and maximized state.
//!
//! Sizes are stored in *logical* units and applied through the window builder, before the
//! window exists. Storing physical pixels and resizing after creation (what
//! tauri-plugin-window-state does) breaks under GNOME fractional scaling: GTK3 renders at an
//! integer scale (2× for a 125% display) that the window doesn't know until it's mapped, so a
//! restored physical size is misread and the window comes back the wrong size.
//!
//! Position is restored only where the platform lets apps place windows (not Wayland), and
//! only if it still lies on a connected monitor.
//!
//! On Wayland, GTK draws the title bar and shadow itself and the reported "inner" size includes
//! them, while the builder's size excludes them, so a naive save/restore grows the window by
//! the frame on every launch. We measure the frame once (reported size right after creation
//! minus the size requested) and subtract it when saving.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{LogicalPosition, LogicalSize, Manager, Runtime, WebviewWindow, WebviewWindowBuilder, WindowEvent};

const FILE: &str = "window.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowState {
    pub width: f64,
    pub height: f64,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub maximized: bool,
    /// Client-side frame (title bar + shadow) included in the reported size, logical px.
    #[serde(default)]
    pub frame_w: f64,
    #[serde(default)]
    pub frame_h: f64,
}

/// Largest plausible client-side frame; bigger differences mean the compositor resized us.
const MAX_FRAME: f64 = 200.0;

fn path<R: Runtime, M: Manager<R>>(app: &M) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join(FILE))
}

/// Wayland compositors don't let clients choose or read window positions.
fn can_position() -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let forced_x11 = std::env::var("GDK_BACKEND").is_ok_and(|b| b.starts_with("x11"));
    !wayland || forced_x11
}

pub fn load<R: Runtime, M: Manager<R>>(app: &M) -> Option<WindowState> {
    let s: WindowState = serde_json::from_slice(&std::fs::read(path(app)?).ok()?).ok()?;
    (s.width >= 200.0 && s.height >= 200.0).then_some(s)
}

/// Apply a saved state to the builder (logical units, so GTK scales it correctly).
pub fn apply<'a, R: Runtime, M: Manager<R>>(
    app: &M,
    builder: WebviewWindowBuilder<'a, R, M>,
    state: &WindowState,
) -> WebviewWindowBuilder<'a, R, M> {
    let mut builder = builder.inner_size(state.width, state.height).maximized(state.maximized);
    if let (true, Some(x), Some(y)) = (can_position(), state.x, state.y) {
        let on_screen = app.app_handle().available_monitors().unwrap_or_default().iter().any(|m| {
            let scale = m.scale_factor();
            let pos: LogicalPosition<f64> = m.position().to_logical(scale);
            let size: LogicalSize<f64> = m.size().to_logical(scale);
            // At least the top-left 100×40 (title bar area) must be visible.
            x + 100.0 > pos.x && x < pos.x + size.width && y >= pos.y && y + 40.0 < pos.y + size.height
        });
        if on_screen {
            builder = builder.position(x, y);
        }
    }
    builder
}

fn snapshot<R: Runtime>(
    window: &WebviewWindow<R>,
    previous: Option<&WindowState>,
    frame: (f64, f64),
) -> Option<WindowState> {
    let scale = window.scale_factor().ok()?;
    let maximized = window.is_maximized().unwrap_or(false);
    if window.is_minimized().unwrap_or(false) {
        return None;
    }
    // While maximized keep the last normal size/position, so un-maximizing after a restart
    // returns to it rather than to a screen-sized window.
    if maximized {
        return previous.map(|p| WindowState { maximized: true, frame_w: frame.0, frame_h: frame.1, ..p.clone() });
    }
    let size: LogicalSize<f64> = window.inner_size().ok()?.to_logical(scale);
    let (x, y) = if can_position() {
        let p: LogicalPosition<f64> = window.outer_position().ok()?.to_logical(scale);
        (Some(p.x), Some(p.y))
    } else {
        (None, None)
    };
    Some(WindowState {
        width: (size.width - frame.0).round(),
        height: (size.height - frame.1).round(),
        x,
        y,
        maximized: false,
        frame_w: frame.0,
        frame_h: frame.1,
    })
}

fn save<R: Runtime>(window: &WebviewWindow<R>, state: &WindowState) {
    let Some(path) = path(window) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(state) {
        // Write-then-rename so a crash mid-write can't leave a truncated file.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Save the window's state 600 ms after the user stops moving/resizing it, and on close.
/// `requested` is the logical inner size the window was created with.
pub fn track<R: Runtime>(window: &WebviewWindow<R>, initial: Option<WindowState>, requested: (f64, f64)) {
    // Frame size: known from a previous run, or measured on the first settle after creation.
    let frame = Arc::new(std::sync::Mutex::new(
        initial.as_ref().filter(|s| s.frame_w > 0.0 || s.frame_h > 0.0).map(|s| (s.frame_w, s.frame_h)),
    ));
    let started_maximized = initial.as_ref().is_some_and(|s| s.maximized);
    let last = Arc::new(std::sync::Mutex::new(initial));
    let generation = Arc::new(AtomicU64::new(0));

    let settle = {
        let (w, last, frame) = (window.clone(), last.clone(), frame.clone());
        move || {
            let mut frame = frame.lock().unwrap();
            if frame.is_none() && !started_maximized && !w.is_maximized().unwrap_or(false) {
                if let (Ok(scale), Ok(inner)) = (w.scale_factor(), w.inner_size()) {
                    let size: LogicalSize<f64> = inner.to_logical(scale);
                    let (dw, dh) = (size.width - requested.0, size.height - requested.1);
                    *frame = Some(if (0.0..=MAX_FRAME).contains(&dw) && (0.0..=MAX_FRAME).contains(&dh) {
                        (dw.round(), dh.round())
                    } else {
                        (0.0, 0.0)
                    });
                }
            }
            let f = frame.unwrap_or((0.0, 0.0));
            let mut last = last.lock().unwrap();
            if let Some(state) = snapshot(&w, last.as_ref(), f) {
                save(&w, &state);
                *last = Some(state);
            }
        }
    };
    let settle = Arc::new(settle);

    window.on_window_event(move |event| match event {
        WindowEvent::Resized(_) | WindowEvent::Moved(_) => {
            let mine = generation.fetch_add(1, Ordering::SeqCst) + 1;
            let (generation, settle) = (generation.clone(), settle.clone());
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(600)).await;
                if generation.load(Ordering::SeqCst) == mine {
                    settle();
                }
            });
        }
        WindowEvent::CloseRequested { .. } => settle(),
        _ => {}
    });
}
