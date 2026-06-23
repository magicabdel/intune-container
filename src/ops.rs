//! High-level operations — the command surface shared by both front-ends.
//!
//! Each function performs one user-facing operation (enroll, edge, daemon,
//! stop, …) and returns structured results where a UI needs them. Interactive
//! concerns that differ between front-ends are pushed to the caller:
//!
//! * password entry for [`init`] is a parameter (CLI prompts on the tty; the GUI
//!   shows a dialog);
//! * [`destroy`] performs the teardown and returns what it removed — it never
//!   prompts (the caller confirms);
//! * [`status`] and [`doctor`](crate::doctor::collect) return data the caller
//!   renders.
//!
//! Progress is reported through `tracing`, so the CLI surfaces it on stderr and
//! the GUI can subscribe to it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::{backend, backup, compositor, config, display, lock};

// ===== Setup / lifecycle =====

/// Whether the container has been initialized (rootfs provisioned).
pub fn is_initialized() -> bool {
    config::Config::load()
        .map(|c| c.initialized)
        .unwrap_or(false)
}

/// Initialize the container: download the rootfs and provision it. The password
/// for the container user is supplied by the caller (the CLI prompts on the tty;
/// the GUI uses a dialog).
pub fn init(force: bool, image: Option<&str>, password: &str) -> Result<()> {
    debug!("Initializing intune container...");

    // Serialize against any concurrent lifecycle operation (e.g. a browser-
    // spawned native host trying to boot the container while we wipe the rootfs).
    let _lock = lock::LifecycleLock::acquire()?;

    let mut config =
        config::Config::load_or_create().context("Failed to load or create configuration")?;

    debug!(rootfs = %config.rootfs_path.display(), "Root filesystem path");

    // If force is set, stop + remove any existing rootfs first.
    if force {
        if backend::is_running(&config) {
            debug!("Stopping running container before re-init...");
            backend::stop(&config)?;
        }
        debug!("--force specified, removing existing rootfs...");
        backend::remove_rootfs(&config)?;
    }

    backend::initialize(&mut config, password, image)?;

    config.initialized = true;
    config.save()?;

    Ok(())
}

/// Same as [`ensure_running`] but holds the lifecycle lock for its duration so
/// concurrent invocations (notably the browser-spawned native host) cannot race
/// on stop/boot. The lock is released as soon as this returns, so it never
/// blocks while a GUI app or interactive shell is running.
fn locked_ensure_running(config: &mut config::Config, with_display: bool) -> Result<()> {
    let _lock = lock::LifecycleLock::acquire()?;
    ensure_running(config, with_display)
}

/// Ensure the container is initialized and running, attaching the host display
/// when a GUI flow needs it.
///
/// Callers must hold the lifecycle lock (use [`locked_ensure_running`]).
fn ensure_running(config: &mut config::Config, with_display: bool) -> Result<()> {
    if !config.initialized {
        anyhow::bail!("Not set up yet. Run:  intune-container enroll");
    }

    let display = display::DisplayInfo::detect();

    if backend::is_running(config) {
        if with_display && !config.display_forwarding {
            backend::attach_display(config, &display)?;
        }
        return Ok(());
    }

    debug!("Container not running — starting it...");
    boot(config, &display, with_display)?;
    Ok(())
}

/// Boot the container in the given display mode and persist that mode so other
/// commands know whether display forwarding is currently active.
fn boot(
    config: &mut config::Config,
    display: &display::DisplayInfo,
    with_display: bool,
) -> Result<()> {
    backend::start(config, display, with_display)?;

    if config.display_forwarding != with_display {
        config.display_forwarding = with_display;
        config.save()?;
    }

    Ok(())
}

/// Detach the host display from the running container once GUI usage is done,
/// returning it to headless isolation. Skipped when another known GUI app
/// (Edge or the portal) is still open.
fn auto_detach_after_gui(config: &mut config::Config) -> Result<()> {
    if !config.display_forwarding {
        return Ok(());
    }
    if backend::gui_app_running(config) {
        debug!("Another GUI app is still running — leaving the display attached");
        return Ok(());
    }
    let _lock = lock::LifecycleLock::acquire()?;
    backend::detach_display(config)
}

