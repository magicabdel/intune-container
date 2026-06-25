//! The container backend: a pure-Rust, rootless runtime.
//!
//! Boots the rootfs's systemd inside an unprivileged user namespace via a
//! detached supervisor process, and enters the running container with `setns`
//! for exec/shell/probes — no `sudo`, `systemd-nspawn`, `machinectl`, or
//! `nsenter` helper. In-container apps run as the container's root, which the
//! user-namespace id-map points at the unprivileged host user, so host-owned
//! resources (the Wayland socket, the persistence store) are accessible and
//! anything created stays owned by the host user.
//!
//! [`crate::ops`] drives these functions directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::display::DisplayInfo;
use crate::lock::SingletonLock;
use crate::provision::{self, ContainerUser};
use crate::runtime;

/// Default published image, pulled when no rootfs exists yet.
const DEFAULT_IMAGE: &str = "ghcr.io/magicabdel/intune-container:latest";

/// The container's user session bus (root's), where the identity broker
/// registers `com.microsoft.identity.broker1`.
const BROKER_BUS: &str = "/run/user/0/bus";

// ===== Lifecycle =====

/// Whether the container is currently running.
pub fn is_running(_config: &Config) -> bool {
    matches!(runtime::running_leader(), Ok(Some(_)))
}

/// Provision the rootfs (download image, install the session profile). The
/// caller marks the config initialized and saves it afterwards.
pub fn initialize(config: &mut Config, _password: &str, image: Option<&str>) -> Result<()> {
    runtime::preflight().context("this host can't run the rootless backend")?;
    let rootfs = rootless_rootfs(config);
    config.rootfs_path = rootfs.clone();
    let image = image.unwrap_or(DEFAULT_IMAGE);
    info!(%image, dest = %rootfs.display(), "Provisioning rootless rootfs...");
    crate::oci::pull_rootfs(image, &rootfs).context("failed to pull/extract the rootfs")?;
    provision::provision(&rootfs, &run_user()).context("failed to provision the rootfs")?;
    info!("rootfs provisioned (session profile + keyring)");
    Ok(())
}

/// Remove the container's root filesystem (used by `init --force` / destroy).
/// The rootfs home is chowned to the container user (a mapped subuid) at
/// runtime, so removal happens inside a user namespace that maps that range.
pub fn remove_rootfs(config: &Config) -> Result<()> {
    runtime::remove_tree_as_root(&rootless_rootfs(config))?;
    let _ = runtime::clear_runtime_state();
    Ok(())
}

