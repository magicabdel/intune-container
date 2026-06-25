//! intune-container: a single binary that is both the graphical interface (the
//! default) and the command-line tool.
//!
//! * Run with **no subcommand** (or `gui`) to open the Tauri desktop interface.
//! * Run with any **subcommand** (`enroll`, `edge`, `stop`, …) for CLI behavior.
//!
//! Both paths call the same in-process [`intune_container::ops`] functions.

mod gui;

use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use intune_container::{config, doctor, ops};

#[derive(Parser)]
#[command(
    name = "intune-container",
    about = "Manage Microsoft Intune in an isolated, rootless Linux container",
    long_about = "Manage Microsoft Intune in an isolated, rootless Linux container.\n\n\
                  Run with no subcommand to open the graphical interface.",
    version
)]
struct Cli {
    /// Subcommand to run. When omitted, the graphical interface opens.
    #[command(subcommand)]
    command: Option<Command>,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Run the interface in the foreground instead of detaching from the
    /// terminal (only affects the GUI; useful for debugging).
    #[arg(short = 'F', long, global = true, hide = true)]
    foreground: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Open the graphical interface (default when no subcommand is given)
    Gui,
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
    /// Start the container headless and make browser SSO ready (Teams/M365 in
    /// your host browser)
    Start,
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
    /// Detach the host display from the running container (back to headless)
    #[command(name = "detach-display", hide = true)]
    DetachDisplay,
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

    /// Internal re-exec target: rootless container supervisor (not for direct use)
    #[command(name = "__rootless-supervise", hide = true)]
    RootlessSupervise {
        /// Forward the host display into the container at boot
        #[arg(long)]
        display: bool,
    },
    /// Internal re-exec target: rootless in-container exec waiter (not for direct use)
    #[command(name = "__rootless-exec", hide = true)]
    RootlessExec {
        /// Host pid of the container's PID 1
        leader: i32,
        /// In-container uid to run as
        uid: u32,
        /// Shell script to run via `bash -lc`
        script: String,
        /// `KEY=VALUE` environment pairs (after `--`)
        #[arg(trailing_var_arg = true)]
        env: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Logs go to STDERR — stdout is reserved for the native-messaging protocol.
    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        // Color on a real terminal; plain text when redirected to the log file.
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();

    let foreground = cli.foreground;

    let Some(command) = cli.command else {
        // No subcommand → open the graphical interface.
        run_or_detach_gui(foreground);
        return Ok(());
    };

    match command {
        Command::Gui => run_or_detach_gui(foreground),
        Command::Init { force, image } => cmd_init(force, image)?,
        Command::Enroll { image } => cmd_enroll(image)?,
        Command::Start => cmd_start()?,
        Command::Edge { verbose, args } => ops::edge(verbose, &args)?,
        Command::Stop => ops::stop()?,
        Command::Status => cmd_status()?,
        Command::Doctor => doctor::run()?,
        Command::Backup { output } => cmd_backup(output)?,
        Command::BackupInspect { input } => ops::backup_inspect(input.as_deref())?,
        Command::Restore { input } => cmd_restore(input)?,
        Command::NativeHost => ops::native_host()?,
        Command::DetachDisplay => cmd_detach_display()?,
        Command::SsoTest => ops::sso_test()?,
        Command::Shell => ops::shell()?,
        Command::Destroy { force, purge } => cmd_destroy(force, purge)?,
        Command::RootlessSupervise { display } => cmd_rootless_supervise(display)?,
        Command::RootlessExec {
            leader,
            uid,
            script,
            env,
        } => cmd_rootless_exec(leader, uid, script, env)?,
    }

    Ok(())
}

/// Internal: run the container supervisor (re-exec target). Diverges on success.
fn cmd_rootless_supervise(display: bool) -> Result<()> {
    let code = intune_container::backend::supervise_main(display)?;
    std::process::exit(code);
}

/// Internal: run the in-container exec waiter (re-exec target).
fn cmd_rootless_exec(leader: i32, uid: u32, script: String, env: Vec<String>) -> Result<()> {
    let env: Vec<(String, String)> = env
        .iter()
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    let code = intune_container::backend::exec_main(leader, uid, &script, &env)?;
    std::process::exit(code);
}

/// Run the GUI, detaching from the controlling terminal by default so the
/// launching shell is freed and closing it won't kill the app. `--foreground`
/// (or the internal child marker set on the re-spawned process) keeps it
/// attached.
fn run_or_detach_gui(foreground: bool) {
    if foreground || std::env::var_os("INTUNE_CONTAINER_GUI_CHILD").is_some() {
        gui::run();
        return;
    }
    match spawn_detached_gui() {
        Ok(()) => {
            eprintln!("\u{2713} Interface launched — you can close this terminal.")
        }
        Err(e) => {
            eprintln!("Could not detach ({e:#}); running in the foreground instead.");
            gui::run();
        }
    }
}

/// Re-spawn this binary as a detached `gui` child in its own session, with stdio
/// redirected, so it survives the terminal closing (no controlling tty → no
/// SIGHUP). Its stderr is captured to a log for diagnosability.
fn spawn_detached_gui() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("cannot determine own executable path")?;

    let mut cmd = Command::new(exe);
    cmd.arg("gui")
        .env("INTUNE_CONTAINER_GUI_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    match open_gui_log() {
        Some(file) => {
            cmd.stderr(Stdio::from(file));
        }
        None => {
            cmd.stderr(Stdio::null());
        }
    }

