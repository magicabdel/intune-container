//! The Tauri desktop front-end (the default interface), built into the single
//! `intune-container` binary.
//!
//! Every operation is a **direct, in-process call** into the library `ops`
//! module — the GUI never shells out. Because the GUI has no controlling
//! terminal, it runs the library in *GUI mode* ([`set_gui_mode`]), which routes
//! `sudo` password prompts through a graphical `$SUDO_ASKPASS` helper.
//!
//! Long-running operations (enroll, edge, …) run on Tauri's blocking pool so the
//! window stays responsive. The app is tray-resident: closing the window hides
//! it to the tray (when a tray is available); only the tray's "Quit" exits.

use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use base64::Engine;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent, Wry,
};

use intune_container::doctor::Check;
use intune_container::ops::{self, DestroyOutcome, StartReport, StatusReport};

/// Whether a system-tray icon was successfully created. When false (e.g. no
/// libappindicator on the host), closing the window exits the app instead of
/// hiding it — otherwise the app would become unreachable.
static TRAY_AVAILABLE: AtomicBool = AtomicBool::new(false);

// ===== Tauri commands (thin wrappers over `ops`) =====

/// Run a blocking library call on Tauri's blocking pool, mapping errors to
/// strings the frontend can display.
async fn run_blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("background task failed: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

/// Run a lifecycle operation. The rootless backend needs no privilege
/// escalation, so this is just a passthrough kept for call-site clarity.
fn privileged<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    f()
}

#[tauri::command]
async fn get_status() -> StatusReport {
    tauri::async_runtime::spawn_blocking(ops::status)
        .await
        .unwrap_or_default()
}

#[tauri::command]
async fn get_doctor() -> Vec<Check> {
    tauri::async_runtime::spawn_blocking(intune_container::doctor::collect)
        .await
        .unwrap_or_default()
}

#[tauri::command]
async fn get_account() -> Option<intune_container::native_host::AccountInfo> {
    tauri::async_runtime::spawn_blocking(|| ops::account().ok().flatten())
        .await
        .unwrap_or(None)
}

#[tauri::command]
fn is_initialized() -> bool {
    ops::is_initialized()
}

#[tauri::command]
async fn init(password: String) -> Result<(), String> {
    run_blocking(move || privileged(|| ops::init(false, None, &password))).await
}

#[tauri::command]
async fn enroll() -> Result<bool, String> {
    run_blocking(|| privileged(ops::enroll)).await
}

#[tauri::command]
async fn edge() -> Result<(), String> {
    run_blocking(|| privileged(|| ops::edge(false, &[]))).await
}

#[tauri::command]
async fn start() -> Result<StartReport, String> {
    run_blocking(|| privileged(ops::start)).await
}

#[tauri::command]
async fn stop() -> Result<(), String> {
    run_blocking(|| privileged(ops::stop)).await
}

#[tauri::command]
async fn detach_display() -> Result<(), String> {
    run_blocking(|| privileged(ops::detach_display)).await
}

#[tauri::command]
async fn backup(path: Option<String>) -> Result<String, String> {
    run_blocking(move || {
        privileged(|| {
            ops::backup(path.as_deref().map(std::path::Path::new)).map(|p| p.display().to_string())
        })
    })
    .await
}

#[tauri::command]
async fn restore(path: Option<String>) -> Result<(), String> {
    run_blocking(move || privileged(|| ops::restore(path.as_deref().map(std::path::Path::new))))
        .await
}