// ===== Enroll =====

/// Open the Intune portal so the user can sign in and enroll, then return the
/// container to headless isolation. Requires the container to be initialized
/// (call [`init`] first).
///
/// Returns `true` if the portal opened and was closed by the user.
pub fn enroll() -> Result<bool> {
    let mut config = config::Config::load().context("Failed to load configuration")?;
    if !config.initialized {
        anyhow::bail!("Not set up yet. Initialize first (init), then enroll.");
    }

    // Enrollment needs the interactive portal window — forward the display.
    locked_ensure_running(&mut config, true)?;

    info!("Opening Intune Portal — the window can take up to ~30s the first time.");

    // Gate on the container's services being up so the portal doesn't race
    // startup ("Failed to register: ... Message recipient disconnected").
    backend::wait_until_portal_ready(&config);

    let display_info = display::DisplayInfo::detect();
    backend::exec(&config, "intune-portal", None, &display_info)
        .context("Failed to launch portal")?;

    info!("Sign in and enroll in the window, then close it to finish...");
    let closed = backend::wait_for_app_exit(&config, "intune-portal");

    // GUI flow finished — return the container to headless isolation.
    if let Err(e) = auto_detach_after_gui(&mut config) {
        warn!("Failed to detach display after enrollment: {e:#}");
    }

    Ok(closed)
}

// ===== Edge =====

/// Launch Microsoft Edge inside the container (a GUI flow: forwards the real
/// display, restarting headless→display if needed). When `verbose` is false the
/// call blocks until Edge exits; otherwise it runs in the foreground.
pub fn edge(verbose: bool, args: &[String]) -> Result<()> {
    let mut config = config::Config::load().context("Failed to load configuration")?;

    locked_ensure_running(&mut config, true)?;

    let display_info = display::DisplayInfo::detect();

    let command = if args.is_empty() {
        "microsoft-edge-stable".to_string()
    } else {
        format!("microsoft-edge-stable {}", args.join(" "))
    };

    // Clean up a stale profile lock from a previous unclean exit. Safe — only
    // removes when no Edge is running.
    let prelaunch = Some(
        "if ! pgrep -x msedge >/dev/null 2>&1; then \
           rm -f \"$HOME/.config/microsoft-edge/Singleton\"* 2>/dev/null; \
         fi"
        .to_string(),
    );

    if verbose {
        info!(command = %command, "Launching Edge in foreground (verbose)");
        backend::exec_foreground(&config, &command, prelaunch.as_deref(), &display_info)
            .context("Application exited with error")?;
    } else {
        info!(command = %command, "Launching Edge in container");
        backend::exec(&config, &command, prelaunch.as_deref(), &display_info)
            .context("Failed to launch Edge in container")?;
        info!("Edge is open. The host display stays attached until you close it.");
        backend::wait_for_app_exit(&config, "msedge");
    }

    // Edge has exited — return the container to headless isolation (no restart).
    if let Err(e) = auto_detach_after_gui(&mut config) {
        warn!("Failed to detach display after Edge exit: {e:#}");
    }

    Ok(())
}

// ===== Stop / shell / detach =====

/// Stop the container.
pub fn stop() -> Result<()> {
    let config = config::Config::load().context("Failed to load configuration")?;
    let _lock = lock::LifecycleLock::acquire()?;

    if !backend::is_running(&config) {
        info!("Container is not running");
        return Ok(());
    }

    backend::stop(&config).context("Failed to stop container")?;
    info!("Container stopped");
    Ok(())
}

/// Open an interactive shell inside the container (terminal-only).
pub fn shell() -> Result<()> {
    let mut config = config::Config::load().context("Failed to load configuration")?;
    locked_ensure_running(&mut config, false)?;
    backend::shell(&config).context("Failed to open shell in container")
}

/// Detach the host display from the running container (back to headless).
pub fn detach_display() -> Result<()> {
    let mut config = config::Config::load().context("Failed to load configuration")?;
    let _lock = lock::LifecycleLock::acquire()?;
    backend::detach_display(&mut config)
}