    // SAFETY: `setsid` takes no arguments; failing (already a session leader) is
    // tolerated. Detaching into a new session removes the controlling terminal.
    unsafe {
        cmd.pre_exec(|| {
            let _ = nix::unistd::setsid();
            Ok(())
        });
    }

    cmd.spawn()
        .context("failed to spawn the detached interface")?;
    Ok(())
}

/// Append-mode log for the detached GUI child
/// (`~/.local/share/intune-container/gui.log`).
fn open_gui_log() -> Option<std::fs::File> {
    let dir = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })?
        .join("intune-container");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gui.log"))
        .ok()
}

/// Prompt for and confirm the container-user password.
fn prompt_password() -> Result<String> {
    let password = rpassword::prompt_password("Enter password for container user: ")
        .context("Failed to read password")?;
    let confirm = rpassword::prompt_password("Confirm password: ")
        .context("Failed to read password confirmation")?;
    if password != confirm {
        anyhow::bail!("Passwords do not match");
    }
    Ok(password)
}

fn cmd_init(force: bool, image: Option<String>) -> Result<()> {
    let password = prompt_password()?;
    ops::init(force, image.as_deref(), &password)
}

/// First-time setup in one command: init (if needed) → start → open portal.
fn cmd_enroll(image: Option<String>) -> Result<()> {
    if !ops::is_initialized() {
        eprintln!("Setting up the Intune container (one-time)...");
        let password = prompt_password()?;
        ops::init(false, image.as_deref(), &password)?;
    }

    eprintln!();
    eprintln!("Opening Intune Portal — the window can take up to ~30s the first time.");
    eprintln!();

    let closed = ops::enroll()?;

    if closed {
        eprintln!("✓ Done. Start background browser SSO with:  intune-container start");
    } else {
        eprintln!(
            "⚠ The portal didn't open. Try again with:  intune-container enroll\n  \
             or check the stack with:  intune-container doctor"
        );
    }
    Ok(())
}

fn cmd_start() -> Result<()> {
    let report = ops::start()?;

    eprintln!("✓ Container started. Native messaging host installed.");
    for p in &report.manifests {
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

fn cmd_detach_display() -> Result<()> {
    ops::detach_display()?;
    eprintln!("✓ Host display detached — the container is headless again.");
    Ok(())
}

fn cmd_status() -> Result<()> {
    let s = ops::status();
    if !s.configured {
        anyhow::bail!("Configuration not found. Run `intune-container init` first.");
    }

    println!("=== intune-container status ===");
    println!();
    println!("Machine:      {}", s.machine_name);
    println!("Rootfs:       {}", s.rootfs_path);
    println!("Running:      {}", if s.running { "yes" } else { "no" });
    println!("Host user:    {} (uid {})", s.host_user, s.host_uid);
    println!(
        "Display fwd:  {}",
        if s.display_forwarding {
            "on (GUI session active)"
        } else {
            "off (headless — default, max isolation)"
        }
    );
    println!(
        "Bus exposed:  {}",
        if s.expose_bus {
            "yes (browser SSO)"
        } else {
            "no"
        }
    );
    println!();
    println!("=== Display Environment ===");
    println!();
    println!("Compositor:   {}", s.compositor);

    match s.wayland {
        Some(ref ws) => println!("Wayland:      {}", ws),
        None => println!("Wayland:      not detected"),
    }
    match s.x11_display {
        Some(ref x11) => {
            println!("X11 display:  {}", x11);
            println!("Abstract X11: {}", s.has_abstract_x11);
        }
        None => println!("X11:          not detected"),
    }
    match s.xauthority {
        Some(ref xa) => println!("Xauthority:   {}", xa),
        None => println!("Xauthority:   not found"),
    }

    Ok(())
}

fn cmd_backup(output: Option<std::path::PathBuf>) -> Result<()> {
    let path = ops::backup(output.as_deref())?;
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
    ops::restore(input.as_deref())?;
    eprintln!("✓ Enrollment restored. Bring the container up with:");
    eprintln!("  intune-container start    (background SSO)");
    eprintln!("  intune-container enroll   (if you also need the portal)");
    Ok(())
}

fn cmd_destroy(force: bool, purge: bool) -> Result<()> {
    let config = config::Config::load().context("Failed to load configuration")?;
    let config_path = config::Config::config_path()?;

    eprintln!("This will permanently delete:");
    eprintln!("  • Container rootfs:  {}", config.rootfs_path.display());
    eprintln!("  • Configuration:     {}", config_path.display());
    if purge {
        eprintln!("  • Enrollment store:  ~/.local/share/intune-container/persist");
    }
    eprintln!();

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

    let outcome = ops::destroy(purge)?;

    eprintln!();
    eprintln!("✓ Container destroyed.");
    eprintln!();
    eprintln!("Removed:");
    eprintln!("  ✓ Rootfs");
    eprintln!("  ✓ Config");
    eprintln!("  ✓ Browser SSO manifests");
    if outcome.purged {
        eprintln!("  ✓ Enrollment store ({})", outcome.device_state);
    } else {
        eprintln!();
        eprintln!("Kept (use --purge to remove it too):");
        eprintln!("  • Enrollment store: {}", outcome.device_state);
    }

    Ok(())
}
