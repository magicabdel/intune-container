//! A private, invisible X display for one sign-in.
//!
//! WHY IT IS OURS AND NOT THE USER'S. XTEST input goes wherever the display's
//! input focus is, so typing a password into a *shared* display would be a race
//! with whatever else is on it. This one is created for the sign-in, holds
//! nothing else, and is destroyed with the command — which also means a headless
//! server needs no compositor, no VNC server and no viewer.

use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};
use tracing::debug;

/// Where to start looking for a display number. Well clear of `:0`/`:1` (a real
/// session, or the VNC display a human might be using) and of `:99`, which the
/// container's own compliance agent runs on.
const FIRST_DISPLAY: u32 = 77;

/// A running `Xvfb`, killed when this value is dropped.
pub struct Xvfb {
    number: u32,
    child: Child,
}

impl Xvfb {
    /// Start an invisible display of `width × height`. `number` picks the display
    /// number; `None` takes the first free one.
    pub fn start(width: u32, height: u32, number: Option<u32>) -> Result<Self> {
        if let Some(number) = number {
            // A number the reader named is used or reported, never quietly swapped
            // for another: they picked it to reach it.
            return Self::start_on(width, height, number);
        }

        // Picking a free number and starting a server on it are two steps, and
        // between them another process can take the number: two screen shares can
        // start in the same second, and the test suite starts two servers at once
        // on purpose.
        //
        // The number itself says which failure this was, so no message is parsed:
        // a display that is taken NOW, having been free a moment ago, was taken by
        // somebody else, and the next number is worth trying. A failure that leaves
        // the number free — Xvfb not installed, a refused geometry — is the
        // reader's, and it is reported as it is rather than tried 22 more times.
        let mut lost = Vec::new();
        loop {
            let number = first_free_display(|n| display_taken(n) || lost.contains(&n))
                .with_context(|| {
                    format!("no free X display number between :{FIRST_DISPLAY} and :{LAST_DISPLAY}")
                })?;
            match Self::start_on(width, height, number) {
                Err(e) if display_taken(number) => {
                    debug!(
                        number,
                        "another process took this display number first: {e:#}"
                    );
                    lost.push(number);
                }
                other => return other,
            }
        }
    }

    /// Start a server on one display number, with no search and no retry.
    fn start_on(width: u32, height: u32, number: u32) -> Result<Self> {
        // Xvfb's own diagnostics go to a file rather than to /dev/null: "Server is
        // already active for display 77" is the difference between a bug in this
        // code and a display number that is genuinely taken.
        let log_path = format!("/tmp/intune-container-xvfb-{number}.log");
        let log = std::fs::File::create(&log_path)
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());

        let child = Command::new("Xvfb")
            .arg(format!(":{number}"))
            .args(["-screen", "0", &format!("{width}x{height}x24")])
            // No TCP, ever: the sign-in page is on this display, and the machine
            // may be on a network where a listening X server is reachable.
            .arg("-nolisten")
            .arg("tcp")
            // Survive the last client disconnecting, so the display outlives one
            // portal window and the viewer keeps working across a relaunch.
            .arg("-noreset")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .context(
                "cannot start Xvfb — install it (Debian/Ubuntu: sudo apt install xvfb) and retry",
            )?;

        let mut xvfb = Self { number, child };
        xvfb.wait_until_ready(&log_path)?;
        debug!(display = %xvfb.display(), width, height, "private X display up");
        Ok(xvfb)
    }

    /// The `DISPLAY` value for this server, for example `":77"`.
    pub fn display(&self) -> String {
        format!(":{}", self.number)
    }

    /// Block until the display ANSWERS, or fail with the reason it did not.
    ///
    /// It really has to connect: the socket file appears at `bind` time, before
    /// the server `listen`s, so a wait that only stats the path hands back a
    /// display whose first connection is refused. That cost an hour once — the
    /// socket was there, the server was not, and the error read like a bug in the
    /// X code rather than a race in this function.
    ///
    /// A dead child is reported ahead of the timeout, with what Xvfb said, because
    /// "already active for display 77" needs a different fix from "not installed".
    fn wait_until_ready(&mut self, log_path: &str) -> Result<()> {
        let display = self.display();
        for _ in 0..100 {
            if let Ok(Some(status)) = self.child.try_wait() {
                let log = std::fs::read_to_string(log_path).unwrap_or_default();
                let said = said_why(&log);
                anyhow::bail!(
                    "Xvfb exited immediately ({status}){}{}",
                    if said.is_empty() { "" } else { ": " },
                    said
                );
            }
            if x11rb::connect(Some(&display)).is_ok() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        anyhow::bail!("Xvfb did not answer on {display} within five seconds (see {log_path})")
    }
}

