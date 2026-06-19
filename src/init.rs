//! Container initialization: pulls the OCI image and extracts rootfs.
//!
//! Uses docker (or podman) to pull the base image, exports it to a tar, and
//! extracts into the rootfs directory.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::Config;

/// Path to the nsenter helper script on the host.
const NSENTER_HELPER_PATH: &str = "/usr/local/libexec/intune-container/nsenter-exec";
/// Path to the sudoers rule on the host.
const SUDOERS_PATH: &str = "/etc/sudoers.d/intune-container";

/// Marker embedded in the nsenter helper to identify its version. Bump this
/// whenever the helper's behaviour changes so `boot()` can auto-reinstall a
/// stale helper after a binary upgrade (the old one has a different arg ABI).
const HELPER_VERSION_MARKER: &str = "intune-container-helper-v3";

/// Default container image. Build the derived image (base + xvfb) from the
/// repo `Dockerfile` with `just build-image`, push it to your registry, then
/// hardcode that URL here so headless background SSO has a private display.
/// Override per-run with `--image`.
const DEFAULT_IMAGE: &str = "ghcr.io/magicabdel/intune-container:latest";

/// Initialize the container rootfs from an OCI image.
pub fn initialize(config: &Config, password: &str, image: Option<&str>) -> Result<()> {
    let rootfs = &config.rootfs_path;
    let image = image.unwrap_or(DEFAULT_IMAGE);

    if rootfs.exists() {
        anyhow::bail!(
            "Rootfs already exists at {}. Use `destroy` first, or `--force` to overwrite.",
            rootfs.display()
        );
    }

    // Step 1: Pull the OCI image
    info!("Pulling OCI image: {}", image);
    pull_image(image)?;

    // Step 2: Create a temporary container and export the filesystem
    debug!("Exporting filesystem...");
    let tar_path = export_image(image)?;

    // Step 3: Create rootfs directory and extract
    debug!("Extracting to {}...", rootfs.display());
    extract_rootfs(&tar_path, rootfs)?;

    // Step 4: Provision the container user
    debug!("Provisioning container user: {}", config.host_user);
    provision_user(rootfs, &config.host_user, config.host_uid, password)?;

    // Step 5: Install session setup scripts
    debug!("Installing session scripts...");
    install_session_scripts(rootfs, &config.host_user, config.host_uid)?;

    // Step 6: Install sudoers rule and nsenter helper (host-side)
    debug!("Installing passwordless exec rule...");
    install_sudoers_and_helper(config)?;

    // Cleanup temporary tar
    let _ = std::fs::remove_file(&tar_path);

    info!("Initialization complete!");
    Ok(())
}

/// Check if the sudoers rule and nsenter helper are both installed on the host.
pub fn is_sudoers_installed(_config: &Config) -> bool {
    Path::new(NSENTER_HELPER_PATH).exists() && Path::new(SUDOERS_PATH).exists()
}

/// Check whether the installed nsenter helper matches the current version.
///
/// After a binary upgrade the on-disk helper may use an older argument ABI
/// (e.g. it expected a caller-supplied PID). Comparing the embedded version
/// marker lets `boot()` reinstall a stale helper exactly once. The helper is
/// world-readable (mode 755), so this needs no privileges.
pub fn is_helper_current() -> bool {
    std::fs::read_to_string(NSENTER_HELPER_PATH)
        .map(|s| s.contains(HELPER_VERSION_MARKER))
        .unwrap_or(false)
}

