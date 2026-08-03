//! Boot autostart via a systemd **user** service.
//!
//! Users expect the SSO container to be up whenever they are logged in — and to
//! come back on its own after a reboot — without remembering to run
//! `intune-container start`. This module installs a `systemd --user` unit that
//! runs `start` on login and `stop` on logout, and enables **lingering** so the
//! unit also starts at boot before any interactive session exists.
//!
//! Everything here is idempotent and reversible: `enable` can be run repeatedly,
//! and `disable` removes exactly what `enable` created.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// The unit name, used both as the file name and the `systemctl --user` argument.
const UNIT_NAME: &str = "intune-container.service";

/// Absolute path of the installed unit file (`~/.config/systemd/user/…`).
fn unit_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("cannot resolve XDG_CONFIG_HOME or HOME")?;
    Ok(unit_path_in(base))
}

/// Pure helper: the unit path under a given config base. Split out so it can be
/// tested without touching process-wide environment variables.
fn unit_path_in(config_base: PathBuf) -> PathBuf {
    config_base.join("systemd/user").join(UNIT_NAME)
}

/// Render the unit, pinned to the absolute path of the currently running binary
/// so it keeps working regardless of what is (or isn't) on `PATH` at boot.
fn render_unit() -> Result<String> {
    let exe = std::env::current_exe()
        .context("cannot determine own executable path")?
        .to_string_lossy()
        .into_owned();
    Ok(format!(
        "[Unit]\n\
         Description=Intune container (headless broker for SSO)\n\
         Documentation=https://github.com/magicabdel/intune-container\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart={exe} start\n\
         ExecStop={exe} stop\n\
         # First boot pulls/extracts the rootfs and provisions; give it room.\n\
         TimeoutStartSec=600\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ))
}

/// Run `systemctl --user <args>`, returning an error if it is missing or fails.
fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("failed to run systemctl --user (is systemd available?)")?;
    if !status.success() {
        anyhow::bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

/// Best-effort `loginctl enable-linger <user>` so the unit also starts at boot,
/// before any interactive session. Requires privilege, so a failure is reported
/// as guidance rather than aborting the enable.
fn try_enable_linger() -> bool {
    let user = match std::env::var("USER").ok().filter(|u| !u.is_empty()) {
        Some(u) => u,
        None => return false,
    };
    Command::new("loginctl")
        .args(["enable-linger", &user])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Install and enable the autostart unit, then start it now. Idempotent.
pub fn enable() -> Result<()> {
    let path = unit_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(&path, render_unit()?).with_context(|| format!("write {}", path.display()))?;
    println!("✓ Installed {}", path.display());

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", UNIT_NAME])?;
    println!("✓ Enabled {UNIT_NAME} (starts on login)");

    if try_enable_linger() {
        println!("✓ Lingering on — the container also starts at boot");
    } else {
        println!(
            "• Could not enable lingering automatically. For boot-time (pre-login) \
             start, run:\n    sudo loginctl enable-linger \"$USER\""
        );
    }

    // Start it now so the user doesn't have to reboot to get the current session going.
    systemctl(&["start", UNIT_NAME])?;
    println!("✓ Started now — SSO container is up");
    Ok(())
}

/// Stop, disable, and remove the autostart unit. Also drops lingering. Idempotent.
pub fn disable() -> Result<()> {
    // Ignore failures on stop/disable so a partially-installed state still cleans up.
    let _ = systemctl(&["stop", UNIT_NAME]);
    let _ = systemctl(&["disable", UNIT_NAME]);

    let path = unit_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        println!("✓ Removed {}", path.display());
    }
    let _ = systemctl(&["daemon-reload"]);

    if let Some(user) = std::env::var("USER").ok().filter(|u| !u.is_empty()) {
        let _ = Command::new("loginctl")
            .args(["disable-linger", &user])
            .status();
    }
    println!("✓ Autostart disabled");
    Ok(())
}

/// Print whether the autostart unit is installed and enabled.
pub fn status() -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        println!("Autostart: not installed (run:  intune-container autostart enable)");
        return Ok(());
    }
    let enabled = Command::new("systemctl")
        .args(["--user", "is-enabled", UNIT_NAME])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let active = Command::new("systemctl")
        .args(["--user", "is-active", UNIT_NAME])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("Autostart: installed at {}", path.display());
    println!("  enabled: {enabled}");
    println!("  active:  {active}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_path_prefers_xdg_config_home() {
        // Pure check — no environment mutation, so it's race-free.
        let p = unit_path_in(PathBuf::from("/tmp/xdg-cfg"));
        assert_eq!(
            p,
            PathBuf::from("/tmp/xdg-cfg/systemd/user/intune-container.service")
        );
    }

    #[test]
    fn rendered_unit_pins_the_binary_and_wires_start_stop() {
        let unit = render_unit().unwrap();
        // The ExecStart/ExecStop point at an absolute path (current_exe) + subcommand.
        assert!(unit.contains("ExecStart="));
        assert!(unit.contains(" start\n"));
        assert!(unit.contains("ExecStop="));
        assert!(unit.contains(" stop\n"));
        // oneshot + RemainAfterExit models "bring up and keep it up".
        assert!(unit.contains("Type=oneshot"));
        assert!(unit.contains("RemainAfterExit=yes"));
        // Installed into the user default target so it starts on login/boot.
        assert!(unit.contains("WantedBy=default.target"));
    }
}
