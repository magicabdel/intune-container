//! Backup and restore Intune enrollment state.
//!
//! All enrollment state lives on the HOST (bind-mounted into the container),
//! so backup/restore operate on host paths and don't depend on the rootfs:
//!
//! - Device registration: /var/lib/intune-container/device-broker/
//! - Agent state:         /var/lib/intune-container/intune/
//! - Tokens (keyring):    ~/Intune/.local/share/keyrings/
//! - Broker user config:  ~/Intune/.config/microsoft-identity-broker/
//! - Intune user config:  ~/Intune/.config/intune/
//!
//! These survive container rebuilds on their own; the backup is for moving to a
//! new machine or guarding against accidental deletion.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::Config;

/// Persistent host directory holding device-state (must match container.rs).
const PERSISTENT_STATE_DIR: &str = "/var/lib/intune-container";

/// Root-owned device-state subdirs under PERSISTENT_STATE_DIR.
const DEVICE_STATE_SUBDIRS: &[&str] = &["device-broker", "intune"];

/// User-owned enrollment paths relative to ~/Intune.
const HOME_SUBPATHS: &[&str] = &[
    ".local/share/keyrings",
    ".config/microsoft-identity-broker",
    ".config/intune",
];

/// Default backup file location.
pub fn default_backup_path() -> Result<PathBuf> {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("cannot determine data directory: neither $XDG_DATA_HOME nor $HOME is set")?;

    Ok(data_dir
        .join("intune-container")
        .join("enrollment-backup.tar.gz"))
}

fn intune_home() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Intune"))
}

/// Backup enrollment state (device registration + tokens) to a tar archive.
///
/// Archive layout:
///   device-state/<subdir>/...   (from /var/lib/intune-container/<subdir>)
///   home/<subpath>/...          (from ~/Intune/<subpath>)
pub fn backup(_config: &Config, output: Option<&Path>) -> Result<PathBuf> {
    let backup_path = match output {
        Some(p) => p.to_path_buf(),
        None => default_backup_path()?,
    };
    if let Some(parent) = backup_path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create backup directory")?;
    }

    let home = intune_home()?;

    // Build a list of (base_dir, member_path) entries that actually exist.
    // We tar with sudo since device-state is root-owned.
    let mut tar_args: Vec<String> = vec![
        "tar".into(),
        "-czf".into(),
        backup_path.to_string_lossy().to_string(),
    ];

    let mut found_any = false;

    // device-state/* from /var/lib/intune-container
    // (--transform rewrites the stored path prefix to "device-state/")
    for sub in DEVICE_STATE_SUBDIRS {
        let full = format!("{}/{}", PERSISTENT_STATE_DIR, sub);
        if Path::new(&full).exists() {
            tar_args.push("-C".into());
            tar_args.push(PERSISTENT_STATE_DIR.to_string());
            tar_args.push("--transform=s,^,device-state/,".to_string());
            tar_args.push(sub.to_string());
            found_any = true;
        }
    }

    // home/* from ~/Intune
    for sub in HOME_SUBPATHS {
        let full = home.join(sub);
        if full.exists() {
            tar_args.push("-C".into());
            tar_args.push(home.to_string_lossy().to_string());
            tar_args.push("--transform=s,^,home/,".to_string());
            tar_args.push(sub.to_string());
            found_any = true;
        }
    }

    if !found_any {
        anyhow::bail!("No enrollment data found to backup. Is the device enrolled?");
    }

    info!(output = %backup_path.display(), "Creating backup");

    let out = Command::new("sudo")
        .args(&tar_args)
        .output()
        .context("Failed to create backup archive")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("No such file") {
            warn!(
                "Some paths were missing during backup (non-fatal): {}",
                stderr.trim()
            );
        } else {
            anyhow::bail!("Backup tar failed: {}", stderr.trim());
        }
    }

    // Make the archive user-owned.
    let uid = nix::unistd::getuid();
    let _ = Command::new("sudo")
        .args([
            "chown",
            &format!("{}:{}", uid, uid),
            &backup_path.to_string_lossy(),
        ])
        .status();

    let size = std::fs::metadata(&backup_path)
        .map(|m| m.len())
        .unwrap_or(0);
    info!(size_kb = size / 1024, path = %backup_path.display(), "Backup complete");

    Ok(backup_path)
}

