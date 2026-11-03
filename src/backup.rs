//! Backup and restore Intune enrollment state.
//!
//! Enrollment state lives in the persistent store outside the rootfs
//! (`~/.local/share/intune-container/persist/`), bound into the container at
//! boot, so it already survives rebuilds. The backup is for moving to a new
//! machine or guarding against accidental deletion. The archive layout
//! (`device-state/…`, `home/…`) is stable so archives stay portable.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use tracing::info;

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

// ===== Rootless backend =====

/// Per-user data dir (`$XDG_DATA_HOME` or `~/.local/share`) / `intune-container`.
fn data_dir() -> Result<PathBuf> {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("cannot determine data directory: neither $XDG_DATA_HOME nor $HOME is set")
        .map(|d| d.join("intune-container"))
}

/// The rootless persistence store (must match `backend::rootless::persist_dir`).
fn rootless_persist_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("persist"))
}

/// Whether this machine holds a device registration: the Workplace Join keys
/// and certificate under `persist/state/identity-broker-etc`.
///
/// That is the one piece of enrollment state nothing writes until the device
/// is registered, so an embedding application can ask it before importing a
/// backup or offering an enrollment: either one over a registration already
/// made here would replace it.
///
/// It FAILS CLOSED. A directory that exists but cannot be read — owned by an id
/// this process does not hold — answers `true`, because both callers act only
/// on `false`, and acting on a registration nobody could read is the one wrong
/// answer that destroys something. Only a missing or empty directory is "none".
pub fn has_device_registration() -> bool {
    let Ok(persist) = rootless_persist_dir() else {
        return true;
    };
    registration_in(&persist.join("state/identity-broker-etc"))
}

fn registration_in(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// `(persist subpath, archive member)` pairs. The archive layout matches the
/// nspawn backup (`device-state/...`, `home/...`) so backups are interchangeable
/// between backends.
fn rootless_map() -> &'static [(&'static str, &'static str)] {
    &[
        ("state/device-broker", "device-state/device-broker"),
        ("state/intune", "device-state/intune"),
        // Device registration keys/certs (bound from /etc/microsoft/identity-broker).
        (
            "state/identity-broker-etc",
            "device-state/identity-broker-etc",
        ),
        ("home/keyrings", "home/.local/share/keyrings"),
        (
            "home/config-broker",
            "home/.config/microsoft-identity-broker",
        ),
        ("home/config-intune", "home/.config/intune"),
    ]
}

/// Recursively copy `src` into `dst`, preserving file permissions (so the
/// keyring keeps its 0600 mode). Both ends are host-owned in the rootless model.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst).with_context(|| format!("mkdir {}", dst.display()))?;
        for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst).with_context(|| format!("copy {}", src.display()))?;
    }
    Ok(())
}

/// Whether a directory has any contents worth backing up.
fn non_empty_dir(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// Back up the rootless enrollment state (the persist store) to a tar archive.
/// No privilege needed: the store is owned by the host user.
pub fn backup_rootless(output: Option<&Path>) -> Result<PathBuf> {
    let backup_path = match output {
        Some(p) => p.to_path_buf(),
        None => default_backup_path()?,
    };
    if let Some(parent) = backup_path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create backup directory")?;
    }

    let persist = rootless_persist_dir()?;
    let stage = std::env::temp_dir().join(format!("intune-backup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).context("create staging dir")?;

    let mut found_any = false;
    for (sub, member) in rootless_map() {
        let src = persist.join(sub);
        if src.is_dir() && non_empty_dir(&src) {
            copy_tree(&src, &stage.join(member))?;
            found_any = true;
        }
    }
    if !found_any {
        let _ = std::fs::remove_dir_all(&stage);
        anyhow::bail!("No enrollment data found to backup. Is the device enrolled?");
    }

    info!(output = %backup_path.display(), "Creating backup (rootless)");
    let status = Command::new("tar")
        .args([
            "-czf",
            &backup_path.to_string_lossy(),
            "-C",
            &stage.to_string_lossy(),
            ".",
        ])
        .status()
        .context("Failed to create backup archive")?;
    let _ = std::fs::remove_dir_all(&stage);
    if !status.success() {
        anyhow::bail!("Backup tar failed");
    }

    let size = std::fs::metadata(&backup_path)
        .map(|m| m.len())
        .unwrap_or(0);
    info!(size_kb = size / 1024, path = %backup_path.display(), "Backup complete");
    Ok(backup_path)
}

/// Restore the rootless enrollment state from a tar archive into the persist
/// store. The caller must ensure the container is stopped.
pub fn restore_rootless(input: Option<&Path>) -> Result<()> {
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

    let persist = rootless_persist_dir()?;
    info!(path = %backup_path.display(), "Restoring from backup (rootless)");

    let tmp = std::env::temp_dir().join(format!("intune-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).context("Failed to create temp dir")?;

    let status = Command::new("tar")
        .args([
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

    if !tmp.join("device-state").exists() && !tmp.join("home").exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        anyhow::bail!(
            "{} does not look like an intune-container backup (no device-state/ or home/ entries)",
            backup_path.display()
        );
    }

    for (sub, member) in rootless_map() {
        let src = tmp.join(member);
        if src.exists() {
            let dst = persist.join(sub);
            let _ = std::fs::remove_dir_all(&dst);
            copy_tree(&src, &dst)?;
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    info!("Enrollment state restored. Bring the container up with: intune-container start");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registration is "none" only when there is provably none: an unreadable
    /// directory reads as present, so nobody imports or enrolls over it.
    #[test]
    fn a_registration_nobody_can_read_counts_as_one() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("ic-reg-{}", std::process::id()));
        let dir = base.join("identity-broker-etc");
        let _ = std::fs::remove_dir_all(&base);
        assert!(!registration_in(&dir), "missing is none");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!registration_in(&dir), "empty is none");
        std::fs::write(dir.join("cert.pem"), b"x").unwrap();
        assert!(registration_in(&dir), "a file is a registration");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        // root reads it anyway; only an unprivileged run can see the refusal.
        if std::fs::read_dir(&dir).is_err() {
            assert!(registration_in(&dir), "unreadable fails closed");
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}
