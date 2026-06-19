//! Configuration management for intune-container.
//!
//! Loads configuration from `~/.local/share/intune-container/config.toml`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Primary configuration for the intune container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Name of the systemd-nspawn machine (used with machinectl)
    pub machine_name: String,

    /// Path to the container root filesystem
    pub rootfs_path: PathBuf,

    /// UID of the host user (mapped into the container)
    pub host_uid: u32,

    /// Username of the host user
    pub host_user: String,

    /// Expose the container's D-Bus session bus to the host so the SSO native
    /// messaging host can reach the Microsoft Identity Broker. Set by `sso`.
    #[serde(default, alias = "broker_proxy")]
    pub expose_bus: bool,

    /// Whether the currently-running (or last-started) container has the real
    /// host display forwarded. Managed automatically: headless is the default
    /// (max isolation, used for background SSO); the GUI flows (`enroll`,
    /// `edge`) flip this on for the duration of the session. Default: false.
    #[serde(default)]
    pub display_forwarding: bool,

    /// Set to true once `init` has successfully provisioned the rootfs.
    /// Used instead of stat'ing the rootfs, which lives under /var/lib/machines
    /// (often mode 700 root) and isn't readable by the unprivileged user.
    #[serde(default)]
    pub initialized: bool,
}

impl Default for Config {
    fn default() -> Self {
        let uid = nix::unistd::getuid().as_raw();
        let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());

        Self {
            machine_name: "intune".to_string(),
            rootfs_path: PathBuf::from("/var/lib/machines/intune"),
            host_uid: uid,
            host_user: user,
            expose_bus: false,
            display_forwarding: false,
            initialized: false,
        }
    }
}

impl Config {
    /// Path where the container's session bus socket is exposed on the host
    /// (when broker_proxy is enabled). The container bind-mounts its runtime
    /// dir here so the host proxy can reach the broker.
    pub fn broker_bus_path(&self) -> Result<PathBuf> {
        Ok(data_dir()?
            .join("intune-container")
            .join("container-runtime")
            .join("bus"))
    }

    /// Host directory bind-mounted to the container's /run/user/<uid>.
    pub fn broker_runtime_dir(&self) -> Result<PathBuf> {
        Ok(self
            .broker_bus_path()?
            .parent()
            .expect("bus path always has a parent")
            .to_path_buf())
    }
}

impl Config {
    /// Returns the path to the configuration file.
    pub fn config_path() -> Result<PathBuf> {
        Ok(data_dir()?.join("intune-container").join("config.toml"))
    }

    /// Load configuration from the standard path.
    ///
    /// Returns an error if the config file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;

        if !path.exists() {
            anyhow::bail!(
                "Configuration not found at {}. Run `intune-container init` first.",
                path.display()
            );
        }

        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config from {}", path.display()))?;

        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config at {}", path.display()))?;
        config.validate()?;

        Ok(config)
    }

    /// Load configuration if it exists, or create a default one.
    pub fn load_or_create() -> Result<Self> {
        let path = Self::config_path()?;

        if path.exists() {
            debug!(path = %path.display(), "Loading existing configuration");
            return Self::load();
        }

        debug!(path = %path.display(), "Creating default configuration");
        let config = Config::default();
        config.validate()?;
        config.save()?;
        Ok(config)
    }

    /// Save configuration to the standard path.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory {}", parent.display())
            })?;
        }

        let contents = toml::to_string_pretty(self).context("Failed to serialize configuration")?;

        std::fs::write(&path, contents)
            .with_context(|| format!("Failed to write config to {}", path.display()))?;

        debug!(path = %path.display(), "Configuration saved");
        Ok(())
    }

    /// Validate fields that are interpolated into shell scripts and the sudoers
    /// rule (`host_user`, `machine_name`). Rejecting metacharacters here is
    /// defense-in-depth against a hostile `$USER`/config value turning a
    /// `format!`-built script into command injection.
    fn validate(&self) -> Result<()> {
        if !is_safe_name(&self.host_user) {
            anyhow::bail!(
                "invalid host_user {:?}: must start with a letter/digit/underscore and \
                 contain only [A-Za-z0-9_.-]",
                self.host_user
            );
        }
        if !is_safe_name(&self.machine_name) {
            anyhow::bail!(
                "invalid machine_name {:?}: must start with a letter/digit/underscore and \
                 contain only [A-Za-z0-9_.-]",
                self.machine_name
            );
        }
        Ok(())
    }
}

/// Resolve the per-user data directory (`$XDG_DATA_HOME` or `~/.local/share`)
/// without panicking when neither is set.
fn data_dir() -> Result<PathBuf> {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("cannot determine data directory: neither $XDG_DATA_HOME nor $HOME is set")
}

/// A name safe to interpolate into shell scripts / sudoers: non-empty, ≤64
/// chars, first char alphanumeric or `_`, rest from `[A-Za-z0-9_.-]`.
fn is_safe_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    let mut bytes = s.bytes();
    let first = bytes.next().unwrap();
    if !(first.is_ascii_alphanumeric() || first == b'_') {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::is_safe_name;

    #[test]
    fn accepts_real_names() {
        for name in [
            "intune",
            "magicabdel",
            "user_1",
            "web-svc",
            "a.b-c_d",
            "_svc",
        ] {
            assert!(is_safe_name(name), "{name} should be accepted");
        }
    }

    #[test]
    fn rejects_injection_and_bad_input() {
        for name in [
            "",
            "-leading-dash",
            "a b",
            "a;rm -rf /",
            "a$(touch x)",
            "a`id`",
            "a|b",
            "a&b",
            "a/b",
            "a\"b",
        ] {
            assert!(!is_safe_name(name), "{name:?} should be rejected");
        }
        assert!(
            !is_safe_name(&"a".repeat(65)),
            "over-long should be rejected"
        );
    }
}