/// Install the passwordless sudoers rule and nsenter-exec helper script on the host.
///
/// This enables `sudo /usr/local/libexec/intune-container/nsenter-exec` without a password,
/// which is required for the nsenter-based exec approach.
pub fn install_sudoers_and_helper(config: &Config) -> Result<()> {
    let user = &config.host_user;

    // 1. Create the helper script directory
    let helper_dir = Path::new(NSENTER_HELPER_PATH).parent().unwrap();
    let status = Command::new("sudo")
        .args(["mkdir", "-p", &helper_dir.to_string_lossy()])
        .status()
        .context("Failed to create nsenter helper directory")?;
    if !status.success() {
        anyhow::bail!("Failed to create {}", helper_dir.display());
    }

    // 2. Write the nsenter-exec helper script
    //
    // SECURITY: the target PID is resolved *here* from the fixed machine name,
    // never taken from the caller. The earlier design accepted an arbitrary PID,
    // which let any local process enter any process's namespaces via the
    // passwordless sudo rule. The caller now controls only the IPC mode ($1)
    // and the script ($2), which always runs as the unprivileged container user
    // inside the container — equivalent to `machinectl shell`, no escalation.
    //
    // IPC mode: "host" omits `-i`, leaving the process in the host IPC namespace
    // so X MIT-SHM works against the host's X server (XWayland). Display GUI
    // apps (the enroll portal, edge) need this or they crash on startup with an
    // X11 BadAccess (MIT-SHM) error. Anything else keeps the container's private
    // IPC namespace (used by the headless Xvfb path for full isolation).
    let helper_content = format!(
        r#"#!/bin/bash
# Installed by intune-container. Enters the intune container's namespaces and
# runs the given script as the container user via a non-login bash.
# Invoked through passwordless sudo (the intune-container sudoers rule).
# Args: $1 = IPC mode ("host" = share host IPC ns for X MIT-SHM; else private),
#       $2 = the script to run.
# {marker}
set -euo pipefail
MACHINE="{machine}"
LEADER="$(machinectl show "$MACHINE" -p Leader --value 2>/dev/null || true)"
if [ -z "$LEADER" ] || [ "$LEADER" = "0" ]; then
    echo "intune-container: machine $MACHINE is not running" >&2
    exit 1
fi
if [ "${{1:-}}" = "host" ]; then
    _ipc=""
else
    _ipc="-i"
fi
exec /usr/bin/nsenter -t "$LEADER" -m -u $_ipc -n -p -- /bin/su -s /bin/bash {user} -c "$2"
"#,
        marker = HELPER_VERSION_MARKER,
        machine = config.machine_name,
        user = user
    );

    debug!(path = NSENTER_HELPER_PATH, "Writing nsenter helper script");
    sudo_write_file(Path::new(NSENTER_HELPER_PATH), &helper_content, "755")?;

    // Ensure root ownership
    let _ = Command::new("sudo")
        .args(["chown", "root:root", NSENTER_HELPER_PATH])
        .status();

    // 3. Write the sudoers rule
    let sudoers_content = format!(
        "# Allow {user} to run the intune-container nsenter helper without password\n\
         {user} ALL=(root) NOPASSWD: {helper}\n",
        user = user,
        helper = NSENTER_HELPER_PATH
    );

    debug!(path = SUDOERS_PATH, "Writing sudoers rule");
    sudo_write_file(Path::new(SUDOERS_PATH), &sudoers_content, "0440")?;

    // Ensure root ownership on sudoers file
    let _ = Command::new("sudo")
        .args(["chown", "root:root", SUDOERS_PATH])
        .status();

    debug!("Sudoers rule and nsenter helper installed");
    Ok(())
}

/// Remove the host-side passwordless-sudo integration: the sudoers rule and the
/// nsenter helper (plus its directory). Best-effort — it does not fail if a
/// piece is already gone. Used by `destroy` so teardown never leaves a dangling
/// `NOPASSWD` rule pointing at a deleted helper.
pub fn uninstall_sudoers_and_helper() {
    let helper_dir = Path::new(NSENTER_HELPER_PATH)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/local/libexec/intune-container".to_string());

    let _ = Command::new("sudo")
        .args(["rm", "-f", SUDOERS_PATH])
        .status();
    let _ = Command::new("sudo")
        .args(["rm", "-rf", &helper_dir])
        .status();
    debug!("Removed sudoers rule and nsenter helper");
}

/// The container engine to use for pulling/exporting the image.
///
/// We prefer `docker` (what most people have/expect) and fall back to `podman`.
/// NOTE: a shell `alias docker=podman` is invisible here because we exec the
/// binary directly, so we look for a real `docker` executable on PATH and
/// otherwise use `podman`.
fn container_engine() -> &'static str {
    let on_path = |bin: &str| {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|p| p.join(bin).is_file()))
            .unwrap_or(false)
    };
    if on_path("docker") {
        "docker"
    } else {
        "podman"
    }
}

