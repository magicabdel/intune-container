//! Pure-Rust provisioning for the rootless backend (Phase 2).
//!
//! The published image already ships a usable user (`ubuntu:1000`), the Intune
//! portal, and gnome-keyring, and boots the broker without help. What it does
//! *not* provide is the per-user session setup that `machinectl`'s PAM login
//! gives the nspawn backend for free: a session profile that unlocks the keyring
//! and exports the display/D-Bus activation environment, plus a runtime
//! `XDG_RUNTIME_DIR` and correct home ownership.
//!
//! This module fills those gaps with plain file writes (no `Command::new`,
//! no chroot): static provisioning happens at `init` time into the rootfs we
//! own, and the runtime setup script is handed to the running container's root
//! via `setns` exec.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use tracing::debug;

/// The in-container user GUI apps run as.
#[derive(Debug, Clone)]
pub struct ContainerUser {
    pub uid: u32,
    pub name: String,
    pub home: String,
}

/// Find the image's primary login user by scanning the rootfs `/etc/passwd` for
/// the first regular account (uid in `1000..60000` with a real shell). Falls
/// back to the Ubuntu image default.
pub fn detect_user(rootfs: &Path) -> ContainerUser {
    let fallback = ContainerUser {
        uid: 1000,
        name: "ubuntu".into(),
        home: "/home/ubuntu".into(),
    };
    let passwd = match std::fs::read_to_string(rootfs.join("etc/passwd")) {
        Ok(s) => s,
        Err(_) => return fallback,
    };
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 {
            continue;
        }
        let uid: u32 = match f[2].parse() {
            Ok(u) => u,
            Err(_) => continue,
        };
        let shell = f[6];
        if (1000..60000).contains(&uid) && (shell.ends_with("sh")) {
            return ContainerUser {
                uid,
                name: f[0].to_string(),
                home: f[5].to_string(),
            };
        }
    }
    fallback
}

/// Static provisioning at `init` time: install the session profile and seed the
/// keyring directory for the given session `user`. Operates on the rootfs files
/// directly (we own them after the rootless OCI extraction).
pub fn provision(rootfs: &Path, user: &ContainerUser) -> Result<()> {
    debug!(user = %user.name, uid = user.uid, home = %user.home, "provisioning rootless session");
    install_session_profile(rootfs, user)?;
    // Best-effort: the home may already be owned by a container subuid (after a
    // prior boot chowned it), so we can't always write here from the host. The
    // runtime setup script recreates the keyring dir as the container's root.
    if let Err(e) = seed_keyring_dir(rootfs, user) {
        debug!("keyring dir seed skipped ({e:#}); will be created at runtime");
    }
    Ok(())
}

/// Write a profile.d entry (sourced by the `bash -lc` we launch apps with) that
/// exports the session environment. The heavy session/keyring setup is done once
/// per boot by [`runtime_setup_script`] (run as root before any app launches);
/// this just points the app at the already-running bus and keyring.
fn install_session_profile(rootfs: &Path, user: &ContainerUser) -> Result<()> {
    let script = format!(
        r#"#!/bin/sh
# intune-container (rootless) session setup — sourced for login shells.
: "${{XDG_RUNTIME_DIR:=/run/user/{uid}}}"
export XDG_RUNTIME_DIR
: "${{DBUS_SESSION_BUS_ADDRESS:=unix:path=/run/user/{uid}/bus}}"
export DBUS_SESSION_BUS_ADDRESS
export HOME="{home}"
export GNOME_KEYRING_CONTROL="/run/user/{uid}/keyring"
export SSH_AUTH_SOCK="/run/user/{uid}/keyring/ssh"

# WebKitGTK auth browser: fall back to software rendering if GPU is unavailable.
export WEBKIT_DISABLE_COMPOSITING_MODE=1

# Make the display + secrets env available to D-Bus-activated services.
if command -v dbus-update-activation-environment >/dev/null 2>&1; then
    dbus-update-activation-environment --systemd \
        DISPLAY XAUTHORITY WAYLAND_DISPLAY GDK_BACKEND XDG_RUNTIME_DIR \
        GNOME_KEYRING_CONTROL SSH_AUTH_SOCK >/dev/null 2>&1 || true
fi
"#,
        uid = user.uid,
        home = user.home,
    );

    let profile_dir = rootfs.join("etc/profile.d");
    std::fs::create_dir_all(&profile_dir).context("create /etc/profile.d")?;
    let profile = profile_dir.join("99-intune-container.sh");
    std::fs::write(&profile, script).context("write session profile")?;
    set_mode(&profile, 0o644)?;
    Ok(())
}

