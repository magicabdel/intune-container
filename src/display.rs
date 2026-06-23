//! Compositor-agnostic display detection.
//!
//! This module detects Wayland sockets, X11 displays (including abstract sockets),
//! and Xauthority files without hardcoding any socket names. Everything is read
//! from the environment and probed at runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

/// Collected display information for passing into the container.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    /// Path to the Wayland socket (e.g., /run/user/1000/wayland-1)
    pub wayland_socket: Option<PathBuf>,

    /// X11 display string (e.g., ":0" or ":1")
    pub x11_display: Option<String>,

    /// Path to the Xauthority file
    pub xauthority: Option<PathBuf>,

    /// Whether X11 is using an abstract socket (common with Niri's XWayland)
    pub has_abstract_x11: bool,
}

impl DisplayInfo {
    /// Detect all available display information from the current environment.
    pub fn detect() -> Self {
        let wayland_socket = detect_wayland();
        let (x11_display, has_abstract_x11) = detect_x11();
        let xauthority = find_xauthority();

        let info = Self {
            wayland_socket,
            x11_display,
            xauthority,
            has_abstract_x11,
        };

        // Log what we found (debug-level: detect() runs several times per
        // command, so emitting these at info would spam the default output).
        if info.wayland_socket.is_some() {
            debug!("Wayland socket detected");
        }
        if info.x11_display.is_some() {
            debug!(
                abstract_socket = info.has_abstract_x11,
                "X11 display detected"
            );
        }
        if info.xauthority.is_some() {
            debug!("Xauthority file found");
        } else if info.x11_display.is_some() {
            debug!("X11 display found but no Xauthority — may need to generate one");
        }

        info
    }

    /// Returns the XDG_RUNTIME_DIR for the current user.
    pub fn xdg_runtime_dir() -> Option<PathBuf> {
        std::env::var("XDG_RUNTIME_DIR").ok().map(PathBuf::from)
    }
}

/// Detect the Wayland socket by reading $WAYLAND_DISPLAY.
///
/// The value can be either:
/// - A bare name like "wayland-1" → resolved relative to $XDG_RUNTIME_DIR
/// - An absolute path like "/run/user/1000/wayland-1"
///
/// We NEVER hardcode "wayland-0" or any other name.
pub fn detect_wayland() -> Option<PathBuf> {
    let wayland_display = match std::env::var("WAYLAND_DISPLAY") {
        Ok(val) if !val.is_empty() => val,
        _ => {
            debug!("$WAYLAND_DISPLAY not set or empty");
            return None;
        }
    };

    debug!(wayland_display = %wayland_display, "Found $WAYLAND_DISPLAY");

    let socket_path = if wayland_display.starts_with('/') {
        // Absolute path — use directly
        PathBuf::from(&wayland_display)
    } else {
        // Relative name — resolve against $XDG_RUNTIME_DIR
        let runtime_dir = match std::env::var("XDG_RUNTIME_DIR") {
            Ok(dir) => PathBuf::from(dir),
            Err(_) => {
                warn!("$XDG_RUNTIME_DIR not set, cannot resolve Wayland socket");
                return None;
            }
        };
        runtime_dir.join(&wayland_display)
    };

    if socket_path.exists() {
        debug!(path = %socket_path.display(), "Wayland socket exists");
        Some(socket_path)
    } else {
        warn!(
            path = %socket_path.display(),
            "Wayland socket path does not exist"
        );
        None
    }
}

/// Detect X11 display information.
///
/// Returns (display_string, is_abstract).
///
/// Checks:
/// 1. $DISPLAY environment variable
/// 2. File-based sockets at /tmp/.X11-unix/X{N}
/// 3. Abstract sockets by parsing /proc/net/unix (used by Niri and some XWayland setups)
pub fn detect_x11() -> (Option<String>, bool) {
    let x11_display = match std::env::var("DISPLAY") {
        Ok(val) if !val.is_empty() => val,
        _ => {
            debug!("$DISPLAY not set or empty");
            return (None, false);
        }
    };

    debug!(x11_display = %x11_display, "Found $DISPLAY");

    // Extract display number from ":N" or ":N.S" format
    let display_num = extract_display_number(&x11_display);

    if let Some(num) = display_num {
        // Check for file-based socket first
        let socket_path = PathBuf::from(format!("/tmp/.X11-unix/X{}", num));
        if socket_path.exists() {
            debug!(
                path = %socket_path.display(),
                "File-based X11 socket found"
            );
            return (Some(x11_display), false);
        }

        // Check for abstract socket by parsing /proc/net/unix
        let has_abstract = check_abstract_x11_socket(num);
        if has_abstract {
            debug!(display_num = num, "Abstract X11 socket found");
            return (Some(x11_display), true);
        }

        // Socket file doesn't exist but $DISPLAY is set — might still work
        // (e.g., TCP connection or socket not yet created)
        warn!(
            x11_display = %x11_display,
            "X11 display set but no socket found (file or abstract)"
        );
        return (Some(x11_display), false);
    }

    // Could be a TCP display like "hostname:0"
    debug!(x11_display = %x11_display, "Non-local X11 display");
    (Some(x11_display), false)
}