/// Boot the container, forwarding the host display when `with_display`.
pub fn start(config: &mut Config, _display: &DisplayInfo, with_display: bool) -> Result<()> {
    runtime::preflight().context("this host can't run the rootless backend")?;
    ensure_rootfs(config)?;

    if matches!(runtime::running_leader(), Ok(Some(_))) {
        debug!("container already running");
        return Ok(());
    }

    spawn_supervisor(with_display)?;

    // Wait for the supervisor to boot systemd and publish its state, then set up
    // the per-user session (XDG_RUNTIME_DIR, keyring, compliance agent).
    for _ in 0..40 {
        if matches!(runtime::running_leader(), Ok(Some(_))) {
            prepare_session(config, !with_display);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!(
        "container did not start within 20s (see {})",
        boot_log().display()
    )
}

/// Stop the container (poweroff + clear state).
pub fn stop(_config: &Config) -> Result<()> {
    if let Some(leader) = runtime::running_leader()? {
        runtime::poweroff_pid(leader);
        for _ in 0..40 {
            if !runtime::is_running(leader) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = runtime::clear_runtime_state();
    }
    Ok(())
}

/// Attach the host display to the running container. Display sockets are bound
/// at boot, so a headless→display switch is a restart with the sockets attached.
pub fn attach_display(config: &mut Config, display: &DisplayInfo) -> Result<()> {
    if config.display_forwarding {
        return Ok(());
    }
    info!("Restarting container with the host display attached...");
    stop(config)?;
    start(config, display, true)?;
    config.display_forwarding = true;
    config.save()?;
    Ok(())
}

/// Detach the host display, returning the container to headless isolation.
pub fn detach_display(config: &mut Config) -> Result<()> {
    if !config.display_forwarding {
        return Ok(());
    }
    info!("Restarting container headless (detaching display)...");
    let headless = DisplayInfo::detect();
    stop(config)?;
    start(config, &headless, false)?;
    config.display_forwarding = false;
    config.save()?;
    Ok(())
}

/// Launch `command` in the background (reparented to the container's init).
pub fn exec(
    config: &Config,
    command: &str,
    prelaunch: Option<&str>,
    display: &DisplayInfo,
) -> Result<()> {
    run(config, command, prelaunch, display, true)
}

/// Launch `command` in the foreground (blocks until it exits).
pub fn exec_foreground(
    config: &Config,
    command: &str,
    prelaunch: Option<&str>,
    display: &DisplayInfo,
) -> Result<()> {
    run(config, command, prelaunch, display, false)
}

/// Open an interactive login shell inside the container.
pub fn shell(config: &Config) -> Result<()> {
    let leader = runtime::running_leader()?.context("container is not running")?;
    let user = cuser(config);
    let env = session_env(&user, &DisplayInfo::detect());
    let code = runtime::exec_pid_env(leader, &["/bin/bash", "--login"], Some(user.uid), &env)?;
    if code != 0 {
        anyhow::bail!("shell exited with status {code}");
    }
    Ok(())
}

/// Block until the container has booted far enough to launch the portal.
pub fn wait_until_portal_ready(config: &Config) {
    info!("Waiting for Intune services to be ready...");
    let script = "for _ in $(seq 1 60); do \
            state=$(systemctl is-system-running 2>/dev/null || true); \
            case \"$state\" in \"\"|starting|initializing) sleep 1 ;; *) break ;; esac; \
          done; \
          for _ in $(seq 1 45); do \
            [ -S /run/intune/daemon.socket ] && \
            [ \"$(systemctl is-active microsoft-identity-device-broker.service 2>/dev/null)\" = active ] && \
            exit 0; sleep 1; \
          done; exit 1";
    if probe(config, script) != 0 {
        warn!("Intune daemon/broker not ready in time — launching portal anyway");
    }
}

/// Wait for `process` to appear, then block until it exits. Returns `true` if
/// it was seen running (then exited).
pub fn wait_for_app_exit(config: &Config, process: &str) -> bool {
    let script = format!(
        "appeared=0; \
         for _ in $(seq 1 45); do \
           if pgrep -u \"$(id -u)\" -x {process} >/dev/null 2>&1; then appeared=1; break; fi; \
           sleep 1; \
         done; \
         [ \"$appeared\" = 0 ] && exit 1; \
         while pgrep -u \"$(id -u)\" -x {process} >/dev/null 2>&1; do sleep 0.3; done; \
         exit 0"
    );
    probe(config, &script) == 0
}

/// Whether a display-forwarded GUI app (Edge/portal) is still running.
pub fn gui_app_running(config: &Config) -> bool {
    let script = "pgrep -u \"$(id -u)\" -x msedge >/dev/null 2>&1 || \
                  pgrep -u \"$(id -u)\" -x intune-portal >/dev/null 2>&1";
    probe(config, script) == 0
}

/// Remove the persistent state kept outside the rootfs (the keyring/device-state
/// store). Used by `destroy --purge`. The keyring dirs are owned by a mapped
/// subuid, so removal happens inside a user namespace mapping that range.
pub fn purge() -> Result<()> {
    runtime::remove_tree_as_root(&persist_dir())
}

/// Back up enrollment state to a tar archive (caller stops the container first).
pub fn backup(output: Option<&Path>) -> Result<PathBuf> {
    crate::backup::backup_rootless(output)
}

/// Restore enrollment state from a tar archive (caller stops the container first).
pub fn restore(input: Option<&Path>) -> Result<()> {
    crate::backup::restore_rootless(input)
}

/// Run the browser native-messaging host bridge to the identity broker, from
/// **inside** the container (via `setns`) so it reaches the broker on the
/// container's own session bus. The browser's stdin/stdout pipes carry through
/// the fork, so no host bus exposure is needed.
pub fn native_host(_config: &Config) -> Result<()> {
    let leader = runtime::running_leader()?.context("container is not running")?;
    let code = runtime::run_in_container(leader, Some(0), &broker_bridge_env(), || {
        match crate::native_host::run_blocking(Path::new(BROKER_BUS)) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("native host: {e:#}");
                1
            }
        }
    })?;
    if code != 0 {
        anyhow::bail!("native host exited with status {code}");
    }
    Ok(())
}

/// Query the broker directly (SSO debugging), from inside the container.
pub fn sso_test(_config: &Config) -> Result<()> {
    let leader = runtime::running_leader()?.context("container is not running")?;
    let code = runtime::run_in_container(leader, Some(0), &broker_bridge_env(), || {
        match crate::native_host::test_blocking(Path::new(BROKER_BUS)) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("sso-test: {e:#}");
                1
            }
        }
    })?;
    if code != 0 {
        anyhow::bail!("sso-test exited with status {code}");
    }
    Ok(())
}

