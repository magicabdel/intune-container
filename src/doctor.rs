//! `doctor` — health checks for the whole stack.
//!
//! Verifies the things that have bitten us before: config, container, DNS,
//! device registration persistence, keyring, broker services, bus exposure,
//! browser SSO integration, and backups.
//!
//! [`collect`] returns the checks as structured data (used by the GUI); [`run`]
//! prints them as status lines (used by the CLI).

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::backend;
use crate::backup;
use crate::config::Config;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
            Status::Skip => "–",
        }
    }
}

/// A single health-check result.
#[derive(Clone, Serialize)]
pub struct Check {
    pub status: Status,
    pub label: String,
    pub detail: String,
}

impl Check {
    fn new(status: Status, label: &str, detail: &str) -> Self {
        Check {
            status,
            label: label.to_string(),
            detail: detail.to_string(),
        }
    }
}

/// Run all health checks and return them as structured results.
///
/// Never errors: a missing configuration is itself reported as a failing check.
pub fn collect() -> Vec<Check> {
    let mut checks = Vec::new();

    // --- Config / setup ---
    let config = match Config::load() {
        Ok(c) => c,
        Err(_) => {
            checks.push(Check::new(
                Status::Fail,
                "Configuration",
                "not found — run: intune-container enroll",
            ));
            return checks;
        }
    };
    checks.push(Check::new(Status::Ok, "Configuration", "loaded"));

    if config.initialized {
        checks.push(Check::new(Status::Ok, "Initialized", "rootfs provisioned"));
    } else {
        checks.push(Check::new(
            Status::Fail,
            "Initialized",
            "run: intune-container enroll",
        ));
    }

    // --- Persistent state (survives rebuilds), host-readable ---
    let persist = persist_dir();
    let device_dir = persist.join("state/device-broker");
    match dir_has_content(&device_dir) {
        true => checks.push(Check::new(
            Status::Ok,
            "Device registration",
            "present (persisted on host)",
        )),
        false => checks.push(Check::new(
            Status::Warn,
            "Device registration",
            "empty — enroll with: intune-container enroll",
        )),
    }

    // Keyring (tokens) in the persistent store
    let keyring = persist.join("home/keyrings/login.keyring");
    if keyring.exists() {
        let size = std::fs::metadata(&keyring).map(|m| m.len()).unwrap_or(0);
        checks.push(Check::new(
            Status::Ok,
            "Keyring (tokens)",
            &format!("present ({} KB)", size / 1024),
        ));
    } else {
        checks.push(Check::new(
            Status::Warn,
            "Keyring (tokens)",
            "none yet — created on first sign-in",
        ));
    }

    // --- Display detection ---
    checks.push(Check::new(
        Status::Ok,
        "Display mode",
        if config.display_forwarding {
            "forwarding on (GUI works)"
        } else {
            "headless (max isolation)"
        },
    ));

    // --- Browser SSO integration (host-side files) ---
    let manifest = format!(
        "{}/.mozilla/native-messaging-hosts/linux_entra_sso.json",
        std::env::var("HOME").unwrap_or_default()
    );
    if config.expose_bus && Path::new(&manifest).exists() {
        checks.push(Check::new(
            Status::Ok,
            "Browser SSO",
            "native host installed + bus exposed",
        ));
    } else if Path::new(&manifest).exists() {
        checks.push(Check::new(
            Status::Warn,
            "Browser SSO",
            "manifest present but bus not exposed — run: intune-container daemon",
        ));
    } else {
        checks.push(Check::new(
            Status::Skip,
            "Browser SSO",
            "not set up (optional) — run: intune-container daemon",
        ));
    }

    // --- Backup ---
    match backup::default_backup_path() {
        Ok(p) if p.exists() => {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            checks.push(Check::new(
                Status::Ok,
                "Backup",
                &format!("{} ({} KB)", p.display(), size / 1024),
            ));
        }
        _ => checks.push(Check::new(
            Status::Skip,
            "Backup",
            "none — run: intune-container backup",
        )),
    }

    // --- Container runtime checks ---
    let running = backend::is_running(&config);
    if !running {
        checks.push(Check::new(
            Status::Warn,
            "Container",
            "not running — start it for live checks",
        ));
        return checks;
    }
    checks.push(Check::new(Status::Ok, "Container", "running"));

    // DNS (inside container)
    if backend::probe(&config, "getent hosts login.microsoftonline.com >/dev/null") == 0 {
        checks.push(Check::new(
            Status::Ok,
            "DNS",
            "login.microsoftonline.com resolves",
        ));
    } else {
        checks.push(Check::new(
            Status::Fail,
            "DNS",
            "resolution failed inside container",
        ));
    }

    // Device broker service
    if backend::probe(
        &config,
        "systemctl is-active --quiet microsoft-identity-device-broker.service",
    ) == 0
    {
        checks.push(Check::new(Status::Ok, "Device broker", "active"));
    } else {
        checks.push(Check::new(
            Status::Warn,
            "Device broker",
            "not confirmed active",
        ));
    }

    // Keyring unlocked? (query the user session bus the broker uses)
    let keyring_locked = "export XDG_RUNTIME_DIR=/run/user/0 \
         DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus; \
         [ \"$(busctl --user get-property org.freedesktop.secrets \
           /org/freedesktop/secrets/collection/login \
           org.freedesktop.Secret.Collection Locked 2>/dev/null)\" = \"b false\" ]";
    match backend::probe(&config, keyring_locked) {
        0 => checks.push(Check::new(Status::Ok, "Keyring", "unlocked")),
        _ => checks.push(Check::new(
            Status::Skip,
            "Keyring",
            "locked or lock state unknown",
        )),
    }

    checks
}

/// Run all health checks and print them as status lines. Returns Ok always;
/// problems are reported as lines.
pub fn run() -> Result<()> {
    println!("=== intune-container doctor ===\n");

    let checks = collect();
    for c in &checks {
        if c.detail.is_empty() {
            println!("  {} {}", c.status.glyph(), c.label);
        } else {
            println!("  {} {} — {}", c.status.glyph(), c.label, c.detail);
        }
    }

    // Hint when live checks were skipped because the container is down.
    let container_down = checks
        .iter()
        .any(|c| c.label == "Container" && matches!(c.status, Status::Warn));
    if container_down {
        println!("\nStart the container and re-run `doctor` for DNS/broker checks.");
    } else {
        println!("\nAll checks complete.");
    }

    Ok(())
}

/// The rootless persistence store (`~/.local/share/intune-container/persist`).
fn persist_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("intune-container")
        .join("persist")
}

/// Whether a directory exists and has any content.
fn dir_has_content(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}
