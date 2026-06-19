//! Cross-process advisory lock that serializes container lifecycle operations.
//!
//! Booting and stopping the container are not safe to run concurrently: the SSO
//! native messaging host is spawned by the browser at arbitrary times and calls
//! `ensure_running`, so it can race a user-invoked `enroll`/`sso`/`stop`. Two
//! processes doing stop+boot at once can leave a half-booted machine — or, worse,
//! `rm -rf` a rootfs that is still mounted.
//!
//! Every command holds this exclusive lock around its lifecycle-mutating section
//! (and only that section — long-running work like an interactive shell or the
//! native-host event loop runs after the lock is released).

use std::fs::File;
use std::path::PathBuf;

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
use tracing::info;

/// An held exclusive lifecycle lock. Releasing it (drop) unlocks automatically.
pub struct LifecycleLock {
    // The lock is released when the underlying open file description is closed,
    // which happens when this `Flock` is dropped.
    _flock: Flock<File>,
}

impl LifecycleLock {
    /// Acquire the exclusive lifecycle lock, waiting if another instance holds it.
    ///
    /// Tries non-blocking first so we can tell the user we're waiting, then blocks.
    pub fn acquire() -> Result<Self> {
        let path = lock_path()?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = File::create(&path)
            .with_context(|| format!("Failed to open lock file {}", path.display()))?;

        let flock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(f) => f,
            Err((file, _)) => {
                info!("Waiting for another intune-container instance to finish...");
                Flock::lock(file, FlockArg::LockExclusive)
                    .map_err(|(_, e)| e)
                    .context("Failed to acquire lifecycle lock")?
            }
        };

        Ok(Self { _flock: flock })
    }
}

/// Path to the lock file under the per-user data directory.
fn lock_path() -> Result<PathBuf> {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("Neither XDG_DATA_HOME nor HOME is set")?;
    Ok(data_dir.join("intune-container").join("lifecycle.lock"))
}
