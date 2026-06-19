//! intune-container: CLI tool for managing Microsoft Intune in a systemd-nspawn container.
//!
//! This is a Rust replacement for the Go-based "intuneme" tool, with better
//! Wayland compositor compatibility.

mod backup;
mod compositor;
mod config;
mod container;
mod display;
mod doctor;
mod init;
mod lock;
mod native_host;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{debug, info, warn};

#[derive(Parser)]
#[command(
    name = "intune-container",
    about = "Manage Microsoft Intune in a systemd-nspawn container",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize the container (download rootfs, configure)
    Init {
        /// Force re-initialization even if rootfs already exists
        #[arg(short, long)]
        force: bool,

        /// OCI image to use (default: ghcr.io/frostyard/ubuntu-intune:latest)
        #[arg(long)]
        image: Option<String>,
    },
    /// First-time setup: download, start, and enroll your device (one command)
    Enroll {
        /// OCI image to use (default: ghcr.io/frostyard/ubuntu-intune:latest)
        #[arg(long)]
        image: Option<String>,
    },
    /// Set up seamless browser SSO (Teams/M365 in your host browser)
    Daemon,
    /// Open Microsoft Edge inside the container (forwards your display)
    Edge {
        /// Run in foreground with logs visible (don't background)
        #[arg(short = 'v', long)]
        verbose: bool,

        /// Extra arguments passed to Edge
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Stop the container
    Stop,
    /// Show container status and detected display info
    Status,
    /// Run health checks across the whole stack
    Doctor,
    /// Backup Intune enrollment state (preserves registration across rebuilds)
    Backup {
        /// Output file path (default: ~/.local/share/intune-container/enrollment-backup.tar.gz)
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
    },
    /// Inspect a backup archive (show contents without extracting)
    #[command(name = "backup-inspect")]
    BackupInspect {
        /// Backup file to inspect
        #[arg(short, long)]
        input: Option<std::path::PathBuf>,
    },
    /// Restore Intune enrollment state from a backup
    Restore {
        /// Input file path (default: ~/.local/share/intune-container/enrollment-backup.tar.gz)
        #[arg(short, long)]
        input: Option<std::path::PathBuf>,
    },

    // --- Hidden / advanced commands (not part of the everyday surface) ---
    /// Native messaging host for the linux-entra-sso browser extension (internal)
    #[command(name = "native-host", hide = true)]
    NativeHost,
    /// Debug browser SSO: query the broker directly and show raw responses
    #[command(name = "sso-test", hide = true)]
    SsoTest,
    /// Open a shell inside the container (advanced)
    #[command(hide = true)]
    Shell,
    /// Destroy the container rootfs and configuration (advanced)
    #[command(hide = true)]
    Destroy {
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,

        /// Also remove ~/Intune data and persistent device-state
        #[arg(long)]
        purge: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing based on verbosity. Logs go to STDERR — stdout is
    // reserved for the native-messaging protocol (`native-host`).
    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    match cli.command {
        Command::Init { force, image } => cmd_init(force, image)?,
        Command::Enroll { image } => cmd_enroll(image)?,
        Command::Daemon => cmd_daemon()?,
        Command::Edge { verbose, args } => cmd_edge(verbose, args)?,
        Command::Stop => cmd_stop()?,
        Command::Status => cmd_status()?,
        Command::Doctor => doctor::run()?,
        Command::Backup { output } => cmd_backup(output)?,
        Command::BackupInspect { input } => cmd_backup_inspect(input)?,
        Command::Restore { input } => cmd_restore(input)?,
        Command::NativeHost => cmd_native_host()?,
        Command::SsoTest => cmd_sso_test()?,
        Command::Shell => cmd_shell()?,
        Command::Destroy { force, purge } => cmd_destroy(force, purge)?,
    }

    Ok(())
}

fn cmd_init(force: bool, image: Option<String>) -> Result<()> {
    debug!("Initializing intune container...");

    // Serialize against any concurrent lifecycle operation (e.g. a browser-
    // spawned native host trying to boot the container while we wipe the rootfs).
    let _lock = lock::LifecycleLock::acquire()?;

    let config =
        config::Config::load_or_create().context("Failed to load or create configuration")?;

    debug!(machine = %config.machine_name, "Configuration ready");
    debug!(rootfs = %config.rootfs_path.display(), "Root filesystem path");

    // If force is set and rootfs already exists, remove it first
    if force && config.rootfs_path.exists() {
        // Stop the container first — removing a mounted/running rootfs corrupts it.
        if container::is_running(&config.machine_name) {
            debug!("Stopping running container before re-init...");
            container::stop(&config.machine_name)?;
        }
        debug!("--force specified, removing existing rootfs...");
        let status = std::process::Command::new("sudo")
            .args(["rm", "-rf", &config.rootfs_path.to_string_lossy()])
            .status()
            .context("Failed to remove existing rootfs")?;
        if !status.success() {
            anyhow::bail!("Failed to remove existing rootfs");
        }
    }

    // Prompt for container user password
    let password = rpassword::prompt_password("Enter password for container user: ")
        .context("Failed to read password")?;
    let password_confirm = rpassword::prompt_password("Confirm password: ")
        .context("Failed to read password confirmation")?;

    if password != password_confirm {
        anyhow::bail!("Passwords do not match");
    }

    init::initialize(&config, &password, image.as_deref())?;

    // Mark as initialized (user-readable flag; avoids stat'ing root-owned rootfs)
    let mut config = config;
    config.initialized = true;
    config.save()?;

    Ok(())
}

// ===== Convenience helpers for a "just works" experience =====

/// Same as [`ensure_running`] but holds the lifecycle lock for its duration so
/// concurrent invocations (notably the browser-spawned native host) cannot race
/// on stop/boot. The lock is released as soon as this returns, so it never
/// blocks while a GUI app or interactive shell is running.
fn locked_ensure_running(config: &mut config::Config, with_display: bool) -> Result<()> {
    let _lock = lock::LifecycleLock::acquire()?;
    ensure_running(config, with_display)
}

/// Ensure the container is initialized and running in the requested display
/// mode. Headless (`with_display = false`) is the default for everything except
/// the interactive GUI flows (`enroll`, `edge`), which forward the real display.
///
/// If the container is already running headless and a display is now needed, it
/// is restarted with forwarding. A running display session is never silently
/// downgraded (that would kill an open GUI app).
///
/// Callers must hold the lifecycle lock (use [`locked_ensure_running`]).
fn ensure_running(config: &mut config::Config, with_display: bool) -> Result<()> {
    if !config.initialized {
        anyhow::bail!("Not set up yet. Run:  intune-container enroll");
    }

    // Always make sure the host-side integration (passwordless nsenter helper +
    // sudoers rule) exists and is current BEFORE any exec — even when the
    // container is already running and we skip the boot path below. Otherwise an
    // upgraded binary would call a stale helper with the wrong argument ABI.
    ensure_host_integration(config)?;

    let display = display::DisplayInfo::detect();

    if container::is_running(&config.machine_name) {
        if with_display && !config.display_forwarding {
            info!("GUI requested — restarting container with display forwarding...");
            container::stop(&config.machine_name)?;
            boot(config, &display, true)?;
        }
        return Ok(());
    }

    debug!("Container not running — starting it...");
    boot(config, &display, with_display)?;
    Ok(())
}

/// Self-heal the host-side integration: ensure the passwordless nsenter sudoers
/// rule and helper script exist and are current. The helper is reinstalled when
/// missing or stale (e.g. after a binary upgrade that changed its argument ABI).
/// Reinstalling needs sudo, so we only do it when actually required.
fn ensure_host_integration(config: &config::Config) -> Result<()> {
    if !init::is_sudoers_installed(config) || !init::is_helper_current() {
        info!("Installing/updating nsenter helper...");
        init::install_sudoers_and_helper(config)?;
    }
    Ok(())
}

/// Boot the container in the given display mode and persist that mode so other
/// commands know whether display forwarding is currently active.
fn boot(
    config: &mut config::Config,
    display: &display::DisplayInfo,
    with_display: bool,
) -> Result<()> {
    container::start(config, display, with_display)?;

    ensure_host_integration(config)?;

    if config.display_forwarding != with_display {
        config.display_forwarding = with_display;
        config.save()?;
    }
    Ok(())
}

/// First-time setup in one command: init (if needed) → start → open portal.
fn cmd_enroll(image: Option<String>) -> Result<()> {
    // "Initialized" is tracked by a user-readable config flag — NOT a filesystem
    // stat, since the rootfs under /var/lib/machines is often unreadable (mode 700).
    let already = config::Config::load()
        .map(|c| c.initialized)
        .unwrap_or(false);

    if !already {
        eprintln!("Setting up the Intune container (one-time)...");
        cmd_init(false, image)?;
    } else {
        debug!("Already initialized — skipping download");
    }

    let mut config = config::Config::load().context("Failed to load configuration")?;
    // Enrollment needs the interactive portal window — forward the display.
    locked_ensure_running(&mut config, true)?;

    eprintln!();
    eprintln!("Opening Intune Portal — the window can take up to ~30s the first time.");
    eprintln!();

    // Gate on the container's services being up so the portal doesn't race
    // startup ("Failed to register: ... Message recipient disconnected").
    container::wait_until_portal_ready(&config.machine_name, &config.host_user);

    let display_info = display::DisplayInfo::detect();
    // Launch the portal DETACHED (background) so it isn't tied to this terminal,
    // then block until the user closes it before reporting success.
    container::exec(
        &config.machine_name,
        &config.host_user,
        config.host_uid,
        "intune-portal",
        None,
        &display_info,
    )
    .context("Failed to launch portal")?;

    eprintln!("Sign in and enroll in the window, then close it to finish...");
    if container::wait_for_app_exit(&config.machine_name, &config.host_user, "intune-portal") {
        eprintln!("✓ Done. Set up seamless browser SSO with:  intune-container daemon");
    } else {
        eprintln!(
            "⚠ The portal didn't open. Try again with:  intune-container enroll\n  \
             or check the stack with:  intune-container doctor"
        );
    }
    Ok(())
}

/// Native messaging host entry point. Spawned by the browser; bridges the
/// linux-entra-sso extension to the container's broker over D-Bus.
/// Stdout is the protocol channel — do not print anything else to it.
fn cmd_native_host() -> Result<()> {
    let mut config = config::Config::load().context("Failed to load configuration")?;
    // Ensure the container (and its exposed bus) is up so the broker is reachable.
    // Background SSO runs headless (no display forwarding).
    if config.expose_bus {
        let _ = locked_ensure_running(&mut config, false);
    }
    let bus_path = config.broker_bus_path()?;
    native_host::run_blocking(&bus_path)?;
    Ok(())
}

/// Debug browser SSO by querying the broker directly and printing responses.
fn cmd_sso_test() -> Result<()> {
    let mut config =
        config::Config::load().context("Not set up yet. Run:  intune-container enroll")?;

    if !config.expose_bus {
        anyhow::bail!("Bus not exposed. Run:  intune-container daemon   (then retry sso-test)");
    }
    locked_ensure_running(&mut config, false)?;

    let bus_path = config.broker_bus_path()?;
    native_host::test_blocking(&bus_path)?;
    Ok(())
}

/// Set up seamless browser SSO via the linux-entra-sso native messaging host.
/// Lightweight path: extension + our binary, no Python, no proxy daemon, no host
/// session bus — the host connects straight to the container's broker bus.
fn cmd_daemon() -> Result<()> {
    let mut config =
        config::Config::load().context("Not set up yet. Run:  intune-container enroll")?;

    let mut needs_restart = false;

    // Broker bus must be exposed for the native host to reach it.
    if !config.expose_bus {
        config.expose_bus = true;
        needs_restart = true;
    }
    config.save()?;

    // Background SSO runs HEADLESS (the broker uses the private in-container
    // Xvfb, not your real screen). Force headless here: restart if the bus was
    // just enabled OR the container is currently display-forwarding.
    //
    // Scoped lifecycle lock: held only across the stop/boot, then released
    // before the (local, fast) manifest writes below.
    {
        let _lock = lock::LifecycleLock::acquire()?;
        let display = display::DisplayInfo::detect();
        if container::is_running(&config.machine_name) {
            if needs_restart || config.display_forwarding {
                info!("Bringing SSO container up headless...");
                container::stop(&config.machine_name)?;
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

    // 1. Wrapper script: the native-messaging manifest's `path` must be a bare
    //    executable, but the browser appends args (manifest path, extension id).
    //    The wrapper discards those and runs our `native-host` subcommand.
    let wrapper_dir = format!("{}/.local/lib/intune-container", home);
    std::fs::create_dir_all(&wrapper_dir)?;
    let wrapper_path = format!("{}/sso-native-host", wrapper_dir);
    let wrapper = format!("#!/bin/sh\nexec \"{}\" native-host\n", self_exe);
    std::fs::write(&wrapper_path, wrapper)?;
    let mut perms = std::fs::metadata(&wrapper_path)?.permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper_path, perms)?;

    // 2. Native messaging manifests.
    //
    // Firefox-family browsers use the `allowed_extensions` schema and a
    // `native-messaging-hosts` dir; Chromium-family browsers use the
    // `allowed_origins` schema (with the extension's Web Store ID) and a
    // `NativeMessagingHosts` dir. We install both so SSO works regardless of
    // which browser the user picks.
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

    // The signed Chrome Web Store extension ID for linux-entra-sso.
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

    // (manifest, native-messaging dir). Firefox family first, then Chromium
    // family. `.mozilla` (Firefox) is always installed; the rest only when the
    // browser's config dir already exists.
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
        // Firefox is always installed; others only if their profile dir exists.
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
        // Only install where the browser's config dir already exists, so we don't
        // scatter manifests for browsers the user hasn't got.
        let parent = std::path::Path::new(dir).parent();
        let present = parent.map(|p| p.exists()).unwrap_or(false);
        if present {
            std::fs::create_dir_all(dir)?;
            let manifest_path = format!("{}/linux_entra_sso.json", dir);
            std::fs::write(&manifest_path, &chrome_manifest)?;
            installed.push(manifest_path);
        }
    }

    eprintln!("✓ Native messaging host installed.");
    for p in &installed {
        eprintln!("  {}", p);
    }
    eprintln!();
    eprintln!("Final step — install the browser extension (linux-entra-sso):");
    eprintln!("  Firefox / Thunderbird: download the signed .xpi from");
    eprintln!("    https://github.com/siemens/linux-entra-sso/releases/");
    eprintln!("  Chrome / Chromium / Brave: install from the Chrome Web Store");
    eprintln!("    https://chromewebstore.google.com/  (search 'linux-entra-sso')");
    eprintln!();
    eprintln!("Then open teams.microsoft.com — it signs in automatically via your");
    eprintln!("container's Intune enrollment. No Python, no proxy, no session-bus setup.");

    Ok(())
}

fn cmd_stop() -> Result<()> {
    let config = config::Config::load().context("Failed to load configuration")?;
    let _lock = lock::LifecycleLock::acquire()?;

    if !container::is_running(&config.machine_name) {
        info!(machine = %config.machine_name, "Container is not running");
        return Ok(());
    }

    container::stop(&config.machine_name).context("Failed to stop container")?;

    info!(machine = %config.machine_name, "Container stopped");
    Ok(())
}

fn cmd_shell() -> Result<()> {
    let mut config = config::Config::load().context("Failed to load configuration")?;

    locked_ensure_running(&mut config, false)?;

    container::shell(&config.machine_name, &config.host_user)
        .context("Failed to open shell in container")?;

    Ok(())
}

/// Launch Microsoft Edge inside the container. This is a GUI flow, so it
/// forwards the real display (restarting the container with forwarding if it is
/// currently running headless).
fn cmd_edge(verbose: bool, args: Vec<String>) -> Result<()> {
    let mut config = config::Config::load().context("Failed to load configuration")?;

    locked_ensure_running(&mut config, true)?;

    let display_info = display::DisplayInfo::detect();

    let command = if args.is_empty() {
        "microsoft-edge-stable".to_string()
    } else {
        // The user's own extra args are passed straight through to the in-
        // container shell. This is the caller's own input run inside their own
        // container (equivalent to `shell`), so no extra quoting is applied.
        format!("microsoft-edge-stable {}", args.join(" "))
    };

    // Clean up a stale profile lock from a previous unclean exit. Edge records
    // the hostname in SingletonLock; a container restart makes it think
    // "another computer" holds the profile. Safe — only removes when no Edge is
    // running. Runs as a separate statement before launch.
    let prelaunch = Some(
        "if ! pgrep -x msedge >/dev/null 2>&1; then \
           rm -f \"$HOME/.config/microsoft-edge/Singleton\"* 2>/dev/null; \
         fi"
        .to_string(),
    );

    if verbose {
        info!(command = %command, "Launching Edge in foreground (verbose)");
        container::exec_foreground(
            &config.machine_name,
            &config.host_user,
            config.host_uid,
            &command,
            prelaunch.as_deref(),
            &display_info,
        )
        .context("Application exited with error")?;
    } else {
        info!(command = %command, "Launching Edge in container");
        container::exec(
            &config.machine_name,
            &config.host_user,
            config.host_uid,
            &command,
            prelaunch.as_deref(),
            &display_info,
        )
        .context("Failed to launch Edge in container")?;
        debug!("Edge launched");
    }

    Ok(())
}

fn cmd_destroy(force: bool, purge: bool) -> Result<()> {
    let config = config::Config::load().context("Failed to load configuration")?;
    let _lock = lock::LifecycleLock::acquire()?;

    // Stop container if running
    if container::is_running(&config.machine_name) {
        debug!("Stopping running container...");
        container::stop(&config.machine_name)?;
    }

    // Show what will be deleted
    let config_path = config::Config::config_path()?;
    let home = std::env::var("HOME").unwrap_or_default();
    let intune_home = format!("{}/Intune", home);
    let device_state_dir = container::persistent_state_dir();

    eprintln!("This will permanently delete:");
    eprintln!("  • Container rootfs:  {}", config.rootfs_path.display());
    eprintln!("  • Configuration:     {}", config_path.display());
    if purge {
        eprintln!("  • User data:         {}", intune_home);
        eprintln!("  • Device state:      {}", device_state_dir);
    }
    eprintln!();

    // Confirm unless --force
    if !force {
        eprint!("Are you sure? [y/N] ");
        use std::io::Write;
        std::io::stderr().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let answer = input.trim().to_lowercase();

        if answer != "y" && answer != "yes" {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    // 1. Remove rootfs
    if config.rootfs_path.exists() {
        debug!("Removing rootfs: {}", config.rootfs_path.display());
        let status = std::process::Command::new("sudo")
            .args(["rm", "-rf", &config.rootfs_path.to_string_lossy()])
            .status()
            .context("Failed to remove rootfs")?;
        if !status.success() {
            anyhow::bail!("Failed to remove rootfs");
        }
    }

    // 2. Remove config file and its parent directory
    if config_path.exists() {
        debug!("Removing config: {}", config_path.display());
        std::fs::remove_file(&config_path)?;
        // Remove parent dir if empty
        if let Some(parent) = config_path.parent() {
            let _ = std::fs::remove_dir(parent); // only succeeds if empty
        }
    }

    // 3. Remove user data (~/Intune) and persistent device-state if --purge.
    //    Both hold enrollment material; without --purge they're kept so a later
    //    `init` can reuse the existing registration.
    if purge {
        let intune_path = std::path::Path::new(&intune_home);
        if intune_path.exists() {
            debug!("Removing user data: {}", intune_home);
            std::fs::remove_dir_all(intune_path).context("Failed to remove ~/Intune")?;
        }

        // Device-state lives under /var/lib (root-owned), outside the rootfs.
        if std::path::Path::new(device_state_dir).exists() {
            debug!("Removing device state: {}", device_state_dir);
            let status = std::process::Command::new("sudo")
                .args(["rm", "-rf", device_state_dir])
                .status()
                .context("Failed to remove device-state directory")?;
            if !status.success() {
                anyhow::bail!("Failed to remove {}", device_state_dir);
            }
        }
    }

    // 4. Clean up any machinectl registration
    let _ = std::process::Command::new("sudo")
        .args(["machinectl", "remove", &config.machine_name])
        .output();

    // 5. Remove host-side integration so teardown leaves nothing privileged
    //    behind: the passwordless sudoers rule + nsenter helper, and the browser
    //    SSO native-messaging manifests + wrapper.
    init::uninstall_sudoers_and_helper();
    remove_browser_sso_integration(&home);

    eprintln!();
    eprintln!("✓ Container destroyed.");
    eprintln!();
    if purge {
        eprintln!("Removed from host (nothing left):");
        eprintln!("  ✓ Rootfs");
        eprintln!("  ✓ Config");
        eprintln!("  ✓ Machine registration");
        eprintln!("  ✓ Sudoers rule + nsenter helper");
        eprintln!("  ✓ Browser SSO manifests");
        eprintln!("  ✓ User data (~/Intune)");
        eprintln!("  ✓ Device state ({})", device_state_dir);
    } else {
        eprintln!("Removed from host:");
        eprintln!("  ✓ Rootfs");
        eprintln!("  ✓ Config");
        eprintln!("  ✓ Machine registration");
        eprintln!("  ✓ Sudoers rule + nsenter helper");
        eprintln!("  ✓ Browser SSO manifests");
        eprintln!();
        eprintln!("Kept (use --purge to remove these too):");
        eprintln!("  • User data:    {}", intune_home);
        eprintln!("  • Device state: {}", device_state_dir);
    }

    Ok(())
}

/// Remove the browser SSO native-messaging manifests and the native-host wrapper
/// that `daemon` installs. Best-effort (missing files are ignored).
///
/// NOTE: keep this directory list in sync with `cmd_daemon`'s install targets.
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
    // The native-host wrapper script lives in its own dir; remove the whole dir.
    let _ = std::fs::remove_dir_all(format!("{home}/.local/lib/intune-container"));
}

fn cmd_status() -> Result<()> {
    let config = config::Config::load().context("Failed to load configuration")?;

    let running = container::is_running(&config.machine_name);
    let display_info = display::DisplayInfo::detect();
    let compositor = compositor::detect_compositor();

    println!("=== intune-container status ===");
    println!();
    println!("Machine:      {}", config.machine_name);
    println!("Rootfs:       {}", config.rootfs_path.display());
    println!("Running:      {}", if running { "yes" } else { "no" });
    println!(
        "Host user:    {} (uid {})",
        config.host_user, config.host_uid
    );
    println!(
        "Display fwd:  {}",
        if config.display_forwarding {
            "on (GUI session active)"
        } else {
            "off (headless — default, max isolation)"
        }
    );
    println!(
        "Bus exposed:  {}",
        if config.expose_bus {
            "yes (browser SSO)"
        } else {
            "no"
        }
    );
    println!();
    println!("=== Display Environment ===");
    println!();
    println!("Compositor:   {:?}", compositor);

    if let Some(ref ws) = display_info.wayland_socket {
        println!("Wayland:      {}", ws.display());
    } else {
        println!("Wayland:      not detected");
    }

    if let Some(ref x11) = display_info.x11_display {
        println!("X11 display:  {}", x11);
        println!("Abstract X11: {}", display_info.has_abstract_x11);
    } else {
        println!("X11:          not detected");
    }

    if let Some(ref xa) = display_info.xauthority {
        println!("Xauthority:   {}", xa.display());
    } else {
        println!("Xauthority:   not found");
    }

    Ok(())
}

fn cmd_backup(output: Option<std::path::PathBuf>) -> Result<()> {
    let mut config = config::Config::load().context("Failed to load configuration")?;

    // Stop container if running (consistent backup)
    let was_running = container::is_running(&config.machine_name);
    if was_running {
        debug!("Stopping container for consistent backup...");
        container::stop(&config.machine_name)?;
    }

    let result = backup::backup(&config, output.as_deref());

    // Always restart if it was running — even if the backup failed — so we never
    // leave the user's container stopped behind their back.
    if was_running {
        debug!("Restarting container...");
        let display_info = display::DisplayInfo::detect();
        let mode = config.display_forwarding;
        if let Err(e) = boot(&mut config, &display_info, mode) {
            warn!("Failed to restart container after backup: {e:#}");
        }
    }

    let path = result?;
    eprintln!("\u{2713} Enrollment backed up to: {}", path.display());
    eprintln!();
    eprintln!("You can now safely destroy and rebuild the container:");
    eprintln!("  intune-container destroy");
    eprintln!("  intune-container init");
    eprintln!("  intune-container restore");
    eprintln!("  intune-container enroll");

    Ok(())
}

fn cmd_restore(input: Option<std::path::PathBuf>) -> Result<()> {
    let config = config::Config::load().context("Failed to load configuration")?;

    if container::is_running(&config.machine_name) {
        debug!("Stopping container before restore...");
        container::stop(&config.machine_name)?;
    }

    backup::restore(&config, input.as_deref())?;

    eprintln!("✓ Enrollment restored. Bring the container up with:");
    eprintln!("  intune-container daemon   (background SSO)");
    eprintln!("  intune-container enroll   (if you also need the portal)");

    Ok(())
}

fn cmd_backup_inspect(input: Option<std::path::PathBuf>) -> Result<()> {
    backup::inspect(input.as_deref())?;
    Ok(())
}