// ===== Browser SSO (daemon) =====

/// What [`daemon`] installed, so the caller can report it.
#[derive(Debug, Clone, Serialize)]
pub struct DaemonReport {
    /// Paths of the native-messaging manifests that were written.
    pub manifests: Vec<String>,
}

/// Set up seamless browser SSO: expose the broker bus (headless), then install
/// the native-messaging host wrapper and manifests for the detected browsers.
pub fn daemon() -> Result<DaemonReport> {
    let mut config =
        config::Config::load().context("Not set up yet. Run:  intune-container enroll")?;

    let mut needs_restart = false;

    // Broker bus must be exposed for the native host to reach it.
    if !config.expose_bus {
        config.expose_bus = true;
        needs_restart = true;
    }
    config.save()?;

    // Background SSO runs HEADLESS. Force headless: restart if the bus was just
    // enabled OR the container is currently display-forwarding.
    {
        let _lock = lock::LifecycleLock::acquire()?;
        let display = display::DisplayInfo::detect();
        if backend::is_running(&config) {
            if needs_restart || config.display_forwarding {
                info!("Bringing SSO container up headless...");
                backend::stop(&config)?;
                boot(&mut config, &display, false)?;
            }
        } else {
            boot(&mut config, &display, false)?;
        }
    }

    let home = std::env::var("HOME").context("HOME not set")?;
    let self_exe = std::env::current_exe()
        .context("cannot determine own executable path")?
        .to_string_lossy()
        .to_string();

    // 1. Wrapper script: the manifest's `path` must be a bare executable, but the
    //    browser appends args. The wrapper discards those and runs `native-host`,
    //    sending the host's stderr to a log so SSO issues are diagnosable.
    let wrapper_dir = format!("{}/.local/lib/intune-container", home);
    std::fs::create_dir_all(&wrapper_dir)?;
    let wrapper_path = format!("{}/sso-native-host", wrapper_dir);
    let data_home =
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
    let log_path = format!("{data_home}/intune-container/native-host.log");
    if let Some(parent) = std::path::Path::new(&log_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let wrapper = format!("#!/bin/sh\nexec \"{self_exe}\" native-host 2>>\"{log_path}\"\n");
    std::fs::write(&wrapper_path, wrapper)?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&wrapper_path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper_path, perms)?;

    // 2. Native messaging manifests (Firefox-family + Chromium-family).
    let firefox_manifest = format!(
        r#"{{
    "name": "linux_entra_sso",
    "description": "Entra ID SSO via Microsoft Identity Broker (intune-container)",
    "path": "{path}",
    "type": "stdio",
    "allowed_extensions": ["linux-entra-sso@example.com", "@linux-entra-sso.tb"]
}}
"#,
        path = wrapper_path
    );

    let chrome_manifest = format!(
        r#"{{
    "name": "linux_entra_sso",
    "description": "Entra ID SSO via Microsoft Identity Broker (intune-container)",
    "path": "{path}",
    "type": "stdio",
    "allowed_origins": ["chrome-extension://jlnfnnolkbjieggibinobhkjdfbpcohn/"]
}}
"#,
        path = wrapper_path
    );

    let firefox_targets = [
        (
            "firefox",
            format!("{}/.mozilla/native-messaging-hosts", home),
        ),
        (
            "librewolf",
            format!("{}/.librewolf/native-messaging-hosts", home),
        ),
        (
            "thunderbird",
            format!("{}/.thunderbird/native-messaging-hosts", home),
        ),
    ];
    let chromium_targets = [
        (
            "chrome",
            format!("{}/.config/google-chrome/NativeMessagingHosts", home),
        ),
        (
            "chromium",
            format!("{}/.config/chromium/NativeMessagingHosts", home),
        ),
        (
            "brave",
            format!(
                "{}/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts",
                home
            ),
        ),
        (
            "vivaldi",
            format!("{}/.config/vivaldi/NativeMessagingHosts", home),
        ),
    ];

    let mut installed = Vec::new();
    for (name, dir) in &firefox_targets {
        let parent = std::path::Path::new(dir).parent();
        let present = parent.map(|p| p.exists()).unwrap_or(false);
        if present || *name == "firefox" {
            std::fs::create_dir_all(dir)?;
            let manifest_path = format!("{}/linux_entra_sso.json", dir);
            std::fs::write(&manifest_path, &firefox_manifest)?;
            installed.push(manifest_path);
        }
    }
    for (_name, dir) in &chromium_targets {
        let parent = std::path::Path::new(dir).parent();
        let present = parent.map(|p| p.exists()).unwrap_or(false);
        if present {
            std::fs::create_dir_all(dir)?;
            let manifest_path = format!("{}/linux_entra_sso.json", dir);
            std::fs::write(&manifest_path, &chrome_manifest)?;
            installed.push(manifest_path);
        }
    }

    Ok(DaemonReport {
        manifests: installed,
    })
}