/// The default backup path, used to seed the save/open dialogs.
#[tauri::command]
fn default_backup_path() -> Result<String, String> {
    intune_container::backup::default_backup_path()
        .map(|p| p.display().to_string())
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn destroy(purge: bool) -> Result<DestroyOutcome, String> {
    run_blocking(move || privileged(|| ops::destroy(purge))).await
}

/// ===== Interactive shell (in-app PTY terminal) =====
///
/// A `/bin/bash` runs on a PTY inside the container; we stream its output to the
/// GUI as `shell://data` events (base64) and accept keystrokes via `shell_input`.
/// At most one session exists at a time, identified by `id` so a stale reader
/// never clears a newer session's state.
struct ShellHandle {
    id: u64,
    master: RawFd,
    pid: i32,
}

#[derive(Default)]
struct ShellState(Mutex<Option<ShellHandle>>);

static SHELL_SEQ: AtomicU64 = AtomicU64::new(0);
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Open (or replace) the interactive shell, sized to `rows`x`cols`.
#[tauri::command]
async fn shell_open(
    app: AppHandle,
    state: State<'_, ShellState>,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    let pty = run_blocking(move || {
        let target = ops::shell_session()?;
        intune_container::runtime::open_shell_pty(
            target.leader,
            Some(target.uid),
            &target.env,
            rows,
            cols,
        )
    })
    .await?;

    let id = SHELL_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut g = state.0.lock().unwrap();
        // Replace any existing session: killing the old shell makes its reader
        // hit EOF and close its own master fd, so we never close it cross-thread.
        if let Some(old) = g.as_ref() {
            unsafe { nix::libc::kill(old.pid, nix::libc::SIGKILL) };
        }
        *g = Some(ShellHandle {
            id,
            master: pty.master,
            pid: pty.pid,
        });
    }

    let master = pty.master;
    let app2 = app.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { nix::libc::read(master, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                break;
            }
            let payload = B64.encode(&buf[..n as usize]);
            if app2.emit("shell://data", payload).is_err() {
                break;
            }
        }
        // The shell ended (or was killed). Clear our slot if it's still ours, and
        // close the master under the lock so input/resize never race the close.
        let st = app2.state::<ShellState>();
        let mut g = st.0.lock().unwrap();
        if matches!(g.as_ref(), Some(h) if h.id == id) {
            *g = None;
        }
        unsafe { nix::libc::close(master) };
        drop(g);
        let _ = app2.emit("shell://exit", ());
    });
    Ok(())
}