/// Extract the display number from a DISPLAY string like ":0", ":1.0", "localhost:0".
fn extract_display_number(display: &str) -> Option<u32> {
    // Format is [host]:number[.screen]
    let after_colon = display.rsplit(':').next()?;
    // Strip screen number if present
    let num_str = after_colon.split('.').next()?;
    num_str.parse().ok()
}

/// Check /proc/net/unix for abstract X11 sockets.
///
/// Abstract sockets show up as lines containing "@/tmp/.X11-unix/X{N}" in /proc/net/unix.
/// This is how Niri's XWayland implementation exposes X11.
fn check_abstract_x11_socket(display_num: u32) -> bool {
    let target = format!("@/tmp/.X11-unix/X{}", display_num);

    match std::fs::read_to_string("/proc/net/unix") {
        Ok(contents) => {
            let found = contents.lines().any(|line| line.contains(&target));
            if found {
                debug!(target = %target, "Found abstract X11 socket in /proc/net/unix");
            }
            found
        }
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/net/unix");
            false
        }
    }
}

/// Find the Xauthority file.
///
/// Search order:
/// 1. $XAUTHORITY environment variable
/// 2. Common compositor-specific patterns in $XDG_RUNTIME_DIR:
///    - .mutter-Xwaylandauth.* (GNOME)
///    - xauth_* (generic)
/// 3. ~/.Xauthority (traditional location)
/// 4. If nothing found, attempt to generate one using `xauth`
pub fn find_xauthority() -> Option<PathBuf> {
    // 1. Check $XAUTHORITY
    if let Ok(path) = std::env::var("XAUTHORITY") {
        let path = PathBuf::from(&path);
        if path.exists() {
            debug!(path = %path.display(), "Found Xauthority via $XAUTHORITY");
            return Some(path);
        }
        debug!(
            path = %path.display(),
            "$XAUTHORITY set but file does not exist"
        );
    }

    // 2. Search XDG_RUNTIME_DIR for compositor-specific patterns
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let runtime_path = Path::new(&runtime_dir);

        // Patterns to search for (in priority order)
        let patterns = [
            ".mutter-Xwaylandauth.", // GNOME/Mutter
            "xauth_",                // Generic XWayland
        ];

        if let Ok(entries) = std::fs::read_dir(runtime_path) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                for pattern in &patterns {
                    if name_str.contains(pattern) {
                        let path = entry.path();
                        debug!(
                            path = %path.display(),
                            pattern = %pattern,
                            "Found Xauthority via pattern match"
                        );
                        return Some(path);
                    }
                }
            }
        }
    }

    // 3. Check ~/.Xauthority (must be non-empty to be useful)
    if let Ok(home) = std::env::var("HOME") {
        let path = PathBuf::from(&home).join(".Xauthority");
        if path.exists() {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size > 0 {
                debug!(path = %path.display(), size = size, "Found ~/.Xauthority");
                return Some(path);
            } else {
                debug!(path = %path.display(), "~/.Xauthority exists but is empty, skipping");
            }
        }
    }

    // 4. Nothing found — we may need to generate one
    debug!("No Xauthority file found in any standard location");
    match generate_xauthority() {
        Ok(Some(path)) => {
            info!(path = %path.display(), "Generated Xauthority file");
            Some(path)
        }
        Ok(None) => {
            debug!("Xauthority generation not needed or not possible");
            None
        }
        Err(e) => {
            warn!(error = %e, "Failed to generate Xauthority");
            None
        }
    }
}

/// Attempt to generate an Xauthority file using `xauth`.
///
/// This is needed when the compositor (e.g., Niri) uses abstract sockets and
/// doesn't create an Xauthority file itself.
fn generate_xauthority() -> Result<Option<PathBuf>> {
    // Only generate if we actually have an X11 display
    let x11_display = match std::env::var("DISPLAY") {
        Ok(d) if !d.is_empty() => d,
        _ => return Ok(None),
    };

    // Determine where to put the generated file
    let auth_path = std::env::var("XDG_RUNTIME_DIR")
        .map(|dir| PathBuf::from(dir).join("intune-container-xauth"))
        .or_else(|_| {
            std::env::var("HOME").map(|home| PathBuf::from(home).join(".intune-container-xauth"))
        })
        .context("Neither XDG_RUNTIME_DIR nor HOME is set")?;

    debug!(
        path = %auth_path.display(),
        x11_display = %x11_display,
        "Generating Xauthority"
    );

    // Generate a random hex cookie
    let cookie: String = (0..32).map(|_| format!("{:x}", rand_nibble())).collect();

    // Use xauth to create the file
    let status = Command::new("xauth")
        .args([
            "-f",
            &auth_path.to_string_lossy(),
            "add",
            &x11_display,
            ".",
            &cookie,
        ])
        .status()
        .context("Failed to run xauth command")?;

    if status.success() {
        Ok(Some(auth_path))
    } else {
        warn!("xauth command failed with status {}", status);
        Ok(None)
    }
}