/// Native messaging host entry point. Spawned by the browser; bridges the
/// linux-entra-sso extension to the container's broker over D-Bus. Stdout is the
/// protocol channel — callers must not print anything else to it.
pub fn native_host() -> Result<()> {
    let mut config = config::Config::load().context("Failed to load configuration")?;
    if config.expose_bus {
        let _ = locked_ensure_running(&mut config, false);
    }
    backend::native_host(&config)
}

/// Debug browser SSO by querying the broker directly and printing responses.
pub fn sso_test() -> Result<()> {
    let mut config =
        config::Config::load().context("Not set up yet. Run:  intune-container enroll")?;
    if !config.expose_bus {
        anyhow::bail!("Bus not exposed. Run:  intune-container daemon   (then retry sso-test)");
    }
    locked_ensure_running(&mut config, false)?;
    backend::sso_test(&config)
}

// ===== Backup / restore =====

/// Back up the enrollment state, stopping/restarting the container as needed for
/// a consistent archive. Returns the path of the created backup.
pub fn backup(output: Option<&Path>) -> Result<PathBuf> {
    let mut config = config::Config::load().context("Failed to load configuration")?;

    let was_running = backend::is_running(&config);
    if was_running {
        debug!("Stopping container for consistent backup...");
        backend::stop(&config)?;
    }

    let result = backend::backup(output);

    // Always restart if it was running — even if the backup failed.
    if was_running {
        debug!("Restarting container...");
        let display_info = display::DisplayInfo::detect();
        let mode = config.display_forwarding;
        if let Err(e) = boot(&mut config, &display_info, mode) {
            warn!("Failed to restart container after backup: {e:#}");
        }
    }

    result
}

/// Restore enrollment state from a backup.
pub fn restore(input: Option<&Path>) -> Result<()> {
    let config = config::Config::load().context("Failed to load configuration")?;

    if backend::is_running(&config) {
        debug!("Stopping container before restore...");
        backend::stop(&config)?;
    }

    backend::restore(input)
}

/// Inspect a backup archive (prints contents). CLI-oriented.
pub fn backup_inspect(input: Option<&Path>) -> Result<()> {
    backup::inspect(input)
}

// ===== Status =====

/// A snapshot of container + host display state. Never fails to produce: when
/// no configuration exists, `configured` is false and the rest are defaults.
#[derive(Debug, Clone, Serialize, Default)]
pub struct StatusReport {
    pub configured: bool,
    pub initialized: bool,
    pub running: bool,
    pub display_forwarding: bool,
    pub expose_bus: bool,
    pub machine_name: String,
    pub rootfs_path: String,
    pub host_user: String,
    pub host_uid: u32,
    pub compositor: String,
    pub wayland: Option<String>,
    pub x11_display: Option<String>,
    pub has_abstract_x11: bool,
    pub xauthority: Option<String>,
}