/// Pull an OCI image using the detected container engine.
fn pull_image(image: &str) -> Result<()> {
    let engine = container_engine();

    // Skip the pull if the image is already present locally. Essential for
    // locally-built images like `localhost/intune-container:local` — pulling
    // those would try to reach a bogus "localhost" registry and fail.
    let present = Command::new(engine)
        .args(["image", "inspect", image])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if present {
        debug!("Image already present locally; skipping pull: {image}");
        return Ok(());
    }

    let status = Command::new(engine)
        .args(["pull", image])
        .status()
        .with_context(|| format!("Failed to run `{engine} pull`. Is docker/podman installed?"))?;

    if !status.success() {
        anyhow::bail!(
            "{engine} pull failed with status {}",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// Export an OCI image to a tar file. Returns the path to the tar.
fn export_image(image: &str) -> Result<String> {
    let engine = container_engine();
    let tmp_dir = std::env::var("TMPDIR")
        .or_else(|_| std::env::var("XDG_RUNTIME_DIR"))
        .unwrap_or_else(|_| "/var/tmp".to_string());

    // Unique per-process names so concurrent/interrupted runs don't collide on a
    // shared temp container or tar file.
    let pid = std::process::id();
    let tar_path = format!("{}/intune-container-rootfs-{}.tar", tmp_dir, pid);
    let container_name = format!("intune-container-export-{}", pid);

    // Remove a leftover temp container of the same name, if any.
    let _ = Command::new(engine)
        .args(["rm", "-f", &container_name])
        .output();

    // Create container (don't start it)
    let status = Command::new(engine)
        .args(["create", "--name", &container_name, image, "/bin/true"])
        .status()
        .context("Failed to create temporary container")?;

    if !status.success() {
        anyhow::bail!("{engine} create failed");
    }

    // Export filesystem
    let status = Command::new(engine)
        .args(["export", "-o", &tar_path, &container_name])
        .status()
        .context("Failed to export container filesystem")?;

    // Cleanup temp container
    let _ = Command::new(engine)
        .args(["rm", "-f", &container_name])
        .output();

    if !status.success() {
        // Don't leave a partial tar behind on failure.
        let _ = std::fs::remove_file(&tar_path);
        anyhow::bail!("{engine} export failed");
    }

    Ok(tar_path)
}

/// Extract a tar file into the rootfs directory using sudo.
fn extract_rootfs(tar_path: &str, rootfs: &Path) -> Result<()> {
    let rootfs_str = rootfs.to_string_lossy();

    // Create the rootfs directory
    let status = Command::new("sudo")
        .args(["mkdir", "-p", &rootfs_str])
        .status()
        .context("Failed to create rootfs directory")?;

    if !status.success() {
        anyhow::bail!("Failed to create rootfs directory");
    }

    // Extract the tar
    let status = Command::new("sudo")
        .args(["tar", "-xf", tar_path, "-C", &rootfs_str])
        .status()
        .context("Failed to extract rootfs tar")?;

    if !status.success() {
        anyhow::bail!("Failed to extract rootfs");
    }

    Ok(())
}

/// Create the container user matching the host user.
fn provision_user(rootfs: &Path, user: &str, uid: u32, password: &str) -> Result<()> {
    let rootfs_str = rootfs.to_string_lossy();

    // Use full PATH inside chroot since /usr/sbin may not be in default PATH
    let path_env = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

    // Check if our user already exists, or if another user has our UID.
    // The ubuntu-intune image ships with a pre-created user (often UID 1000).
    let user_script = format!(
        r#"export {path}
# Check if target user already exists
if id -u {user} 2>/dev/null; then
    echo "USER_EXISTS"
    exit 0
fi
# Check if another user has our UID
EXISTING=$(getent passwd {uid} | cut -d: -f1)
if [ -n "$EXISTING" ] && [ "$EXISTING" != "{user}" ]; then
    echo "RENAMING $EXISTING to {user}"
    usermod -l {user} "$EXISTING"
    usermod -d /home/{user} -m {user} 2>/dev/null || true
    groupmod -n {user} "$EXISTING" 2>/dev/null || true
    exit 0
fi
# No existing user — create fresh
useradd -m -u {uid} -s /bin/bash {user}
"#,
        path = path_env,
        user = user,
        uid = uid
    );

    let output = Command::new("sudo")
        .args(["chroot", &rootfs_str, "/bin/bash", "-c", &user_script])
        .output()
        .context("Failed to provision container user")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        debug!("User provisioning: {}", stdout.trim());
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("User provisioning had issues: {}", stderr.trim());
    }

    // Set the password. CRITICAL: the password must NOT appear in process
    // arguments (argv is world-readable via /proc/<pid>/cmdline), so we feed it
    // on stdin and read it inside the chroot. Primary path hashes with openssl
    // and sets it via usermod (chpasswd needs PAM + /proc, which a plain chroot
    // lacks). The script reads one line from stdin as the password.
    let hash_script = format!(
        r#"export {path}
IFS= read -r PW
HASH=$(printf '%s' "$PW" | openssl passwd -6 -stdin) || exit 1
usermod -p "$HASH" {user}
"#,
        path = path_env,
        user = user,
    );

    let ok =
        chroot_with_stdin(&rootfs_str, &hash_script, password).context("Failed to set password")?;

    if !ok {
        // Fallback: mount /proc and use chpasswd (still password-via-stdin).
        warn!("Direct password hash failed, trying with /proc mounted");
        let proc_path = format!("{}/proc", rootfs_str);
        let _ = Command::new("sudo")
            .args(["mount", "-t", "proc", "proc", &proc_path])
            .status();

        let chpasswd_script = format!(
            r#"export {path}
IFS= read -r PW
printf '%s:%s\n' "{user}" "$PW" | chpasswd
"#,
            path = path_env,
            user = user,
        );
        let ok = chroot_with_stdin(&rootfs_str, &chpasswd_script, password).unwrap_or(false);

        let _ = Command::new("sudo").args(["umount", &proc_path]).status();

        if !ok {
            warn!("Password setting failed — you can set it manually with: intune-container shell, then: passwd");
        }
    }

    // Add user to required groups
    let groups_script = format!(
        "export {path}; usermod -aG sudo,video,render,plugdev {user} 2>/dev/null || true",
        path = path_env,
        user = user
    );

    let _ = Command::new("sudo")
        .args(["chroot", &rootfs_str, "/bin/bash", "-c", &groups_script])
        .status();

    Ok(())
}

/// Install session setup scripts inside the container rootfs.
/// These scripts configure DISPLAY, WAYLAND_DISPLAY, audio, keyring, etc.
fn install_session_scripts(rootfs: &Path, user: &str, uid: u32) -> Result<()> {
    let script_content = format!(
        r#"#!/bin/bash
# intune-container session setup
# Sources display and audio environment for GUI applications.

export XDG_RUNTIME_DIR="/run/user/{uid}"
export DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/{uid}/bus"

# Read display marker if available
if [ -f /etc/intune-container-display ]; then
    source /etc/intune-container-display
fi

# PipeWire/PulseAudio
if [ -S "/run/user/{uid}/pipewire-0" ]; then
    export PIPEWIRE_REMOTE="/run/user/{uid}/pipewire-0"
fi
if [ -S "/run/user/{uid}/pulse/native" ]; then
    export PULSE_SERVER="unix:/run/user/{uid}/pulse/native"
fi

# Unlock gnome-keyring (auto-unlock with empty password)
if command -v gnome-keyring-daemon >/dev/null 2>&1; then
    echo -n "" | gnome-keyring-daemon --start --unlock --components=secrets,pkcs11 2>/dev/null || true
    export GNOME_KEYRING_CONTROL
    export SSH_AUTH_SOCK
fi

# Push all env into D-Bus activation environment (for identity broker)
if command -v dbus-update-activation-environment >/dev/null 2>&1; then
    dbus-update-activation-environment DISPLAY XAUTHORITY WAYLAND_DISPLAY \\
        GDK_BACKEND XDG_RUNTIME_DIR GNOME_KEYRING_CONTROL SSH_AUTH_SOCK 2>/dev/null || true
fi
"#,
        uid = uid
    );

    let script_path = rootfs.join("usr/local/bin/intune-container-session-setup");
    let profile_path = rootfs.join("etc/profile.d/99-intune-container.sh");

    // Write session setup script
    sudo_write_file(&script_path, &script_content, "755")?;

    // Write profile.d entry that sources the session setup
    let profile_content = r#"# intune-container: source session setup
if [ -x /usr/local/bin/intune-container-session-setup ]; then
    source /usr/local/bin/intune-container-session-setup
fi
"#;

    sudo_write_file(&profile_path, profile_content, "644")?;

    // Create keyring directory and set up auto-unlock keyring for the user
    setup_keyring(rootfs, user, uid)?;

    Ok(())
}

/// Set up gnome-keyring with an auto-unlock "login" keyring.
/// This creates the keyring directory and a PAM config that auto-unlocks on login.
fn setup_keyring(rootfs: &Path, user: &str, uid: u32) -> Result<()> {
    let rootfs_str = rootfs.to_string_lossy();
    let path_env = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

    // Create keyring directory with correct ownership
    let keyring_script = format!(
        r#"export {path}
mkdir -p /home/{user}/.local/share/keyrings
chown -R {uid}:{uid} /home/{user}/.local/share/keyrings
chmod 700 /home/{user}/.local/share/keyrings
"#,
        path = path_env,
        user = user,
        uid = uid
    );

    let _ = Command::new("sudo")
        .args(["chroot", &rootfs_str, "/bin/bash", "-c", &keyring_script])
        .status();

    // Ensure PAM is configured to auto-unlock gnome-keyring on login.
    // This makes `machinectl shell` (login session) auto-unlock the keyring.
    let pam_script = format!(
        r#"export {path}
# Add gnome-keyring to PAM login if not already present
if [ -f /etc/pam.d/login ] && ! grep -q pam_gnome_keyring /etc/pam.d/login; then
    echo 'auth     optional  pam_gnome_keyring.so' >> /etc/pam.d/login
    echo 'session  optional  pam_gnome_keyring.so auto_start' >> /etc/pam.d/login
fi
"#,
        path = path_env
    );

    let _ = Command::new("sudo")
        .args(["chroot", &rootfs_str, "/bin/bash", "-c", &pam_script])
        .status();

    debug!("Keyring directory created and PAM configured for auto-unlock");
    Ok(())
}

/// Write a file inside the rootfs using sudo (since rootfs is root-owned).
fn sudo_write_file(path: &Path, content: &str, mode: &str) -> Result<()> {
    let path_str = path.to_string_lossy();

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        let _ = Command::new("sudo")
            .args(["mkdir", "-p", &parent.to_string_lossy()])
            .status();
    }

    // Write content via sudo tee
    let mut child = Command::new("sudo")
        .args(["tee", &path_str])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to write {}", path_str))?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(content.as_bytes())?;
    }

    child.wait()?;

    // Set permissions
    let _ = Command::new("sudo")
        .args(["chmod", mode, &path_str])
        .status();

    Ok(())
}

/// Run a script inside the rootfs via `sudo chroot`, feeding `stdin_data` to its
/// standard input. Used to pass the user password WITHOUT exposing it in
/// process arguments (which are world-readable via /proc/<pid>/cmdline).
/// Returns whether the script exited successfully.
fn chroot_with_stdin(rootfs_str: &str, script: &str, stdin_data: &str) -> Result<bool> {
    let mut child = Command::new("sudo")
        .args(["chroot", rootfs_str, "/bin/bash", "-c", script])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn chroot for password setup")?;

    {
        // Write the secret then drop stdin to send EOF before waiting.
        let mut stdin = child.stdin.take().context("Failed to open chroot stdin")?;
        stdin.write_all(stdin_data.as_bytes())?;
        stdin.write_all(b"\n")?;
    }

    let status = child.wait().context("Failed to wait for chroot")?;
    Ok(status.success())
}