/// Send keystrokes (base64-encoded UTF-8) to the shell.
#[tauri::command]
fn shell_input(state: State<'_, ShellState>, data: String) -> Result<(), String> {
    let bytes = B64.decode(data).map_err(|e| e.to_string())?;
    let g = state.0.lock().unwrap();
    if let Some(h) = g.as_ref() {
        let mut off = 0;
        while off < bytes.len() {
            let n = unsafe {
                nix::libc::write(
                    h.master,
                    bytes[off..].as_ptr() as *const _,
                    bytes.len() - off,
                )
            };
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
    }
    Ok(())
}

/// Tell the shell its terminal was resized.
#[tauri::command]
fn shell_resize(state: State<'_, ShellState>, rows: u16, cols: u16) -> Result<(), String> {
    let g = state.0.lock().unwrap();
    if let Some(h) = g.as_ref() {
        intune_container::runtime::pty_resize(h.master, rows, cols);
    }
    Ok(())
}

/// End the shell session (the reader closes the fd on the resulting EOF).
#[tauri::command]
fn shell_close(state: State<'_, ShellState>) -> Result<(), String> {
    let g = state.0.lock().unwrap();
    if let Some(h) = g.as_ref() {
        unsafe { nix::libc::kill(h.pid, nix::libc::SIGKILL) };
    }
    Ok(())
}

// ===== Logs =====

/// Path of the detached GUI log file.
fn data_dir() -> Option<PathBuf> {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
}

fn log_path() -> Option<PathBuf> {
    Some(data_dir()?.join("intune-container").join("gui.log"))
}

/// Strip ANSI escape sequences (CSI `ESC [ … letter`) at the byte level so
/// UTF-8 content survives. Old logs written to a tty may still contain them.
fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b {
            i += 1;
            if i < input.len() && input[i] == b'[' {
                i += 1;
                while i < input.len() && !input[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if i < input.len() {
                    i += 1; // consume the final letter
                }
            }
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

/// Return the last `max_lines` lines of the GUI log (ANSI-stripped). Empty when
/// the log doesn't exist yet.
#[tauri::command]
fn read_log(max_lines: usize) -> Result<String, String> {
    let Some(path) = log_path() else {
        return Ok(String::new());
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e.to_string()),
    };
    // Only scan the tail of large files.
    const CAP: usize = 512 * 1024;
    let slice = if data.len() > CAP {
        &data[data.len() - CAP..]
    } else {
        &data[..]
    };
    let clean = strip_ansi(slice);
    let text = String::from_utf8_lossy(&clean);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines.max(1));
    Ok(lines[start..].join("\n"))
}

/// Truncate the GUI log.
#[tauri::command]
fn clear_log() -> Result<(), String> {
    if let Some(path) = log_path() {
        std::fs::write(&path, b"").map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ===== Window / tray =====

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    // Opening the full interface dismisses the quick overlay.
    if let Some(overlay) = app.get_webview_window("overlay") {
        let _ = overlay.hide();
    }
}

/// Show the full interface (invoked from the overlay's "Open interface" button).
#[tauri::command]
fn show_interface(app: AppHandle) {
    show_main_window(&app);
}

/// Toggle the tray overlay: a small frameless quick-panel anchored near the
/// click. Shown on a single tray click, hidden again on the next click or when
/// it loses focus.
fn toggle_overlay(app: &AppHandle, at: PhysicalPosition<f64>) {
    let Some(win) = app.get_webview_window("overlay") else {
        // No overlay window (e.g. it failed to build) — fall back to the full UI.
        show_main_window(app);
        return;
    };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
        return;
    }
    position_overlay(&win, at);
    let _ = win.show();
    let _ = win.set_focus();
}

/// Anchor the overlay near the tray click, clamped to the monitor (flips above
/// the cursor when there isn't room below, e.g. a bottom panel).
fn position_overlay(win: &WebviewWindow, at: PhysicalPosition<f64>) {
    let size = win.outer_size().unwrap_or(PhysicalSize::new(340, 470));
    let (w, h) = (size.width as f64, size.height as f64);
    let mut x = at.x - w / 2.0;
    let mut y = at.y + 12.0;
    if let Ok(Some(mon)) = win.current_monitor() {
        let mp = mon.position();
        let ms = mon.size();
        let (mx, my) = (mp.x as f64, mp.y as f64);
        let (mw, mh) = (ms.width as f64, ms.height as f64);
        x = x.clamp(mx + 8.0, mx + mw - w - 8.0);
        if y + h > my + mh - 8.0 {
            y = at.y - h - 12.0;
        }
        y = y.max(my + 8.0);
    }
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

/// The dynamic "Start/Stop container" tray item, kept so the watcher can retitle
/// it as the container's state changes.
static POWER_ITEM: OnceLock<MenuItem<Wry>> = OnceLock::new();

/// Fire a fire-and-forget background operation from a tray menu item.
fn fire(action: &'static str) {
    tauri::async_runtime::spawn(async move {
        let _ = tauri::async_runtime::spawn_blocking(move || match action {
            "portal" => privileged(|| ops::enroll().map(|_| ())),
            "edge" => privileged(|| ops::edge(false, &[])),
            "stop" => privileged(ops::stop),
            "start" => privileged(|| ops::start().map(|_| ())),
            _ => Ok(()),
        })
        .await;
    });
}

/// Start or stop the container depending on its current state (the single
/// "power" tray item toggles between the two).
fn fire_power() {
    tauri::async_runtime::spawn(async move {
        let _ = tauri::async_runtime::spawn_blocking(|| {
            if ops::status().running {
                privileged(ops::stop)
            } else {
                privileged(|| ops::start().map(|_| ()))
            }
        })
        .await;
    });
}

/// A 32px round tray glyph in the given color — the container's status light.
fn status_icon(rgb: (u8, u8, u8)) -> tauri::image::Image<'static> {
    const N: usize = 32;
    let mut px = vec![0u8; N * N * 4];
    let c = (N as f32 - 1.0) / 2.0;
    let radius = 13.0_f32;
    for y in 0..N {
        for x in 0..N {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d = (dx * dx + dy * dy).sqrt();
            // 1px soft edge for a clean circle.
            let alpha = if d <= radius - 1.0 {
                255.0
            } else if d >= radius {
                0.0
            } else {
                (radius - d) * 255.0
            };
            let i = (y * N + x) * 4;
            px[i] = rgb.0;
            px[i + 1] = rgb.1;
            px[i + 2] = rgb.2;
            px[i + 3] = alpha as u8;
        }
    }
    tauri::image::Image::new_owned(px, N as u32, N as u32)
}

/// Poll the container state and keep the tray in sync: retitle the power item
/// (Start vs Stop) and tint the tray icon (grey = stopped, teal = running
/// headless, amber = display attached). Only updates on change.
fn spawn_tray_watcher(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut prev: Option<(bool, (u8, u8, u8))> = None;
        loop {
            if let Ok(st) = tauri::async_runtime::spawn_blocking(ops::status).await {
                let running = st.running;
                let color = if !st.configured || !st.initialized || !running {
                    (0x5e, 0x6b, 0x7c) // faint — stopped / not set up
                } else if st.display_forwarding {
                    (0xe8, 0xa2, 0x3d) // breach — display attached
                } else {
                    (0x23, 0xc9, 0xb8) // seal — running headless
                };
                if prev != Some((running, color)) {
                    prev = Some((running, color));
                    let app2 = app.clone();
                    let _ = app.run_on_main_thread(move || {
                        if let Some(item) = POWER_ITEM.get() {
                            let _ = item.set_text(if running {
                                "Stop container"
                            } else {
                                "Start container"
                            });
                        }
                        if let Some(tray) = app2.tray_by_id("main") {
                            let _ = tray.set_icon(Some(status_icon(color)));
                        }
                    });
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
}

fn build_tray_inner(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Interface", true, None::<&str>)?;
    let portal = MenuItem::with_id(app, "portal", "Open Intune portal", true, None::<&str>)?;
    let edge = MenuItem::with_id(app, "edge", "Open Microsoft Edge", true, None::<&str>)?;
    // Title is updated to "Stop container" by the tray watcher when it's running.
    let power = MenuItem::with_id(app, "power", "Start container", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let _ = POWER_ITEM.set(power.clone());

    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &portal,
            &edge,
            &PredefinedMenuItem::separator(app)?,
            &power,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let icon = app
        .default_window_icon()
        .cloned()
        .expect("a default window icon is embedded at build time");

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("Intune Container")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main_window(app),
            "portal" => fire("portal"),
            "edge" => fire("edge"),
            "power" => fire_power(),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            let app = tray.app_handle();
            match event {
                // Single left-click → the quick overlay near the tray.
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    position,
                    ..
                } => toggle_overlay(app, position),
                // Double-click → the full interface.
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => show_main_window(app),
                _ => {}
            }
        })
        .build(app)?;

    Ok(())
}

/// Try to create the tray, returning whether it succeeded.
///
/// On Linux the tray backend `dlopen`s libappindicator and **panics** if it's
/// missing, so we isolate the attempt with `catch_unwind` (silencing the panic
/// hook for the duration) and fall back to a plain window when it fails.
fn build_tray(app: &AppHandle) -> bool {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_tray_inner(app)));
    std::panic::set_hook(prev_hook);

    match result {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::warn!("system tray unavailable: {e}");
            false
        }
        Err(_) => {
            tracing::warn!(
                "system tray unavailable: libappindicator not found. \
                 Install libayatana-appindicator3 to enable the tray. \
                 Running with a normal window (closing it will quit)."
            );
            false
        }
    }
}