/// Gather the current status (read-only; no privilege escalation).
pub fn status() -> StatusReport {
    let display_info = display::DisplayInfo::detect();
    let compositor = format!("{:?}", compositor::detect_compositor());

    let mut report = StatusReport {
        machine_name: "intune".to_string(),
        compositor,
        wayland: display_info
            .wayland_socket
            .as_ref()
            .map(|p| p.display().to_string()),
        x11_display: display_info.x11_display.clone(),
        has_abstract_x11: display_info.has_abstract_x11,
        xauthority: display_info
            .xauthority
            .as_ref()
            .map(|p| p.display().to_string()),
        ..Default::default()
    };

    if let Ok(config) = config::Config::load() {
        report.configured = true;
        report.initialized = config.initialized;
        report.running = backend::is_running(&config);
        report.display_forwarding = config.display_forwarding;
        report.expose_bus = config.expose_bus;
        report.machine_name = config.machine_name.clone();
        report.rootfs_path = config.rootfs_path.display().to_string();
        report.host_user = config.host_user.clone();
        report.host_uid = config.host_uid;
    }

    report
}

// ===== Destroy =====

/// What [`destroy`] removed (so the caller can report it).
#[derive(Debug, Clone, Serialize)]
pub struct DestroyOutcome {
    pub rootfs: String,
    pub config_path: String,
    pub intune_home: String,
    pub device_state: String,
    pub purged: bool,
}

/// Destroy the container rootfs and per-user state. Does **not** prompt — the
/// caller is responsible for any confirmation. When `purge` is set, also removes
/// the persistent enrollment store (`~/.local/share/intune-container/persist`)
/// and any legacy `~/Intune` data.
pub fn destroy(purge: bool) -> Result<DestroyOutcome> {
    let config = config::Config::load().context("Failed to load configuration")?;
    let _lock = lock::LifecycleLock::acquire()?;

    if backend::is_running(&config) {
        debug!("Stopping running container...");
        backend::stop(&config)?;
    }

    let config_path = config::Config::config_path()?;
    let home = std::env::var("HOME").unwrap_or_default();
    let intune_home = format!("{}/Intune", home);
    let data_home =
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
    let persist_dir = format!("{data_home}/intune-container/persist");

    // 1. Remove the rootfs (via a user namespace; some files are subuid-owned).
    debug!("Removing rootfs: {}", config.rootfs_path.display());
    backend::remove_rootfs(&config)?;

    // 2. Remove config file and its parent directory.
    if config_path.exists() {
        debug!("Removing config: {}", config_path.display());
        std::fs::remove_file(&config_path)?;
        if let Some(parent) = config_path.parent() {
            let _ = std::fs::remove_dir(parent); // only succeeds if empty
        }
    }

    // 3. Remove the persistent enrollment store (+ any legacy ~/Intune) if --purge.
    if purge {
        backend::purge()?;
        let intune_path = std::path::Path::new(&intune_home);
        if intune_path.exists() {
            debug!("Removing legacy user data: {}", intune_home);
            let _ = std::fs::remove_dir_all(intune_path);
        }
    }

    // 4. Browser SSO manifests + wrapper.
    remove_browser_sso_integration(&home);

    Ok(DestroyOutcome {
        rootfs: config.rootfs_path.display().to_string(),
        config_path: config_path.display().to_string(),
        intune_home,
        device_state: persist_dir,
        purged: purge,
    })
}

/// Remove the browser SSO native-messaging manifests and the native-host wrapper
/// that [`daemon`] installs. Best-effort.
///
/// NOTE: keep this directory list in sync with [`daemon`]'s install targets.
fn remove_browser_sso_integration(home: &str) {
    let manifest_dirs = [
        format!("{home}/.mozilla/native-messaging-hosts"),
        format!("{home}/.librewolf/native-messaging-hosts"),
        format!("{home}/.thunderbird/native-messaging-hosts"),
        format!("{home}/.config/google-chrome/NativeMessagingHosts"),
        format!("{home}/.config/chromium/NativeMessagingHosts"),
        format!("{home}/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts"),
        format!("{home}/.config/vivaldi/NativeMessagingHosts"),
    ];
    for dir in &manifest_dirs {
        let _ = std::fs::remove_file(format!("{dir}/linux_entra_sso.json"));
    }
    let _ = std::fs::remove_dir_all(format!("{home}/.local/lib/intune-container"));
}
