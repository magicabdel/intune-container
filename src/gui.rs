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

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

use intune_container::doctor::Check;
use intune_container::ops::{self, DaemonReport, DestroyOutcome, StatusReport};

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
async fn daemon() -> Result<DaemonReport, String> {
    run_blocking(|| privileged(ops::daemon)).await
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

/// Open an interactive shell. A shell inherently needs a terminal, so we launch
/// the user's terminal emulator running `machinectl shell` directly (this is the
/// standard tool, not the intune-container CLI).
#[tauri::command]
fn open_shell() -> Result<(), String> {
    let status = ops::status();
    if !status.configured {
        return Err("not set up yet".into());
    }
    let target = format!("{}@{}", status.host_user, status.machine_name);
    open_terminal(&["machinectl", "shell", &target, "/bin/bash", "--login"])
        .map_err(|e| e.to_string())
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

// ===== Terminal launch (shell only) =====

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run a command inside the user's terminal emulator.
fn open_terminal(argv: &[&str]) -> anyhow::Result<()> {
    let joined = argv
        .iter()
        .copied()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ");

    let candidates: &[(&str, &[&str])] = &[
        ("foot", &["-e", "sh", "-c"]),
        ("kitty", &["sh", "-c"]),
        ("alacritty", &["-e", "sh", "-c"]),
        ("wezterm", &["start", "--", "sh", "-c"]),
        ("konsole", &["-e", "sh", "-c"]),
        ("gnome-terminal", &["--", "sh", "-c"]),
        ("xterm", &["-e", "sh", "-c"]),
        ("x-terminal-emulator", &["-e", "sh", "-c"]),
    ];

    for (term, prefix) in candidates {
        if which(term) {
            let mut cmd = Command::new(term);
            cmd.args(*prefix)
                .arg(&joined)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            detach(&mut cmd);
            if cmd.spawn().is_ok() {
                return Ok(());
            }
        }
    }
    anyhow::bail!("no terminal emulator found")
}

fn detach(cmd: &mut Command) {
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            extern "C" {
                fn setsid() -> i32;
            }
            // SAFETY: setsid takes no args; failure (already a leader) is fine.
            let _ = setsid();
            Ok(())
        });
    }
}

// ===== Window / tray =====

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Fire a fire-and-forget background operation from a tray menu item.
fn fire(action: &'static str) {
    tauri::async_runtime::spawn(async move {
        let _ = tauri::async_runtime::spawn_blocking(move || match action {
            "edge" => privileged(|| ops::edge(false, &[])),
            "stop" => privileged(ops::stop),
            _ => Ok(()),
        })
        .await;
    });
}

fn build_tray_inner(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Interface", true, None::<&str>)?;
    let edge = MenuItem::with_id(app, "edge", "Open Microsoft Edge", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", "Stop container", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &edge,
            &stop,
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
            "edge" => fire("edge"),
            "stop" => fire("stop"),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
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
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_doctor,
            is_initialized,
            init,
            enroll,
            edge,
            daemon,
            stop,
            detach_display,
            backup,
            restore,
            destroy,
            open_shell,
            read_log,
            clear_log,
            default_backup_path,
        ])
        .on_window_event(|window, event| {
            // With a tray, closing the window hides it there; without one,
            // closing exits (otherwise the app would be unreachable).
            if let WindowEvent::CloseRequested { api, .. } = event {
                if TRAY_AVAILABLE.load(Ordering::SeqCst) {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .setup(|app| {
            let tray_ok = build_tray(app.handle());
            TRAY_AVAILABLE.store(tray_ok, Ordering::SeqCst);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Intune Container interface");
}
