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
        let number = match number {
            Some(n) => n,
            None => first_free_display(|n| display_taken(n))
                .context("no free X display number between :77 and :99")?,
        };
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
                let said = std::fs::read_to_string(log_path).unwrap_or_default();
                let said = said.trim();
                anyhow::bail!(
                    "Xvfb exited immediately ({status}){}{}",
                    if said.is_empty() { "" } else { ": " },
                    said.lines().next().unwrap_or("")
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

/// Is a display number already in use? True when either the X socket or the lock
/// file exists — the lock is what a display leaves behind when its server dies
/// badly, and starting a second server on it fails.
fn display_taken(number: u32) -> bool {
    Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
        || Path::new(&format!("/tmp/.X{number}-lock")).exists()
}

/// The first free display number in `77..=99`, or `None`.
fn first_free_display(taken: impl Fn(u32) -> bool) -> Option<u32> {
    (FIRST_DISPLAY..=99).find(|n| !taken(*n))
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
}