/// Run the Tauri interface. Blocks until the app exits.
pub fn run() {
    tauri::Builder::default()
        // Must be registered FIRST. A second `intune-container` launch focuses the
        // existing window (showing it from the tray) instead of starting a
        // duplicate instance; the second process then exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(ShellState::default())
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_doctor,
            get_account,
            is_initialized,
            init,
            enroll,
            edge,
            stop,
            start,
            detach_display,
            backup,
            restore,
            destroy,
            shell_open,
            shell_input,
            shell_resize,
            shell_close,
            show_interface,
            read_log,
            clear_log,
            default_backup_path,
        ])
        .on_window_event(|window, event| match event {
            // With a tray, closing the main window hides it there (the overlay too)
            // instead of exiting; without a tray, closing the main window exits.
            WindowEvent::CloseRequested { api, .. } => {
                if window.label() == "overlay" || TRAY_AVAILABLE.load(Ordering::SeqCst) {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
            // The overlay is a transient popover: dismiss it when it loses focus.
            WindowEvent::Focused(false) if window.label() == "overlay" => {
                let _ = window.hide();
            }
            _ => {}
        })
        .setup(|app| {
            let tray_ok = build_tray(app.handle());
            TRAY_AVAILABLE.store(tray_ok, Ordering::SeqCst);
            if tray_ok {
                spawn_tray_watcher(app.handle().clone());
            }
            // A hidden, frameless quick-panel shown on a single tray click.
            let _ = WebviewWindowBuilder::new(
                app,
                "overlay",
                WebviewUrl::App("index.html#overlay".into()),
            )
            .title("Intune Container")
            .inner_size(340.0, 470.0)
            .decorations(false)
            .resizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .visible(false)
            .build();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Intune Container interface");
}
