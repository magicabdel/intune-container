//! Compositor detection and adaptation.
//!
//! Different Wayland compositors have different quirks around XWayland,
//! Xauthority handling, and environment variable expectations. This module
//! detects which compositor is in use and generates appropriate environment
//! setup scripts for the container.

use std::process::Command;

use tracing::debug;

/// Known Wayland compositors with specific handling requirements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compositor {
    /// Niri — tiling Wayland compositor; uses abstract X11 sockets,
    /// often doesn't create Xauthority files.
    Niri,
    /// Hyprland — dynamic tiling Wayland compositor.
    Hyprland,
    /// Sway — i3-compatible Wayland compositor.
    Sway,
    /// GNOME (Mutter) — uses .mutter-Xwaylandauth.* for Xauthority.
    Gnome,
    /// KDE Plasma (KWin).
    Kde,
    /// Unknown compositor — use generic detection.
    Unknown(String),
}

/// Detect the running Wayland compositor.
///
/// Detection order:
/// 1. $XDG_CURRENT_DESKTOP environment variable
/// 2. $XDG_SESSION_DESKTOP environment variable
/// 3. Check for known compositor processes
pub fn detect_compositor() -> Compositor {
    // Try $XDG_CURRENT_DESKTOP first (most reliable)
    if let Ok(desktop) = std::env::var("XDG_CURRENT_DESKTOP") {
        let desktop_lower = desktop.to_lowercase();
        debug!(xdg_current_desktop = %desktop, "Checking $XDG_CURRENT_DESKTOP");

        if desktop_lower.contains("niri") {
            return Compositor::Niri;
        }
        if desktop_lower.contains("hyprland") {
            return Compositor::Hyprland;
        }
        if desktop_lower.contains("sway") {
            return Compositor::Sway;
        }
        if desktop_lower.contains("gnome") {
            return Compositor::Gnome;
        }
        if desktop_lower.contains("kde") || desktop_lower.contains("plasma") {
            return Compositor::Kde;
        }
    }

    // Try $XDG_SESSION_DESKTOP as fallback
    if let Ok(session) = std::env::var("XDG_SESSION_DESKTOP") {
        let session_lower = session.to_lowercase();
        debug!(xdg_session_desktop = %session, "Checking $XDG_SESSION_DESKTOP");

        if session_lower.contains("niri") {
            return Compositor::Niri;
        }
        if session_lower.contains("hyprland") {
            return Compositor::Hyprland;
        }
        if session_lower.contains("sway") {
            return Compositor::Sway;
        }
        if session_lower.contains("gnome") {
            return Compositor::Gnome;
        }
        if session_lower.contains("kde") || session_lower.contains("plasma") {
            return Compositor::Kde;
        }
    }

    // Last resort: check running processes
    if let Some(compositor) = detect_from_processes() {
        return compositor;
    }

    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    debug!(desktop = %desktop, "Unknown compositor");
    Compositor::Unknown(desktop)
}

/// Check running processes for known compositor binaries.
fn detect_from_processes() -> Option<Compositor> {
    let output = Command::new("ps").args(["-eo", "comm"]).output().ok()?;

    let processes = String::from_utf8_lossy(&output.stdout);

    for line in processes.lines() {
        let proc_name = line.trim();
        match proc_name {
            "niri" => return Some(Compositor::Niri),
            "Hyprland" | "hyprland" => return Some(Compositor::Hyprland),
            "sway" => return Some(Compositor::Sway),
            "gnome-shell" | "mutter" => return Some(Compositor::Gnome),
            "kwin_wayland" | "plasmashell" => return Some(Compositor::Kde),
            _ => {}
        }
    }

    None
}