/// Create the user's keyring directory and mark the "login" collection default,
/// so gnome-keyring auto-unlocks it instead of prompting. Ownership is corrected
/// at runtime (the container's root chowns the home tree).
fn seed_keyring_dir(rootfs: &Path, user: &ContainerUser) -> Result<()> {
    let rel = user.home.trim_start_matches('/');
    let keyrings = rootfs.join(rel).join(".local/share/keyrings");
    std::fs::create_dir_all(&keyrings).context("create keyrings dir")?;
    let default = keyrings.join("default");
    if !default.exists() {
        std::fs::write(&default, "login\n").context("write default keyring marker")?;
    }
    Ok(())
}

/// The shell script the running container's root runs (via `setns`) once per
/// boot to recreate the session a PAM login would, so Intune works end to end:
/// the per-user systemd manager (for `XDG_RUNTIME_DIR` + D-Bus session bus),
/// an unlocked gnome-keyring with a created `login` collection, the Intune
/// **compliance agent timer** (without it the device never reports compliant),
/// and a broker restart so it re-reads the keyring. Mirrors the nspawn session.
pub fn runtime_setup_script(user: &ContainerUser, headless: bool) -> String {
    // Headless (background SSO): the identity broker is a GTK app that exits
    // without a display, so start a private Xvfb and publish DISPLAY into the
    // user D-Bus activation environment (where the on-demand broker inherits it).
    let display_block = if headless {
        r#"if command -v Xvfb >/dev/null 2>&1; then
    pgrep -x Xvfb >/dev/null 2>&1 || setsid Xvfb :99 -screen 0 640x480x16 -nolisten tcp >/tmp/intune-xvfb.log 2>&1 &
    for _ in $(seq 1 20); do [ -S /tmp/.X11-unix/X99 ] && break; sleep 0.3; done
    export DISPLAY=:99 GDK_BACKEND=x11
    systemctl --user import-environment DISPLAY GDK_BACKEND >/dev/null 2>&1 || true
    dbus-update-activation-environment --systemd DISPLAY GDK_BACKEND >/dev/null 2>&1 || true
fi
"#
    } else {
        ""
    };
    format!(
        r#"# Wait for the system to finish booting (logind + system bus).
for _ in $(seq 1 90); do
    state=$(systemctl is-system-running 2>/dev/null || true)
    case "$state" in ""|starting|initializing) sleep 1 ;; *) break ;; esac
done

export XDG_RUNTIME_DIR=/run/user/{uid}
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus
export HOME="{home}"

# Start the per-user systemd manager: it provides /run/user/{uid} and the D-Bus
# session bus, and is what runs the Intune agent timer as a user unit.
loginctl enable-linger {name} 2>/dev/null || true
systemctl start user@{uid}.service 2>/dev/null || true
for _ in $(seq 1 40); do [ -S "$XDG_RUNTIME_DIR/bus" ] && break; sleep 0.5; done