/// Restore enrollment state from a tar archive into the host locations.
pub fn restore(config: &Config, input: Option<&Path>) -> Result<()> {
    let backup_path = match input {
        Some(p) => p.to_path_buf(),
        None => default_backup_path()?,
    };
    if !backup_path.exists() {
        anyhow::bail!(
            "Backup file not found at {}. Run `backup` first.",
            backup_path.display()
        );
    }

    // Refuse to restore into a running container: the device broker holds the
    // device-state files open, so overwriting them underneath it yields a
    // half-updated, inconsistent enrollment.
    if crate::container::is_running(&config.machine_name) {
        anyhow::bail!(
            "Container '{}' is running. Stop it first:  intune-container stop",
            config.machine_name
        );
    }

    let home = intune_home()?;
    info!(path = %backup_path.display(), "Restoring from backup");

    // Extract to a temp dir, then move the two trees into place.
    let tmp = std::env::temp_dir().join(format!("intune-restore-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).context("Failed to create temp dir")?;

    let status = Command::new("sudo")
        .args([
            "tar",
            "-xzf",
            &backup_path.to_string_lossy(),
            "-C",
            &tmp.to_string_lossy(),
        ])
        .status()
        .context("Failed to extract backup archive")?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        anyhow::bail!("Restore extraction failed");
    }

    // Validate the archive looks like one of ours before touching live state.
    let device_src = tmp.join("device-state");
    let home_src = tmp.join("home");
    if !device_src.exists() && !home_src.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        anyhow::bail!(
            "{} does not look like an intune-container backup (no device-state/ or home/ entries)",
            backup_path.display()
        );
    }

    // device-state/* -> /var/lib/intune-container/ (root-owned)
    if device_src.exists() {
        let _ = Command::new("sudo")
            .args(["mkdir", "-p", PERSISTENT_STATE_DIR])
            .status();
        let status = Command::new("sudo")
            .args([
                "cp",
                "-a",
                &format!("{}/.", device_src.to_string_lossy()),
                &format!("{}/", PERSISTENT_STATE_DIR),
            ])
            .status()
            .context("Failed to restore device-state")?;
        if !status.success() {
            warn!("device-state restore had issues");
        }
    }

    // home/* -> ~/Intune/ (user-owned)
    if home_src.exists() {
        std::fs::create_dir_all(&home)?;
        let status = Command::new("cp")
            .args([
                "-a",
                &format!("{}/.", home_src.to_string_lossy()),
                &format!("{}/", home.to_string_lossy()),
            ])
            .status()
            .context("Failed to restore home state")?;
        if !status.success() {
            warn!("home state restore had issues");
        }
    }

    let _ = Command::new("sudo")
        .args(["rm", "-rf", &tmp.to_string_lossy()])
        .status();

    info!("Enrollment state restored. Bring the container up with: intune-container daemon");
    Ok(())
}

/// List what's in a backup archive without extracting.
pub fn inspect(input: Option<&Path>) -> Result<()> {
    let backup_path = match input {
        Some(p) => p.to_path_buf(),
        None => default_backup_path()?,
    };
    if !backup_path.exists() {
        anyhow::bail!("Backup file not found at {}", backup_path.display());
    }

    let size = std::fs::metadata(&backup_path)
        .map(|m| m.len())
        .unwrap_or(0);
    println!("Backup: {}", backup_path.display());
    println!("Size:   {} KB", size / 1024);
    println!();
    println!("Contents:");

    let output = Command::new("tar")
        .args(["-tzf", &backup_path.to_string_lossy()])
        .output()
        .context("Failed to list backup contents")?;
    if output.status.success() {
        let contents = String::from_utf8_lossy(&output.stdout);
        for line in contents.lines().take(50) {
            println!("  {}", line);
        }
        let total = contents.lines().count();
        if total > 50 {
            println!("  ... and {} more entries", total - 50);
        }
    }

    Ok(())
}
