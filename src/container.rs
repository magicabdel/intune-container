//! Container lifecycle management using systemd-nspawn and machinectl.

use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::display::DisplayInfo;

/// Check if a machine is currently running via machinectl.
pub fn is_running(machine: &str) -> bool {
    let output = Command::new("machinectl")
        .args(["show", machine, "-p", "State", "--value"])
        .output();

    match output {
        Ok(o) => {
            let state = String::from_utf8_lossy(&o.stdout).trim().to_string();
            debug!(machine = %machine, state = %state, "Machine state");
            state == "running"
        }
        Err(e) => {
            debug!(error = %e, "machinectl show failed");
            false
        }
    }
}

/// Persistent host directory (outside the rootfs) for state that must survive
/// container rebuilds (`init --force`). The rootfs at /var/lib/machines/<name>
/// is wiped on rebuild; this directory is not.
const PERSISTENT_STATE_DIR: &str = "/var/lib/intune-container";

/// Device-state subdirectories: (host subdir, container path).
/// These hold the device registration and agent state — losing them forces a
/// full re-enroll, so we persist them outside the rootfs.
fn device_state_mounts() -> [(&'static str, &'static str); 2] {
    [
        ("device-broker", "/var/lib/microsoft-identity-device-broker"),
        ("intune", "/var/lib/intune"),
    ]
}

/// Create the persistent device-state directories on the host (root-owned, as
/// the device-broker runs as root inside the container). Idempotent.
fn ensure_persistent_dirs() -> Result<()> {
    for (sub, _) in device_state_mounts() {
        let host_dir = format!("{}/{}", PERSISTENT_STATE_DIR, sub);
        let status = Command::new("sudo")
            .args(["mkdir", "-p", &host_dir])
            .status()
            .context("Failed to create persistent device-state directory")?;
        if !status.success() {
            anyhow::bail!("Failed to create {}", host_dir);
        }
    }
    Ok(())
}

/// Start the container with proper bind mounts for display, audio, and GPU.
///
/// `with_display` controls whether the real host display (X11/Wayland/GPU) is
/// forwarded. The default everywhere is headless (`false`); only the GUI flows
/// (`enroll`, `edge`) pass `true`.
pub fn start(config: &Config, display: &DisplayInfo, with_display: bool) -> Result<()> {
    // Ensure persistent device-state dirs exist before boot (they're bind-mounted
    // so device registration survives rebuilds).
    ensure_persistent_dirs()?;

    let args = build_nspawn_args(config, display, with_display)?;

    info!(with_display, "Booting container...");
    debug!(
        command = format!("sudo systemd-nspawn {}", args.join(" ")),
        "nspawn command"
    );

    // Write display marker into rootfs before boot
    write_display_marker(config, display, with_display)?;

    // Start nspawn in background
    let status = Command::new("sudo")
        .arg("systemd-nspawn")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to execute systemd-nspawn")?;

    // Don't wait — it's a background boot process
    std::mem::forget(status);

    // Wait for container to be running
    debug!("Waiting for container to boot...");
    let mut booted = false;
    for i in 0..30 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if is_running(&config.machine_name) {
            debug!("Container booted in {}s", i + 1);
            booted = true;
            break;
        }
    }

    if !booted {
        anyhow::bail!("Container did not start within 30 seconds");
    }

    // If bus exposure is enabled, ensure the container's user session (and thus
    // its session D-Bus bus) is up, then wait for the bus socket to appear on
    // the host. Mirrors intuneme's readiness sequence.
    if config.expose_bus {
        prepare_broker_session(config, with_display)?;
    }

    Ok(())
}