// ===== Entry points for the hidden re-exec subcommands (called from main) =====

/// Body of `__rootless-supervise`: boot systemd, publish state, stay alive to
/// reap PID 1. Returns the container's exit code.
pub fn supervise_main(with_display: bool) -> Result<i32> {
    // Hard singleton: at most one supervisor — hence one container — may run.
    // The lock is held for this process's whole life and released on exit. A
    // second supervisor (spawned by a racing `start()`, a boot-timeout retry, or
    // stale runtime state) finds the lock held and exits without booting a
    // duplicate. We retry briefly so a legitimate restart (the previous
    // supervisor still releasing the lock as its container powers off) is not
    // mistaken for a live duplicate.
    let mut held = None;
    for _ in 0..30 {
        match SingletonLock::try_acquire("supervisor")? {
            Some(l) => {
                held = Some(l);
                break;
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _singleton = match held {
        Some(l) => l,
        None => {
            info!("another container supervisor is already running; not booting a duplicate");
            return Ok(0);
        }
    };

    let mut config = Config::load().context("load config in supervisor")?;
    let rootfs = rootless_rootfs(&config);
    config.rootfs_path = rootfs.clone();

    let user = run_user();
    let mut binds = persistence_binds(&user).context("persistence binds")?;
    if with_display {
        let display = DisplayInfo::detect();
        binds.extend(display.attach_plan(user.uid).binds);
    }

    let log = boot_log();
    // Headless (no display forwarded) selects the hardened security profile;
    // an attached display selects the compat profile (see SECURITY.md).
    let hardened = !with_display;
    let container =
        runtime::start_systemd(&rootfs, &binds, Some(&log), hardened).context("start_systemd")?;
    runtime::save_runtime_state(&container.state())?;
    info!(leader = container.leader_pid(), "container booted");

    let code = container.wait().unwrap_or(0);
    let _ = runtime::clear_runtime_state();
    Ok(code)
}

/// Body of `__rootless-exec`: enter the container and run a command, blocking.
pub fn exec_main(leader: i32, uid: u32, script: &str, env: &[(String, String)]) -> Result<i32> {
    runtime::exec_pid_env(leader, &["/bin/bash", "-lc", script], Some(uid), env)
}

// ===== Internals =====

/// Environment for an in-container broker bridge so it reaches the right bus.
fn broker_bridge_env() -> Vec<(String, String)> {
    vec![
        ("XDG_RUNTIME_DIR".into(), "/run/user/0".into()),
        (
            "DBUS_SESSION_BUS_ADDRESS".into(),
            format!("unix:path={BROKER_BUS}"),
        ),
        ("HOME".into(), "/root".into()),
    ]
}

/// User-writable rootfs location. Honors an explicit user-writable path;
/// otherwise (or for a legacy `/var/lib/machines` default) uses the data dir.
fn rootless_rootfs(config: &Config) -> PathBuf {
    if config.rootfs_path.starts_with("/var/lib") {
        data_dir().join("rootfs")
    } else {
        config.rootfs_path.clone()
    }
}

fn data_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("intune-container")
}

fn boot_log() -> PathBuf {
    data_dir().join("rootless-boot.log")
}

/// Host directory (outside the rootfs) holding state that must survive a rootfs
/// rebuild: device registration, agent state, and the user's keyring/config.
fn persist_dir() -> PathBuf {
    data_dir().join("persist")
}

/// `(host_src, container_dst)` persistence bind mounts. The `state/*` dirs are
/// used by the broker as root; the `home/*` dirs back the user's keyring/config.
/// All live under [`persist_dir`] so a rootfs rebuild never wipes them.
fn persistence_binds(user: &ContainerUser) -> Result<Vec<(PathBuf, PathBuf)>> {
    let base = persist_dir();
    let specs: [(&str, String); 5] = [
        (
            "state/device-broker",
            "/var/lib/microsoft-identity-device-broker".to_string(),
        ),
        ("state/intune", "/var/lib/intune".to_string()),
        (
            "home/keyrings",
            format!("{}/.local/share/keyrings", user.home),
        ),
        (
            "home/config-broker",
            format!("{}/.config/microsoft-identity-broker", user.home),
        ),
        (
            "home/config-intune",
            format!("{}/.config/intune", user.home),
        ),
    ];
    let mut binds = Vec::with_capacity(specs.len());
    for (sub, dst) in specs {
        let src = base.join(sub);
        std::fs::create_dir_all(&src)
            .with_context(|| format!("create persistence dir {}", src.display()))?;
        binds.push((src, PathBuf::from(dst)));
    }
    Ok(binds)
}

/// The session user GUI apps / shells run as: the container's **root**, which
/// our id-map points at the real host user — so host-owned resources (the
/// Wayland socket, the persistence binds) are accessible and anything created
/// stays owned by the host user. A non-root container user (a mapped subuid)
/// can't even connect to the host Wayland socket without idmapped mounts.
fn run_user() -> ContainerUser {
    ContainerUser {
        uid: 0,
        name: "root".into(),
        home: "/root".into(),
    }
}

fn cuser(_config: &Config) -> ContainerUser {
    run_user()
}

/// Full environment for a launched app: the display attach env plus the user's
/// HOME/USER/LOGNAME so login shells and the keyring resolve right.
fn session_env(user: &ContainerUser, display: &DisplayInfo) -> Vec<(String, String)> {
    let mut env = display.attach_plan(user.uid).env;
    env.push(("HOME".into(), user.home.clone()));
    env.push(("USER".into(), user.name.clone()));
    env.push(("LOGNAME".into(), user.name.clone()));
    env
}

/// Ensure the rootfs is present (pull on first use) and provisioned.
fn ensure_rootfs(config: &mut Config) -> Result<PathBuf> {
    let rootfs = rootless_rootfs(config);
    if config.rootfs_path != rootfs {
        config.rootfs_path = rootfs.clone();
    }
    let populated = rootfs.join("sbin/init").exists() || rootfs.join("usr/sbin/init").exists();
    if !populated {
        info!(image = %DEFAULT_IMAGE, dest = %rootfs.display(), "Pulling rootfs (pure Rust OCI)...");
        crate::oci::pull_rootfs(DEFAULT_IMAGE, &rootfs)
            .context("failed to pull/extract the rootfs")?;
    }
    // Provisioning is idempotent; run it on every start so an existing rootfs
    // picks up updates too.
    provision::provision(&rootfs, &run_user()).context("failed to provision the rootfs")?;
    Ok(rootfs)
}

/// Run `command` as the container user with the display environment, via
/// `setns`. `background` spawns a detached waiter and returns immediately.
fn run(
    config: &Config,
    command: &str,
    prelaunch: Option<&str>,
    display: &DisplayInfo,
    background: bool,
) -> Result<()> {
    let leader = runtime::running_leader()?.context("container is not running")?;
    let user = cuser(config);
    let env = session_env(&user, display);

    // We run apps as the container's root; Chromium-based apps (Edge) refuse to
    // launch as root without --no-sandbox (the user namespace already isolates).
    let command = if command.contains("microsoft-edge") && !command.contains("--no-sandbox") {
        format!("{command} --no-sandbox")
    } else {
        command.to_string()
    };
    let script = match prelaunch {
        Some(pre) => format!("{pre}\nexec {command}"),
        None => format!("exec {command}"),
    };

    if background {
        // Detach a helper subprocess that enters the container and runs the app;
        // it's reparented when we exit, leaving the app running.
        spawn_exec_helper(leader, user.uid, &script, &env)
    } else {
        let code =
            runtime::exec_pid_env(leader, &["/bin/bash", "-lc", &script], Some(user.uid), &env)?;
        if code != 0 {
            anyhow::bail!("command exited with status {code}");
        }
        Ok(())
    }
}

/// Run a probe shell snippet in the container as root and return its exit code.
/// Useful for health checks (`doctor`).
pub fn probe(config: &Config, script: &str) -> i32 {
    let leader = match runtime::running_leader() {
        Ok(Some(l)) => l,
        _ => return 1,
    };
    let uid = cuser(config).uid;
    runtime::exec_pid_env(leader, &["/bin/sh", "-c", script], Some(uid), &[]).unwrap_or(1)
}

/// Recreate the per-user session a PAM login would (XDG_RUNTIME_DIR, D-Bus
/// session bus, unlocked keyring, compliance agent timer). Runs once per boot as
/// the container's root via `setns`. Best-effort.
fn prepare_session(config: &Config, headless: bool) {
    let leader = match runtime::running_leader() {
        Ok(Some(l)) => l,
        _ => {
            warn!("session setup skipped: container leader not found");
            return;
        }
    };
    let user = cuser(config);
    let script = provision::runtime_setup_script(&user, headless);
    // Surface failures loudly. This routine sets up the keyring and starts the
    // compliance agent timer; when it silently failed (e.g. a setns EPERM), the
    // device drifted to non-compliant with no visible signal for days.
    match runtime::exec_pid_env(leader, &["/bin/sh", "-c", &script], None, &[]) {
        Ok(0) => {}
        Ok(code) => warn!(code, "session setup script exited non-zero"),
        Err(e) => error!("session setup failed to run in the container: {e:#}"),
    }
    // The compliance agent must end up running, or the device never reports
    // compliant. A masked/inactive timer is the exact silent failure we hit.
    if !agent_timer_active(config) {
        warn!(
            "Intune compliance agent timer is not active after session setup; the \
             device will not report compliant. Check: intune-container doctor"
        );
    }
}

/// Whether the Intune compliance agent timer is active (not masked or stopped)
/// in the running container's user session. Runs as the container root (uid 0),
/// whose user manager owns the timer.
fn agent_timer_active(config: &Config) -> bool {
    let script = "export XDG_RUNTIME_DIR=/run/user/0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus; systemctl --user is-active --quiet intune-agent.timer";
    probe(config, script) == 0
}

/// Re-exec this binary as a detached `__rootless-supervise` process that boots
/// systemd, publishes the runtime state, and stays alive to reap PID 1.
fn spawn_supervisor(with_display: bool) -> Result<()> {
    let exe = std::env::current_exe().context("cannot determine own executable path")?;
    let mut cmd = Command::new(exe);
    cmd.arg("__rootless-supervise");
    if with_display {
        cmd.arg("--display");
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(boot_log())
    {
        Ok(f) => {
            cmd.stderr(Stdio::from(f));
        }
        Err(_) => {
            cmd.stderr(Stdio::null());
        }
    }
    detach(&mut cmd);
    cmd.spawn().context("failed to spawn supervisor")?;
    Ok(())
}

/// Re-exec as a detached `__rootless-exec` waiter that enters the container,
/// runs the app, and blocks until it exits (reparented when we return).
fn spawn_exec_helper(leader: i32, uid: u32, script: &str, env: &[(String, String)]) -> Result<()> {
    let exe = std::env::current_exe().context("cannot determine own executable path")?;
    let mut cmd = Command::new(exe);
    cmd.arg("__rootless-exec")
        .arg(leader.to_string())
        .arg(uid.to_string())
        .arg(script)
        .arg("--");
    for (k, v) in env {
        cmd.arg(format!("{k}={v}"));
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    // Capture the launched app's stderr to the boot log so failures (display,
    // D-Bus, GTK init, …) are diagnosable instead of vanishing.
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(boot_log())
    {
        Ok(f) => {
            cmd.stderr(Stdio::from(f));
        }
        Err(_) => {
            cmd.stderr(Stdio::null());
        }
    }
    detach(&mut cmd);
    cmd.spawn().context("failed to spawn exec helper")?;
    Ok(())
}

/// Put the spawned child in its own session so it survives us exiting.
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid takes no args; failure (already a leader) is tolerated.
    unsafe {
        cmd.pre_exec(|| {
            let _ = nix::unistd::setsid();
            Ok(())
        });
    }
}