/// Generate a pseudo-random nibble (0-15) for cookie generation.
/// Uses /dev/urandom for actual randomness.
fn rand_nibble() -> u8 {
    use std::io::Read;
    let mut buf = [0u8; 1];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf[0] & 0x0f
}

// ===== Rootless backend: bind/env plan =====

/// Where in the container a forwarded display artifact should live, plus the
/// environment variable that points the GUI app at it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DisplayAttach {
    /// `(host_path, container_path)` bind mounts to inject before launching.
    pub binds: Vec<(PathBuf, PathBuf)>,
    /// `(name, value)` environment variables to set on the launched process.
    pub env: Vec<(String, String)>,
}

impl DisplayInfo {
    /// Translate detected host display info into the bind mounts and environment
    /// the rootless backend needs to show a GUI app from inside the container.
    ///
    /// The container shares the host network namespace, so abstract X11 sockets
    /// and TCP displays work with no bind — only the Wayland unix socket, the
    /// file-based X11 socket dir, and the Xauthority file need to be mounted in.
    pub fn attach_plan(&self, uid: u32) -> DisplayAttach {
        let mut plan = DisplayAttach::default();
        let runtime_dir = PathBuf::from(format!("/run/user/{uid}"));
        plan.env
            .push(("XDG_RUNTIME_DIR".into(), runtime_dir.display().to_string()));

        if let Some(sock) = &self.wayland_socket {
            // Bind to a stable path OUTSIDE /run/user/<uid>: the container's
            // user-runtime-dir service mounts a tmpfs over /run/user/<uid> at
            // login, which would shadow a socket bound underneath it. Wayland
            // accepts an absolute WAYLAND_DISPLAY, so point it straight at the
            // bind target.
            let target = PathBuf::from("/run/host-wayland");
            plan.binds.push((sock.clone(), target.clone()));
            plan.env
                .push(("WAYLAND_DISPLAY".into(), target.display().to_string()));
        }

        if let Some(display) = &self.x11_display {
            plan.env.push(("DISPLAY".into(), display.clone()));
            // A file-based socket must be bound in; an abstract one rides the
            // shared host network namespace and needs nothing.
            if !self.has_abstract_x11 {
                if let Some(num) = extract_display_number(display) {
                    let sock = PathBuf::from(format!("/tmp/.X11-unix/X{num}"));
                    if sock.exists() {
                        let unix = PathBuf::from("/tmp/.X11-unix");
                        let bind = (unix.clone(), unix);
                        if !plan.binds.contains(&bind) {
                            plan.binds.push(bind);
                        }
                    }
                }
            }
        }

        if let Some(xauth) = &self.xauthority {
            let target = PathBuf::from("/tmp/.intune-container-xauth");
            plan.binds.push((xauth.clone(), target.clone()));
            plan.env
                .push(("XAUTHORITY".into(), target.display().to_string()));
        }

        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_display_number() {
        assert_eq!(extract_display_number(":0"), Some(0));
        assert_eq!(extract_display_number(":1"), Some(1));
        assert_eq!(extract_display_number(":0.0"), Some(0));
        assert_eq!(extract_display_number(":10.0"), Some(10));
        assert_eq!(extract_display_number("localhost:0"), Some(0));
        assert_eq!(extract_display_number(""), None);
    }

    #[test]
    fn attach_plan_wayland_binds_socket_and_sets_env() {
        let info = DisplayInfo {
            wayland_socket: Some(PathBuf::from("/run/user/1000/wayland-1")),
            x11_display: None,
            xauthority: None,
            has_abstract_x11: false,
        };
        let plan = info.attach_plan(1000);
        // Bound to a stable path outside /run/user/<uid> (which gets a tmpfs).
        assert_eq!(
            plan.binds,
            vec![(
                PathBuf::from("/run/user/1000/wayland-1"),
                PathBuf::from("/run/host-wayland")
            )]
        );
        assert!(plan
            .env
            .contains(&("WAYLAND_DISPLAY".into(), "/run/host-wayland".into())));
        assert!(plan
            .env
            .contains(&("XDG_RUNTIME_DIR".into(), "/run/user/1000".into())));
    }

    #[test]
    fn attach_plan_abstract_x11_needs_no_bind() {
        let info = DisplayInfo {
            wayland_socket: None,
            x11_display: Some(":1".into()),
            xauthority: None,
            has_abstract_x11: true,
        };
        let plan = info.attach_plan(1000);
        assert!(plan.binds.is_empty());
        assert!(plan.env.contains(&("DISPLAY".into(), ":1".into())));
    }
}