/// Bring up the container user session and wait for its D-Bus session bus.
///
/// The session bus (/run/user/<uid>/bus) only exists when the container user
/// has an active/lingering session. We enable linger (so the session persists),
/// create a login session to start it now, then wait for the bus socket to
/// appear on the host bind-mount.
fn prepare_broker_session(config: &Config, with_display: bool) -> Result<()> {
    let user_machine = format!("{}@{}", config.host_user, config.machine_name);

    // Bring up the container user session in a SINGLE machinectl shell rather
    // than one spawn per step. This script:
    //   1. enables linger so the user session (and its D-Bus bus) persists;
    //   2. starts the user D-Bus session bus;
    //   3. unlocks gnome-keyring so the identity broker can read stored tokens
    //      (kill + restart the daemon that owns org.freedesktop.secrets — a
    //      second --unlock daemon never takes the name, so its unlock is
    //      ignored; `echo ""` sends an empty-string password, `echo -n ""`
    //      would send EOF and do nothing);
    //   4. (display-forwarding only) publishes the real host display into the
    //      D-Bus activation environment so the GTK broker has a DISPLAY when it
    //      is activated on demand. Headless mode does this via Xvfb instead
    //      (see start_virtual_display), which must use the nsenter helper.
    debug!("Bringing up container user session (linger, bus, keyring)...");
    let mut session_script = format!(
        r#"loginctl enable-linger {user} 2>/dev/null || true
systemctl --user start dbus.socket dbus.service 2>/dev/null || true
mkdir -p "$HOME/.local/share/keyrings"
[ -f "$HOME/.local/share/keyrings/default" ] || echo login > "$HOME/.local/share/keyrings/default"
pkill -u "$(id -u)" -x gnome-keyring-d 2>/dev/null
sleep 1
echo "" | gnome-keyring-daemon --unlock --components=secrets,pkcs11 -d 2>/dev/null
sleep 1
"#,
        user = config.host_user
    );

    if with_display {
        // Publish the real host display recorded by boot() in the marker file.
        session_script.push_str(
            r#"[ -f /etc/intune-container-display ] && . /etc/intune-container-display
if [ -n "${DISPLAY:-}" ]; then
    dbus-update-activation-environment DISPLAY XAUTHORITY WAYLAND_DISPLAY GDK_BACKEND XDG_RUNTIME_DIR 2>/dev/null || true
fi
"#,
        );
    }
    session_script.push_str("true\n");

    let _ = Command::new("machinectl")
        .args(["shell", &user_machine, "/bin/bash", "-lc", &session_script])
        .status();

    if !with_display {
        // Headless mode (default): start a PRIVATE in-container virtual display
        // (Xvfb :99, baked into the image) so the broker has a display with no
        // link to the host screen. Must go through the nsenter helper so Xvfb
        // persists (see start_virtual_display).
        start_virtual_display(config)?;
    }

    // Wait for the session bus socket to appear on the host bind-mount.
    let bus_path = config.broker_bus_path()?;
    debug!(socket = %bus_path.display(), "Waiting for container session bus...");
    for _ in 0..30 {
        if bus_path.exists() {
            debug!("Container session bus ready");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    warn!(
        socket = %bus_path.display(),
        "Container session bus did not appear within 30s — SSO may fail to connect"
    );
    Ok(())
}

/// Start the private in-container virtual display (Xvfb :99) used for headless
/// SSO, and publish DISPLAY=:99 into the D-Bus activation environment so the
/// on-demand-activated identity broker (a GTK app) has a display to init against.
///
/// Xvfb MUST be started through the nsenter helper, NOT `machinectl shell`: a
/// `machinectl shell` session is a transient systemd scope that is torn down
/// when the shell exits, killing every process in it — even those under
/// `setsid`. The nsenter helper backgrounds the process with `nohup … &`, after
/// which it is reparented to the container's PID 1 and persists for the life of
/// the container (the same mechanism that keeps GUI apps alive).
fn start_virtual_display(config: &Config) -> Result<()> {
    debug!("Starting private virtual display (Xvfb :99) for the broker...");

    let script = build_virtual_display_script(config.host_uid);

    let status = Command::new("sudo")
        .args([NSENTER_HELPER, "container", &script])
        .status()
        .context(
            "Failed to start virtual display via nsenter helper. \
             Is the sudoers rule installed? Run: intune-container init",
        )?;

    if !status.success() {
        warn!(
            "Virtual-display start script exited with status {} — headless SSO may fail",
            status.code().unwrap_or(-1)
        );
    }

    Ok(())
}

/// Build the shell script that starts the headless virtual display and publishes
/// it to the broker's activation environment.
///
/// Run via the nsenter helper so the backgrounded Xvfb is reparented to the
/// container's PID 1 and persists (a `machinectl shell` scope would kill it).
fn build_virtual_display_script(uid: u32) -> String {
    // Starts Xvfb detached (persists via PID-1 reparenting), waits for its
    // socket, then pushes DISPLAY into the session/activation env so the
    // on-demand-activated GTK broker has a display.
    format!(
        r#"export XDG_RUNTIME_DIR=/run/user/{uid}
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus
if ! command -v Xvfb >/dev/null 2>&1; then
    echo "intune-container: Xvfb missing from image — build an image with xvfb (see repo Dockerfile / 'just build-image') and set it as DEFAULT_IMAGE" >&2
    exit 0
fi
if ! pgrep -u "$(id -u)" -x Xvfb >/dev/null 2>&1; then
    nohup Xvfb :99 -screen 0 640x480x16 -nolisten tcp >/tmp/intune-xvfb.log 2>&1 &
fi
# Wait (up to ~6s) for the X socket to appear before publishing DISPLAY.
for _i in $(seq 1 20); do
    [ -S /tmp/.X11-unix/X99 ] && break
    sleep 0.3
done
export DISPLAY=:99
export GDK_BACKEND=x11
systemctl --user import-environment DISPLAY GDK_BACKEND 2>/dev/null || true
dbus-update-activation-environment --systemd DISPLAY GDK_BACKEND 2>/dev/null || true
dbus-update-activation-environment DISPLAY GDK_BACKEND 2>/dev/null || true
true
"#,
        uid = uid,
    )
}

/// Stop the container via machinectl.
pub fn stop(machine: &str) -> Result<()> {
    info!(machine = %machine, "Stopping container");

    let status = Command::new("sudo")
        .args(["machinectl", "poweroff", machine])
        .status()
        .context("Failed to execute machinectl poweroff")?;

    if !status.success() {
        // Try terminate as fallback
        warn!("poweroff failed, trying terminate");
        let status = Command::new("sudo")
            .args(["machinectl", "terminate", machine])
            .status()
            .context("Failed to execute machinectl terminate")?;

        if !status.success() {
            anyhow::bail!("Failed to stop container");
        }
    }

    // Wait for it to actually stop. Returning Ok while the machine is still
    // running is dangerous: callers (`init --force`, `destroy`) then `rm -rf`
    // the rootfs, which corrupts a still-mounted filesystem. So we escalate to
    // `terminate` and finally error out rather than reporting a false success.
    for _ in 0..15 {
        if !is_running(machine) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    warn!("Container did not stop gracefully — forcing terminate");
    let _ = Command::new("sudo")
        .args(["machinectl", "terminate", machine])
        .status();

    for _ in 0..10 {
        if !is_running(machine) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    anyhow::bail!(
        "Container '{}' is still running after stop + terminate; refusing to \
         continue (operating on a mounted rootfs would corrupt it)",
        machine
    );
}

/// Path to the nsenter helper script on the host.
const NSENTER_HELPER: &str = "/usr/local/libexec/intune-container/nsenter-exec";

/// How a command should be launched inside the container's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launch {
    /// Detach the process (`nohup … &`) so it survives the nsenter call
    /// returning — used for normal GUI launches.
    Background,
    /// `exec` the process so it stays attached to the terminal — used for
    /// `open -v` (verbose/foreground) so the user sees its output.
    Foreground,
}

/// Execute a command inside the running container using nsenter.
///
/// Uses a passwordless sudoers rule + root-owned helper script (same approach as intuneme).
/// This is the only reliable way to launch GUI apps:
/// - machinectl shell: kills processes on session exit
/// - systemd-run --machine: requires root, permission denied as user
/// - nsenter + nohup: process is reparented to container PID 1, persists
pub fn exec(
    machine: &str,
    user: &str,
    uid: u32,
    command: &str,
    prelaunch: Option<&str>,
    display: &DisplayInfo,
) -> Result<()> {
    let session_script = build_session_script(uid, display, command, prelaunch, Launch::Background);

    debug!(
        machine = %machine,
        user = %user,
        command = %command,
        "Executing via nsenter helper"
    );

    let status = Command::new("sudo")
        .args([NSENTER_HELPER, "host", &session_script])
        .status()
        .context("Failed to execute via nsenter helper. Is the sudoers rule installed? Run: intune-container init")?;

    if !status.success() {
        anyhow::bail!("Command failed with status {}", status.code().unwrap_or(-1));
    }

    Ok(())
}

/// Build the shell script that runs inside the container via nsenter.
///
/// This sets up the full session environment (display, D-Bus activation env,
/// keyring) and then launches the command in the background.
fn build_session_script(
    uid: u32,
    display: &DisplayInfo,
    command: &str,
    prelaunch: Option<&str>,
    launch: Launch,
) -> String {
    let uid_str = uid.to_string();

    let x11_display = display.x11_display.as_deref().unwrap_or(":0");

    let mut script = format!(
        r#"export XDG_RUNTIME_DIR=/run/user/{uid}
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus
export DISPLAY='{display}'
export GDK_BACKEND=x11
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
export NO_AT_BRIDGE=1
export GTK_A11Y=none
export PATH="/opt/microsoft/intune/bin:$PATH"
"#,
        uid = uid_str,
        display = x11_display,
    );

    // Wayland (if available)
    if display.wayland_socket.is_some() {
        script.push_str("export WAYLAND_DISPLAY=/run/host-wayland\n");
    }

    // Xauthority — only if we have one
    if display.xauthority.is_some() {
        script.push_str("export XAUTHORITY=/tmp/.container-xauth\n");
    }

    // Propagate environment to systemd user manager AND D-Bus activation env.
    // The identity broker is D-Bus-activated and only inherits these if pushed here.
    script.push_str(
        r#"
_env_vars="DISPLAY XAUTHORITY WAYLAND_DISPLAY GDK_BACKEND XDG_RUNTIME_DIR NO_AT_BRIDGE GTK_A11Y PATH"
_set_vars=""
for _v in $_env_vars; do
    [ -n "${!_v+x}" ] && _set_vars="$_set_vars $_v"
done
if [ -n "$_set_vars" ]; then
    systemctl --user import-environment $_set_vars 2>/dev/null || true
    dbus-update-activation-environment --systemd $_set_vars 2>/dev/null || true
fi
"#,
    );

    // Initialize gnome-keyring (once per boot via marker). Mirrors intuneme exactly.
    script.push_str(
        r#"
_keyring_dir="$HOME/.local/share/keyrings"
_keyring_marker="/tmp/.intune-container-keyring-init-done"

if [ ! -f "$_keyring_marker" ]; then
    mkdir -p "$_keyring_dir"
    # Tell gnome-keyring which collection is the default ("login")
    [ -f "$_keyring_dir/default" ] || echo "login" > "$_keyring_dir/default"

    # Is the login collection already unlocked? Don't disrupt an in-use keyring.
    _locked=$(busctl --user get-property org.freedesktop.secrets \
        /org/freedesktop/secrets/collection/login \
        org.freedesktop.Secret.Collection Locked 2>/dev/null)

    if [ "$_locked" != "b false" ]; then
        # The secrets name is held by the daemon that claimed it first. A second
        # --unlock daemon does NOT take the name, so its unlock never reaches the
        # broker's daemon. Kill ALL keyring daemons and start one unlocked.
        pkill -u "$(id -u)" -x gnome-keyring-d 2>/dev/null
        sleep 1
        # `echo ""` = newline = empty-string password (unlocks login keyring).
        # `echo -n ""` would send EOF = "no password" and do nothing!
        echo "" | gnome-keyring-daemon --unlock --components=secrets,pkcs11 -d 2>/dev/null
        sleep 1
    fi

    # Force-create the default collection so ReadAlias("default") resolves.
    if ! secret-tool lookup _keyring_init _keyring_init >/dev/null 2>&1; then
        echo "init" | secret-tool store --label="Keyring Init" _keyring_init _keyring_init 2>/dev/null
    fi

    touch "$_keyring_marker"

    # Restart the device broker so it re-reads the now-initialized keyring.
    sudo systemctl restart microsoft-identity-device-broker.service 2>/dev/null || true
fi

# Start the intune agent timer if not already running.
if ! systemctl -q --user is-active intune-agent.timer 2>/dev/null; then
    systemctl --user start intune-agent.timer 2>/dev/null || true
fi
"#,
    );

    // Audio + WebKit rendering fallback
    script.push_str(
        r#"
[ -S /run/host-pipewire ] && export PIPEWIRE_REMOTE=/run/host-pipewire
[ -S /run/host-pulse ] && export PULSE_SERVER=unix:/run/host-pulse

# WebKitGTK auth browser: fall back to software rendering if GPU is unavailable
export WEBKIT_DISABLE_COMPOSITING_MODE=1
"#,
    );

    // Pre-launch hook (e.g. Edge profile lock cleanup) — runs as a normal
    // statement, separate from the exec/nohup'd launch command.
    if let Some(pre) = prelaunch {
        script.push('\n');
        script.push_str(pre);
        script.push('\n');
    }

    // Launch the command. Background detaches it (survives nsenter returning);
    // Foreground execs it so it stays attached to the terminal.
    match launch {
        Launch::Background => {
            script.push_str(&format!("\nnohup {} >/dev/null 2>&1 &\n", command));
        }
        Launch::Foreground => {
            script.push_str(&format!("\nexec {}\n", command));
        }
    }

    script
}

/// Run a command inside the container and capture its stdout (trimmed).
/// Returns Err if the container isn't reachable or the command fails.
/// Used by `doctor` for health checks.
pub fn run_in_container(machine: &str, user: &str, command: &str) -> Result<String> {
    let output = Command::new("machinectl")
        .args([
            "shell",
            &format!("{}@{}", user, machine),
            "/bin/bash",
            "-lc",
            command,
        ])
        .output()
        .context("Failed to run command in container")?;

    if !output.status.success() {
        anyhow::bail!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Path on the host where persistent device-state is stored.
pub fn persistent_state_dir() -> &'static str {
    PERSISTENT_STATE_DIR
}

/// Wait until the container has finished booting before launching the portal.
///
/// `intune-portal` connects to the intune daemon at `/run/intune/daemon.socket`
/// and registers over D-Bus. Launching it mid-boot races service startup,
/// producing "Failed to register: ... Message recipient disconnected from
/// message bus" and no window. (intuneme avoids this by separating `start` from
/// a later, manual `open portal`; our `enroll` boots and launches in one step,
/// so we gate explicitly.)
///
/// Runs as a *single* in-container shell (one `machinectl` spawn). It primarily
/// waits for systemd to finish booting (`is-system-running` leaves "starting"),
/// so enabled units have been started; the daemon-socket check is a cheap sanity
/// touch (the socket is socket-activated and present from early boot, so it is
/// not itself a strong readiness signal). Best-effort: on timeout we proceed
/// rather than block enrollment.
pub fn wait_until_portal_ready(machine: &str, user: &str) {
    info!("Waiting for Intune services to be ready...");

    // Self-bounded (~90s worst case) so the single shell never hangs. Always
    // exits 0 and prints the outcome so run_in_container doesn't treat a
    // timeout as a command failure.
    let script = r#"
# 1. Wait (max 60s) for systemd to finish booting. is-system-running reports
#    starting/initializing until boot completes, then running/degraded/etc.
for _ in $(seq 1 60); do
    state=$(systemctl is-system-running 2>/dev/null || true)
    case "$state" in
        ""|starting|initializing) sleep 1 ;;
        *) break ;;
    esac
done
# 2. Wait (max 30s) for the intune daemon socket the portal connects to.
for _ in $(seq 1 30); do
    if [ -S /run/intune/daemon.socket ]; then
        echo ready
        exit 0
    fi
    sleep 1
done
echo timeout
exit 0
"#;

    match run_in_container(machine, user, script) {
        Ok(out) if out.contains("ready") => debug!("Intune daemon is ready"),
        _ => warn!("Intune daemon socket did not appear in time — launching portal anyway"),
    }
}

/// Wait for the named app to start, then block until it exits inside the
/// container. Returns `true` if the app was seen running (then exited), `false`
/// if it never started within the timeout.
///
/// Lets `enroll` launch the portal **detached** (background, reparented to the
/// container's PID 1, not tied to our terminal) yet still report success only
/// once the user closes the portal window — and report an honest failure if the
/// portal never appeared. Runs as a single in-container shell (one `machinectl`
/// spawn).
pub fn wait_for_app_exit(machine: &str, user: &str, process: &str) -> bool {
    let script = format!(
        r#"
# Wait (max 45s) for the app to actually start (dependencies are already up by
# the time we get here, so it normally appears within a couple of seconds).
appeared=0
for _ in $(seq 1 45); do
    if pgrep -u "$(id -u)" -x {proc} >/dev/null 2>&1; then
        appeared=1
        break
    fi
    sleep 1
done
if [ "$appeared" = 0 ]; then
    echo notstarted
    exit 0
fi
# Block until it exits (the user closing the window). Poll quickly so the shell
# resumes promptly after the window closes; pgrep in-container is cheap.
while pgrep -u "$(id -u)" -x {proc} >/dev/null 2>&1; do
    sleep 0.3
done
echo closed
"#,
        proc = process
    );
    matches!(run_in_container(machine, user, &script), Ok(out) if out.contains("closed"))
}

/// Execute a command in the foreground (interactive, attached to terminal).
pub fn exec_foreground(
    _machine: &str,
    _user: &str,
    uid: u32,
    command: &str,
    prelaunch: Option<&str>,
    display: &DisplayInfo,
) -> Result<()> {
    let session_script = build_session_script(uid, display, command, prelaunch, Launch::Foreground);

    let status = Command::new("sudo")
        .args([NSENTER_HELPER, "host", &session_script])
        .status()
        .context("Failed to nsenter into container")?;

    if !status.success() {
        anyhow::bail!("Command exited with status {}", status.code().unwrap_or(-1));
    }

    Ok(())
}

/// Open an interactive shell in the container via machinectl shell.
pub fn shell(machine: &str, user: &str) -> Result<()> {
    debug!(machine = %machine, user = %user, "Opening shell");

    let status = Command::new("machinectl")
        .args([
            "shell",
            &format!("{}@{}", user, machine),
            "/bin/bash",
            "--login",
        ])
        .status()
        .context("Failed to open shell in container")?;

    if !status.success() {
        anyhow::bail!("Shell exited with status {}", status.code().unwrap_or(-1));
    }

    Ok(())
}

/// Write display configuration into the container rootfs before boot.
/// This marker file is read by profile.d scripts inside the container.
///
/// In headless mode (`!with_display`) we write a marker that sets NO display
/// variables, so the identity broker (D-Bus activated) runs headless instead of
/// trying to open a non-existent display.
fn write_display_marker(config: &Config, display: &DisplayInfo, with_display: bool) -> Result<()> {
    let marker_path = config.rootfs_path.join("etc/intune-container-display");
    let mut content = String::new();

    if !with_display {
        content.push_str("# intune-container: headless mode — no display forwarded\n");
        let marker_str = marker_path.to_string_lossy().to_string();
        let mut child = Command::new("sudo")
            .args(["tee", &marker_str])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .context("Failed to write display marker")?;
        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            stdin.write_all(content.as_bytes())?;
        }
        child.wait()?;
        return Ok(());
    }

    if let Some(ref x11) = display.x11_display {
        content.push_str(&format!("export DISPLAY='{}'\n", x11));
    }

    if let Some(ref wayland) = display.wayland_socket {
        if let Some(name) = wayland.file_name() {
            content.push_str(&format!(
                "export WAYLAND_DISPLAY='{}'\n",
                name.to_string_lossy()
            ));
        }
    }

    if display.xauthority.is_some() {
        content.push_str("export XAUTHORITY='/tmp/.container-xauth'\n");
    } else if display.has_abstract_x11 {
        // No xauth needed for abstract sockets with xhost +local:
        content.push_str("# Abstract X11 socket — no Xauthority required\n");
    }

    // GDK backend — prefer x11 since container GTK is usually X11-only
    if display.x11_display.is_some() {
        content.push_str("export GDK_BACKEND='x11'\n");
    } else if display.wayland_socket.is_some() {
        content.push_str("export GDK_BACKEND='wayland'\n");
    }

    let marker_str = marker_path.to_string_lossy().to_string();

    let mut child = Command::new("sudo")
        .args(["tee", &marker_str])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("Failed to write display marker")?;

    if let Some(ref mut stdin) = child.stdin {
        use std::io::Write;
        stdin.write_all(content.as_bytes())?;
    }

    child.wait()?;
    Ok(())
}

/// Build the systemd-nspawn command-line arguments.
///
/// `with_display` decides whether the real host display (X11/Wayland/GPU) is
/// bound in. Headless (`false`) is the default for background/SSO use.
pub fn build_nspawn_args(
    config: &Config,
    display: &DisplayInfo,
    with_display: bool,
) -> Result<Vec<String>> {
    let mut args = Vec::new();

    // Basic machine configuration
    args.push(format!("--machine={}", config.machine_name));
    args.push(format!("-D{}", config.rootfs_path.display()));
    args.push("--boot".to_string());
    args.push("--console=pipe".to_string());
    // Copy the host's working resolver into the container so DNS survives every
    // boot. Without this the container can lose DNS (e.g. dangling resolved
    // stub symlink), which breaks device enrollment ("couldn't connect to
    // Microsoft services" / "couldn't enroll").
    args.push("--resolv-conf=replace-host".to_string());

    // Home directory bind mount
    let intune_home = format!(
        "{}/Intune",
        std::env::var("HOME").unwrap_or_else(|_| "/root".to_string())
    );
    let container_home = format!("/home/{}", config.host_user);

    // Create ~/Intune on host if it doesn't exist
    let _ = std::fs::create_dir_all(&intune_home);
    args.push(format!("--bind={}:{}", intune_home, container_home));

    // Persistent device-state bind mounts: keep device registration + agent
    // state OUTSIDE the rootfs so `init --force` / rebuilds never wipe the
    // enrollment. (Dirs are created root-owned by ensure_persistent_dirs.)
    for (sub, container_path) in device_state_mounts() {
        let host_dir = format!("{}/{}", PERSISTENT_STATE_DIR, sub);
        args.push(format!("--bind={}:{}", host_dir, container_path));
    }

    // Display/audio forwarding — skipped entirely in headless mode for max
    // isolation (the container gets no window into your screen).
    if with_display {
        // X11 sockets (covers both file-based and abstract socket passthrough)
        let x11_dir = std::path::Path::new("/tmp/.X11-unix");
        if x11_dir.exists() {
            args.push("--bind=/tmp/.X11-unix".to_string());
        }

        // Bind individual sockets — NOT the entire runtime dir.
        // The container has its own /run/user/UID with D-Bus session bus,
        // gnome-keyring, etc. Overwriting it with the host's kills those services.
        if let Some(runtime_dir) = DisplayInfo::xdg_runtime_dir() {
            // Wayland socket → /run/host-wayland
            if let Some(ref wayland_socket) = display.wayland_socket {
                args.push(format!(
                    "--bind={}:/run/host-wayland",
                    wayland_socket.display()
                ));
            }

            // PipeWire → /run/host-pipewire
            let pipewire = runtime_dir.join("pipewire-0");
            if pipewire.exists() {
                args.push(format!("--bind={}:/run/host-pipewire", pipewire.display()));
            }

            // PulseAudio → /run/host-pulse
            let pulse = runtime_dir.join("pulse/native");
            if pulse.exists() {
                args.push(format!("--bind={}:/run/host-pulse", pulse.display()));
            }
        }
    } else {
        debug!("Headless mode: skipping display/audio forwarding");
    }

    // Bus exposure: make the container's session bus reachable from the host so
    // the SSO native messaging host can talk to com.microsoft.identity.broker1.
    // We bind a host directory to the container's /run/user/<uid>.
    if config.expose_bus {
        let runtime_dir = config.broker_runtime_dir()?;
        let _ = std::fs::create_dir_all(&runtime_dir);
        // XDG_RUNTIME_DIR MUST be mode 0700 or gnome-keyring/dbus refuse to use
        // it correctly (this was the bug that broke the keyring/enrollment).
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&runtime_dir) {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(&runtime_dir, perms);
        }
        args.push(format!(
            "--bind={}:/run/user/{}",
            runtime_dir.display(),
            config.host_uid
        ));
        debug!(
            host_dir = %runtime_dir.display(),
            "Exposing container session bus to host"
        );
    }

    // GPU access — bind ALL DRI devices dynamically (skipped in headless mode)
    if with_display && std::path::Path::new("/dev/dri").exists() {
        args.push("--bind=/dev/dri".to_string());
        // Grant rwm for all card and render devices
        if let Ok(entries) = std::fs::read_dir("/dev/dri") {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("card") || name.starts_with("renderD") {
                        args.push(format!("--property=DeviceAllow={} rwm", path.display()));
                    }
                }
            }
        }
    }

    // Nvidia devices (skipped in headless mode)
    if with_display {
        for dev in &[
            "/dev/nvidia0",
            "/dev/nvidiactl",
            "/dev/nvidia-modeset",
            "/dev/nvidia-uvm",
        ] {
            if std::path::Path::new(dev).exists() {
                args.push(format!("--bind={}", dev));
                args.push(format!("--property=DeviceAllow={} rwm", dev));
            }
        }
    }

    // Display environment variables (only meaningful with display forwarding)
    if with_display {
        if let Some(ref x11) = display.x11_display {
            args.push(format!("--setenv=DISPLAY={}", x11));
        }
        if display.wayland_socket.is_some() {
            args.push("--setenv=WAYLAND_DISPLAY=/run/host-wayland".to_string());
        }
    }

    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_display() -> DisplayInfo {
        DisplayInfo {
            wayland_socket: None,
            x11_display: Some(":0".to_string()),
            xauthority: None,
            has_abstract_x11: false,
        }
    }

    /// Background launches must detach (`nohup … &`) so the process survives the
    /// nsenter call returning.
    #[test]
    fn background_launch_detaches() {
        let script = build_session_script(
            1000,
            &test_display(),
            "intune-portal",
            None,
            Launch::Background,
        );
        assert!(script.contains("nohup intune-portal >/dev/null 2>&1 &"));
        assert!(!script.contains("exec intune-portal"));
    }

    /// Foreground launches (`open -v`) must `exec` so output stays attached to
    /// the terminal. Regression guard for the old string-replace approach that
    /// would silently no-op if the launcher line ever changed.
    #[test]
    fn foreground_launch_execs() {
        let script = build_session_script(
            1000,
            &test_display(),
            "intune-portal",
            None,
            Launch::Foreground,
        );
        assert!(script.contains("exec intune-portal"));
        assert!(!script.contains("nohup intune-portal"));
    }

    /// The prelaunch hook must be emitted before the launch line.
    #[test]
    fn prelaunch_runs_before_command() {
        let script = build_session_script(
            1000,
            &test_display(),
            "microsoft-edge-stable",
            Some("rm -f /tmp/lock"),
            Launch::Background,
        );
        let pre = script.find("rm -f /tmp/lock").expect("prelaunch present");
        let launch = script
            .find("nohup microsoft-edge-stable")
            .expect("launch present");
        assert!(pre < launch, "prelaunch must precede the launch line");
    }

    /// The headless virtual display MUST start Xvfb detached with `nohup … &` so
    /// it survives the nsenter helper returning (reparented to PID 1). Starting
    /// it inside a `machinectl shell` scope — or relying on bare `setsid` — would
    /// let systemd kill Xvfb when the shell exits, which broke headless SSO.
    #[test]
    fn virtual_display_starts_xvfb_detached() {
        let script = build_virtual_display_script(1000);
        assert!(
            script.contains("nohup Xvfb :99"),
            "Xvfb must be launched detached via nohup so it persists"
        );
        assert!(
            !script.contains("machinectl"),
            "must not start Xvfb through a transient machinectl shell scope"
        );
        assert!(
            !script.contains("setsid Xvfb"),
            "bare setsid is not enough; use the nsenter helper + nohup"
        );
    }

    /// The headless display script must publish DISPLAY=:99 to the D-Bus
    /// activation environment, or the on-demand-activated GTK broker has no
    /// display and replies NoReply ("Device unknown").
    #[test]
    fn virtual_display_publishes_activation_env() {
        let script = build_virtual_display_script(1000);
        assert!(script.contains("export DISPLAY=:99"));
        assert!(script.contains("dbus-update-activation-environment"));
        // The session bus address is wired from the uid argument.
        assert!(script.contains("/run/user/1000/bus"));
    }
}