impl Drop for Xvfb {
    /// Ask the server to stop before killing it, so it removes its own socket and
    /// lock file. A SIGKILL leaves both behind, and the next run then finds a
    /// stale `/tmp/.X<n>-lock` — which is how a display number that nothing is
    /// using stops being usable.
    fn drop(&mut self) {
        let pid = nix::unistd::Pid::from_raw(self.child.id() as i32);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        for _ in 0..40 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The last display number this tool creates one on.
///
/// `:99` is deliberately outside it. The container's own compliance agent runs a
/// display there, and its socket lands in the `/tmp/.X11-unix` that is bound in —
/// so it looks like one of ours from the host, and a viewer that took it would
/// stream a 640×480 agent instead of a sign-in.
const LAST_DISPLAY: u32 = 98;

/// Every display number this tool may have created, lowest first.
///
/// The screen share walks it to find a display that a portal is already drawing
/// on, because starting a second, empty one is how a reader ends up streaming
/// black pixels.
pub fn private_displays() -> impl Iterator<Item = u32> {
    FIRST_DISPLAY..=LAST_DISPLAY
}

/// Is a display number already in use? True when either the X socket or the lock
/// file exists — the lock is what a display leaves behind when its server dies
/// badly, and starting a second server on it fails.
pub fn display_taken(number: u32) -> bool {
    Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
        || Path::new(&format!("/tmp/.X{number}-lock")).exists()
}

/// The first free display number in this tool's range, or `None`.
fn first_free_display(taken: impl Fn(u32) -> bool) -> Option<u32> {
    (FIRST_DISPLAY..=LAST_DISPLAY).find(|n| !taken(*n))
}

/// The line of Xvfb's output that says why it exited.
///
/// Not the first line: Xvfb opens with the xkbcomp keymap warnings, which are
/// pages long on a current keyboard map and say nothing about the exit. The reason
/// is on an error line after them — `(EE) Server is already active for display 77`
/// — so that is what the message carries.
fn said_why(log: &str) -> String {
    // A bare `(EE)` is a separator and carries nothing, so a line is judged by what
    // is left of it once the marker is off.
    let said: Vec<(bool, &str)> = log
        .lines()
        .map(|line| {
            let line = line.trim();
            match line.strip_prefix("(EE)") {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            }
        })
        .filter(|(_, text)| !text.is_empty())
        .collect();
    said.iter()
        .find(|(marked, _)| *marked)
        .or_else(|| said.first())
        .map(|(_, text)| text.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_free_number_is_taken() {
        assert_eq!(first_free_display(|_| false), Some(77));
        assert_eq!(first_free_display(|n| n < 79), Some(79));
    }

    #[test]
    fn a_full_range_is_reported_rather_than_guessed() {
        // Better a clear failure than a second server fighting for :77.
        assert_eq!(first_free_display(|_| true), None);
    }

    #[test]
    fn the_reason_is_read_past_the_keymap_warnings() {
        // Real output from a display number that was already taken. Reporting the
        // first line here gives "The XKEYBOARD keymap compiler (xkbcomp) reports:",
        // which sends the reader after a keyboard problem they do not have.
        let log = "The XKEYBOARD keymap compiler (xkbcomp) reports:\n\
                   > Warning:          Could not resolve keysym XF86CameraAccessEnable\n\
                   > Warning:          Could not resolve keysym XF86NextElement\n\
                   Errors from xkbcomp are not fatal to the X server\n\
                   (EE) \n\
                   (EE) Server is already active for display 77\n\
                   \tIf this server is no longer running, remove /tmp/.X77-lock\n";
        assert_eq!(said_why(log), "Server is already active for display 77");
    }

    #[test]
    fn an_output_with_no_error_line_still_says_something() {
        assert_eq!(said_why("Fatal server error:\n"), "Fatal server error:");
        assert_eq!(said_why(""), "");
        // A bare marker carries nothing, so it is not the line chosen.
        assert_eq!(said_why("(EE)\nreal reason here"), "real reason here");
    }

    #[test]
    fn a_lost_number_is_skipped_on_the_next_pass() {
        // The search takes the next one rather than trying 77 for ever.
        let lost = [77u32];
        let found = first_free_display(|n| lost.contains(&n));
        assert_eq!(found, Some(78));
    }

    #[test]
    fn the_scan_stops_before_the_agents_display() {
        // :99 is the container's compliance agent. A viewer that streamed it would
        // show a 640×480 service, not a sign-in.
        let numbers: Vec<u32> = private_displays().collect();
        assert_eq!(numbers.first(), Some(&77));
        assert_eq!(numbers.last(), Some(&98));
        assert!(!numbers.contains(&99));
    }
}
