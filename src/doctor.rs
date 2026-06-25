//! `doctor` — health checks for the whole stack.
//!
//! Focused on the signals that can fail silently and that the user cares about:
//! enrollment/registration drift, the container being up, network reach, the
//! identity broker, the keyring, and the compliance agent. Cosmetic facts
//! (display mode, SSO, host) are surfaced elsewhere in the UI, not here.
//!
//! [`collect`] returns the checks as structured data (used by the GUI); [`run`]
//! prints them as status lines (used by the CLI).

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::backend;
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

    // Setup gate: nothing else matters until the container is provisioned.
    let config = match Config::load() {
        Ok(c) if c.initialized => c,
        _ => {
            checks.push(Check::new(
                Status::Fail,
                "Setup",
                "not set up — enroll this device to begin",
            ));
            return checks;
        }
    };

    // Registration drift: a registration the agent flagged for repair is the
    // "stuck re-enrolling → non-compliant" state that has bitten us before.
    let reg = persist_dir().join("home/config-intune/registration.toml");
    match std::fs::read_to_string(&reg) {
        Ok(s) if s.contains("needs_patching = \"true\"") => checks.push(Check::new(
            Status::Warn,
            "Registration",
            "needs patching (device-identity drift) — re-enroll if non-compliant",
        )),
        Ok(_) => checks.push(Check::new(Status::Ok, "Registration", "device enrolled")),
        Err(_) => checks.push(Check::new(
            Status::Skip,
            "Registration",
            "no registration recorded yet",
        )),
    }

    // Live checks need a running container.
    if !backend::is_running(&config) {
        checks.push(Check::new(
            Status::Warn,
            "Container",
            "stopped — start it to check in with Intune",
        ));
        return checks;
    }
    checks.push(Check::new(Status::Ok, "Container", "running"));

    // Network reach to the Microsoft endpoints (from inside the container).
    if backend::probe(&config, "getent hosts login.microsoftonline.com >/dev/null") == 0 {
        checks.push(Check::new(
            Status::Ok,
            "Network",
            "Microsoft endpoints reachable",
        ));
    } else {
        checks.push(Check::new(
            Status::Fail,
            "Network",
            "can't resolve login.microsoftonline.com",
        ));
    }

    // The identity broker — the core service everything else relies on.
    if backend::probe(
        &config,
        "systemctl is-active --quiet microsoft-identity-device-broker.service",
    ) == 0
    {
        checks.push(Check::new(Status::Ok, "Identity broker", "active"));
    } else {
        checks.push(Check::new(Status::Warn, "Identity broker", "not active"));
    }

    // Keyring unlocked (holds the device key; locked → broker can't store secrets).
    let keyring_unlocked = "export XDG_RUNTIME_DIR=/run/user/0 \
         DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus; \
         [ \"$(busctl --user get-property org.freedesktop.secrets \
           /org/freedesktop/secrets/collection/login \
           org.freedesktop.Secret.Collection Locked 2>/dev/null)\" = \"b false\" ]";
    match backend::probe(&config, keyring_unlocked) {
        0 => checks.push(Check::new(Status::Ok, "Keyring", "unlocked")),
        _ => checks.push(Check::new(Status::Skip, "Keyring", "locked or unknown")),
    }

    // Compliance agent: the timer must be scheduled (not masked/stopped), or the
    // device silently drifts to non-compliant. (Whether the *last* run passed is
    // deliberately not shown — it's transiently "failed" right after a boot.)
    let env =
        "export XDG_RUNTIME_DIR=/run/user/0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus;";
    let masked = backend::probe(
        &config,
        &format!(
            "{env} [ \"$(systemctl --user is-enabled intune-agent.timer 2>/dev/null)\" = masked ]"
        ),
    ) == 0;
    let active = backend::probe(
        &config,
        &format!("{env} systemctl --user is-active --quiet intune-agent.timer"),
    ) == 0;
    if masked {
        checks.push(Check::new(
            Status::Fail,
            "Compliance agent",
            "disabled — device won't report compliant; re-enroll",
        ));
    } else if active {
        checks.push(Check::new(Status::Ok, "Compliance agent", "scheduled"));
    } else {
        checks.push(Check::new(
            Status::Warn,
            "Compliance agent",
            "not scheduled",
        ));
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
