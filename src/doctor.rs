//! `doctor` — health checks for the whole stack.
//!
//! Verifies the things that have bitten us before: config, container, DNS,
//! device registration persistence, keyring, broker services, bus exposure,
//! browser SSO integration, and backups. Each check prints a status line.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::backup;
use crate::config::Config;
use crate::container;

#[derive(Clone, Copy)]
enum Status {
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

fn line(status: Status, label: &str, detail: &str) {
    if detail.is_empty() {
        println!("  {} {}", status.glyph(), label);
    } else {
        println!("  {} {} — {}", status.glyph(), label, detail);
    }
}

/// Run all health checks. Returns Ok always; problems are reported as lines.
pub fn run() -> Result<()> {
    println!("=== intune-container doctor ===\n");

    // --- Config / setup ---
    let config = match Config::load() {
        Ok(c) => c,
        Err(_) => {
            line(
                Status::Fail,
                "Configuration",
                "not found — run: intune-container enroll",
            );
            return Ok(());
        }
    };
    line(Status::Ok, "Configuration", "loaded");

    if config.initialized {
        line(Status::Ok, "Initialized", "rootfs provisioned");
    } else {
        line(Status::Fail, "Initialized", "run: intune-container enroll");
    }

    // --- Host-side persistent state (survives rebuilds) ---
    let state_dir = container::persistent_state_dir();
    let device_dir = format!("{}/device-broker", state_dir);
    match dir_has_content_sudo(&device_dir) {
        Some(true) => line(
            Status::Ok,
            "Device registration",
            "present (persisted on host)",
        ),
        Some(false) => line(
            Status::Warn,
            "Device registration",
            "empty — enroll with: intune-container enroll",
        ),
        None => line(
            Status::Warn,
            "Device registration",
            "could not read (run with sudo?)",
        ),
    }

    // Keyring (tokens) on the host
    let keyring = format!(
        "{}/Intune/.local/share/keyrings/login.keyring",
        std::env::var("HOME").unwrap_or_default()
    );
    if Path::new(&keyring).exists() {
        let size = std::fs::metadata(&keyring).map(|m| m.len()).unwrap_or(0);
        line(
            Status::Ok,
            "Keyring (tokens)",
            &format!("present ({} KB)", size / 1024),
        );
    } else {
        line(
            Status::Warn,
            "Keyring (tokens)",
            "none yet — created on first sign-in",
        );
    }

    // --- Display detection ---
    line(
        Status::Ok,
        "Display mode",
        if config.display_forwarding {
            "forwarding on (GUI works)"
        } else {
            "headless (max isolation)"
        },
    );

    // --- Browser SSO integration (host-side files) ---
    let manifest = format!(
        "{}/.mozilla/native-messaging-hosts/linux_entra_sso.json",
        std::env::var("HOME").unwrap_or_default()
    );
    if config.expose_bus && Path::new(&manifest).exists() {
        line(
            Status::Ok,
            "Browser SSO",
            "native host installed + bus exposed",
        );
    } else if Path::new(&manifest).exists() {
        line(
            Status::Warn,
            "Browser SSO",
            "manifest present but bus not exposed — run: intune-container daemon",
        );
    } else {
        line(
            Status::Skip,
            "Browser SSO",
            "not set up (optional) — run: intune-container daemon",
        );
    }

    // --- Backup ---
    match backup::default_backup_path() {
        Ok(p) if p.exists() => {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            line(
                Status::Ok,
                "Backup",
                &format!("{} ({} KB)", p.display(), size / 1024),
            );
        }
        _ => line(
            Status::Skip,
            "Backup",
            "none — run: intune-container backup",
        ),
    }

    // --- Container runtime checks ---
    let running = container::is_running(&config.machine_name);
    if !running {
        line(
            Status::Warn,
            "Container",
            "not running — start it for live checks",
        );
        println!("\nStart the container and re-run `doctor` for DNS/broker checks.");
        return Ok(());
    }
    line(Status::Ok, "Container", "running");

    // DNS (inside container)
    match container::run_in_container(
        &config.machine_name,
        &config.host_user,
        "getent hosts login.microsoftonline.com >/dev/null && echo ok || echo fail",
    ) {
        Ok(s) if s.contains("ok") => line(Status::Ok, "DNS", "login.microsoftonline.com resolves"),
        _ => line(Status::Fail, "DNS", "resolution failed inside container"),
    }

    // Device broker service
    match container::run_in_container(
        &config.machine_name,
        &config.host_user,
        "systemctl is-active microsoft-identity-device-broker 2>/dev/null || sudo systemctl is-active microsoft-identity-device-broker 2>/dev/null || echo unknown",
    ) {
        Ok(s) if s.contains("active") => line(Status::Ok, "Device broker", "active"),
        _ => line(Status::Warn, "Device broker", "not confirmed active"),
    }

    // Keyring unlocked?
    match container::run_in_container(
        &config.machine_name,
        &config.host_user,
        "busctl --user get-property org.freedesktop.secrets /org/freedesktop/secrets/collection/login org.freedesktop.Secret.Collection Locked 2>/dev/null || echo unknown",
    ) {
        Ok(s) if s.contains("false") => line(Status::Ok, "Keyring", "unlocked"),
        Ok(s) if s.contains("true") => line(Status::Warn, "Keyring", "locked — broker can't read tokens"),
        _ => line(Status::Skip, "Keyring", "lock state unknown"),
    }

    // Bus exposure socket
    if config.expose_bus {
        match config.broker_bus_path() {
            Ok(bus) if bus.exists() => line(Status::Ok, "SSO bus", "container session bus exposed"),
            Ok(_) => line(
                Status::Warn,
                "SSO bus",
                "socket missing — restart the container",
            ),
            Err(e) => line(Status::Warn, "SSO bus", &format!("path unavailable: {e}")),
        }
    }

    println!("\nAll checks complete.");
    Ok(())
}

/// Check whether a root-owned directory has any content (using sudo).
/// Returns Some(true/false) or None if it couldn't be read.
fn dir_has_content_sudo(dir: &str) -> Option<bool> {
    let output = Command::new("sudo")
        .args(["sh", "-c", &format!("ls -A {} 2>/dev/null | head -1", dir)])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(!output.stdout.is_empty())
}
