//! Resolve the stable, canonical path of the `intune-container` binary.
//!
//! [`std::env::current_exe`] is unsuitable when the binary is distributed as an
//! AppImage: the AppImage runtime FUSE-mounts the payload to a temporary
//! directory (`/tmp/.mount_intuneXXXXXX/usr/bin/intune-container`) that
//! disappears once the process exits. Anything that persists that path — the
//! native-messaging wrapper script, the systemd autostart unit, a `.desktop`
//! launcher — breaks on the next boot (or when the AppImage is updated, which
//! changes the random suffix).
//!
//! This module detects AppImage mode via the `$APPIMAGE` environment variable
//! (set by the AppImage runtime to the *real* path of the `.AppImage` file) and
//! returns that instead. Non-AppImage builds fall back to [`current_exe`] as
//! before.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Return the canonical, *stable* path of this binary — the one safe to persist
/// in scripts, manifests, and unit files.
///
/// * **AppImage**: returns `$APPIMAGE` (e.g. `~/.local/bin/intune-container.AppImage`).
/// * **Regular binary / `cargo install`**: returns [`std::env::current_exe`].
///
/// Either way the result is an absolute path that survives reboots.
pub fn stable_exe() -> Result<PathBuf> {
    // The AppImage runtime sets $APPIMAGE to the original .AppImage file before
    // FUSE-mounting the payload. It is the only path guaranteed to survive across
    // runs.
    if let Ok(appimage) = std::env::var("APPIMAGE") {
        let p = PathBuf::from(&appimage);
        if p.is_absolute() && p.exists() {
            return Ok(p);
        }
        // $APPIMAGE is set but doesn't exist (moved/deleted since launch). Fall
        // through to current_exe so we don't fail entirely; the path will at
        // least work for this session.
        tracing::warn!(
            appimage = %appimage,
            "APPIMAGE is set but the file does not exist; falling back to current_exe"
        );
    }

    let exe = std::env::current_exe().context("cannot determine own executable path")?;

    // Binary replaced in place while we were running (an upgrade: `install` +
    // `mv` over it): /proc/self/exe then reads "…/intune-container (deleted)",
    // a path that no longer exists — spawning it fails with ENOENT even though
    // a perfectly good replacement sits at the original path. Strip the marker
    // and use the replacement when it exists.
    if let Some(s) = exe.to_str() {
        if let Some(orig) = s.strip_suffix(" (deleted)") {
            let orig = PathBuf::from(orig);
            if orig.exists() {
                tracing::info!(
                    path = %orig.display(),
                    "binary was replaced while running; using the replacement on disk"
                );
                return Ok(orig);
            }
        }
    }
    if !exe.exists() {
        // Deleted with no replacement (or an unusual /proc form). Fall back to
        // whatever `intune-container` resolves to on PATH before giving up.
        if let Some(found) = find_on_path("intune-container") {
            tracing::warn!(
                path = %found.display(),
                "own executable no longer exists; using the one on PATH"
            );
            return Ok(found);
        }
    }
    Ok(exe)
}

/// First executable named `name` on `$PATH`, if any.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without $APPIMAGE, stable_exe must return the same as current_exe.
    #[test]
    fn without_appimage_env_returns_current_exe() {
        // Only meaningful when $APPIMAGE is not set (normal dev builds).
        if std::env::var_os("APPIMAGE").is_some() {
            return;
        }
        let got = stable_exe().unwrap();
        let expected = std::env::current_exe().unwrap();
        assert_eq!(got, expected);
    }
}