# Fallback: if the user manager didn't provide a bus, start one ourselves.
if [ ! -S "$XDG_RUNTIME_DIR/bus" ] && command -v dbus-daemon >/dev/null 2>&1; then
    install -d -m 700 -o {uid} -g {uid} "$XDG_RUNTIME_DIR" 2>/dev/null || true
    setsid dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --nofork --nopidfile --syslog-only >/dev/null 2>&1 &
    for _ in $(seq 1 20); do [ -S "$XDG_RUNTIME_DIR/bus" ] && break; sleep 0.2; done
fi

install -d -m 700 -o {uid} -g {uid} "{home}/.local" "{home}/.local/share" "{home}/.local/share/keyrings" "{home}/.config" 2>/dev/null || true
[ -f "{home}/.local/share/keyrings/default" ] || echo login > "{home}/.local/share/keyrings/default"

# Unlock gnome-keyring with an EMPTY password (note: `echo ""` sends a newline =
# empty password; `printf ''` would send EOF = no password and do nothing) and
# force-create the default "login" collection so the broker can store secrets.
if command -v gnome-keyring-daemon >/dev/null 2>&1; then
    _locked=$(busctl --user get-property org.freedesktop.secrets \
        /org/freedesktop/secrets/collection/login \
        org.freedesktop.Secret.Collection Locked 2>/dev/null)
    if [ "$_locked" != "b false" ]; then
        pkill -x gnome-keyring-d 2>/dev/null || true
        sleep 1
        echo "" | gnome-keyring-daemon --unlock --components=secrets,pkcs11 -d >/dev/null 2>&1 || true
        sleep 1
    fi
    if ! secret-tool lookup _keyring_init _keyring_init >/dev/null 2>&1; then
        echo init | secret-tool store --label="Keyring Init" _keyring_init _keyring_init >/dev/null 2>&1 || true
    fi
fi

# Make the session env available to user units + D-Bus-activated services.
systemctl --user import-environment XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS DISPLAY WAYLAND_DISPLAY >/dev/null 2>&1 || true
{display_block}
# Start the Intune compliance agent timer. WITHOUT THIS the device never reports
# compliant (the agent only runs as a user unit, gated on a graphical session).
systemctl --user start intune-agent.timer >/dev/null 2>&1 || true
systemctl --user start intune-agent.service >/dev/null 2>&1 || true

# Restart the device broker so it re-reads the now-initialized keyring.
systemctl restart microsoft-identity-device-broker.service >/dev/null 2>&1 || true
exit 0
"#,
        uid = user.uid,
        home = user.home,
        name = user.name,
        display_block = display_block,
    )
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_user_parses_passwd() {
        let dir = std::env::temp_dir().join(format!("itc-prov-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::fs::write(
            dir.join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/bash\n\
             daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
             ubuntu:x:1000:1000:Ubuntu:/home/ubuntu:/bin/bash\n",
        )
        .unwrap();
        let u = detect_user(&dir);
        assert_eq!(u.uid, 1000);
        assert_eq!(u.name, "ubuntu");
        assert_eq!(u.home, "/home/ubuntu");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_user_falls_back_without_passwd() {
        let u = detect_user(Path::new("/nonexistent-rootfs-xyz"));
        assert_eq!(u.uid, 1000);
        assert_eq!(u.name, "ubuntu");
    }

    #[test]
    fn runtime_script_sets_up_session_and_keyring() {
        let u = ContainerUser {
            uid: 0,
            name: "root".into(),
            home: "/root".into(),
        };
        let s = runtime_setup_script(&u, true);
        assert!(s.contains("/run/user/0"));
        assert!(s.contains("keyrings/default"));
        assert!(s.contains("gnome-keyring-daemon --unlock"));
        assert!(s.contains("secret-tool store"));
        assert!(s.contains("systemctl restart microsoft-identity-device-broker.service"));
        assert!(s.contains("intune-agent.timer"));
        assert!(s.contains("user@0.service"));
        assert!(s.contains("Xvfb :99"));
        // Display-forwarded variant should NOT start Xvfb.
        assert!(!runtime_setup_script(&u, false).contains("Xvfb"));
    }
}
