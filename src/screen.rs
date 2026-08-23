//! `intune-container screen` — the container's screen, shared to a browser.
//!
//! WHY THIS IS NOT `login --web`. That command signs in: it owns a private
//! display, launches the portal, presses Sign in and types the address and the
//! password ([`crate::login::automate`] does the driving), and it closes
//! everything it opened. This one shows. It starts the portal so the first Intune
//! screen is there without a second command, and from that point it touches
//! nothing: every window the container opens — the portal, the identity broker's
//! Authentication dialog, an Edge window — is drawn as it is, and the reader
//! drives it with their own keyboard and mouse.
//!
//! WHY IT LOOKS FOR A DISPLAY BEFORE IT MAKES ONE. `intune-portal` is a single
//! instance. A second launch hands its request to the running one and exits, so a
//! command that always makes a fresh display gets a display with nothing on it:
//! the portal is still drawing on the display of whatever session started it. The
//! browser then streams a black screen, and nothing in the picture says why. So
//! this looks for a display in this tool's range that already has a window and
//! streams that one instead.
//!
//! WHAT IT LEAVES BEHIND. A display it created, with the portal it started, is
//! torn down the way `login` tears its own down. A display it found is left
//! running, because the session that made it is the one that owns it.

use anyhow::{Context, Result};
use tracing::debug;

use crate::xscreen::XScreen;
use crate::xvfb::{self, Xvfb};
use crate::{display, login, ops, webview};

/// How the reader asked for the share to look.
pub struct Options {
    /// The size of the display, when this command has to create one. A display it
    /// finds keeps the size it already has.
    pub width: u32,
    pub height: u32,
    /// Force a display number instead of taking the first free one.
    pub display: Option<u32>,
    /// Where to serve it.
    pub web: webview::Options,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 800,
            display: None,
            web: webview::Options::resolved(None, None),
        }
    }
}

/// Share the container's screen until the reader closes the page or the last
/// window goes.
pub fn run(options: Options) -> Result<()> {
    login::catch_hangups();

    let stage = open(&options)?;
    let screen = XScreen::connect(stage.display_name())
        .with_context(|| format!("cannot read the display {}", stage.display_name()))?;

    let session = webview::serve(&screen, &options.web)?;
    stage.close(session.portal_gone);
    Ok(())
}

/// The display this session streams, and what it had to create to get one.
enum Stage {
    /// A display that already had a window on it. Another session owns it, so
    /// nothing on it is this command's to stop.
    Found(String),
    /// A display this command created, and the portal it started on it.
    Made {
        // Dropping this kills the X server, so it is held for the whole session
        // even though nothing reads it.
        _xvfb: Xvfb,
        name: String,
    },
}

impl Stage {
    fn display_name(&self) -> &str {
        match self {
            Stage::Found(name) => name,
            Stage::Made { name, .. } => name,
        }
    }

    /// Put back what this command changed, and only that.
    fn close(self, portal_gone: bool) {
        match self {
            Stage::Found(name) => {
                eprintln!();
                eprintln!("The share is closed. The screen {name} stays, and so does everything");
                eprintln!("on it: this command did not start it.");
            }
            Stage::Made { name, .. } => {
                eprintln!();
                if portal_gone {
                    eprintln!("The last window on {name} closed.");
                } else {
                    eprintln!("Closing the Intune portal on {name}.");
                    if let Err(e) = ops::portal_stop() {
                        eprintln!("Could not close the portal: {e:#}");
                    }
                }
                if let Err(e) = ops::portal_finish() {
                    eprintln!("Could not return the container to headless: {e:#}");
                }
            }
        }
    }
}

/// Get a display with Intune on it: the one that already has it, or a new one with
/// a portal started on it.
fn open(options: &Options) -> Result<Stage> {
    if let Some(name) = drawing_display(xvfb::display_taken, |name| {
        XScreen::connect(name)
            .map(|screen| screen.has_window())
            .unwrap_or(false)
    }) {
        eprintln!("Sharing the screen the container is already drawing on: {name}");
        eprintln!("This command started nothing on it, so it will close nothing.");
        return Ok(Stage::Found(name));
    }

    // A portal with no display to draw on is a portal nobody can reach: its
    // display went with the session that made it. A second launch would hand the
    // request to it and exit, leaving the new display empty — which is the black
    // screen this whole search exists to prevent. So it goes first.
    if ops::portal_is_running() {
        eprintln!("A portal is running with no screen left to draw on. Closing it, so the");
        eprintln!("one this command starts is the one you see.");
        // `portal_stop` waits for the exit, which is what makes the launch below
        // reach a container with no instance of a single-instance application.
        ops::portal_stop().context("could not close the portal that has no screen")?;
    }

    let xvfb = Xvfb::start(options.width, options.height, options.display)?;
    let name = xvfb.display();
    debug!(display = %name, "made a screen to share");

    // The container reaches it the way `edge` and `enroll` reach the user's: the
    // socket directory is bound in and DISPLAY names this one. No cookie — a
    // display made a moment ago has no access control, and handing the container
    // the user's Xauthority would grant more than this needs.
    let info = display::DisplayInfo {
        wayland_socket: None,
        x11_display: Some(name.clone()),
        xauthority: None,
        has_abstract_x11: false,
    };
    ops::portal_start(&info).context("could not start the Intune portal")?;

    Ok(Stage::Made { _xvfb: xvfb, name })
}

/// The first display in this tool's range that exists and has a window on it.
///
/// Both questions are parameters so a test can decide what exists and what is
/// drawn without an X server and without touching `/tmp`. `exists` is asked first
/// because it is a `stat` and the other one is a connection.
fn drawing_display(
    exists: impl Fn(u32) -> bool,
    has_window: impl Fn(&str) -> bool,
) -> Option<String> {
    xvfb::private_displays()
        .filter(|number| exists(*number))
        .map(|number| format!(":{number}"))
        .find(|name| has_window(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_that_draws_is_the_one_taken() {
        // :77 and :78 both exist, only :78 has a window: a session was left behind
        // on :77 with nothing on it, and the portal is on :78. This is the case
        // that used to hand the reader a black screen.
        let found = drawing_display(|n| n == 77 || n == 78, |name| name == ":78");
        assert_eq!(found.as_deref(), Some(":78"));
    }

    #[test]
    fn a_display_that_does_not_exist_is_never_connected_to() {
        // The `stat` comes first: a connection attempt per free number would be 22
        // of them on every run.
        let asked = std::cell::RefCell::new(Vec::new());
        drawing_display(
            |n| n == 80,
            |name| {
                asked.borrow_mut().push(name.to_string());
                false
            },
        );
        assert_eq!(asked.into_inner(), vec![":80".to_string()]);
    }

    #[test]
    fn no_window_anywhere_means_no_display_to_share() {
        // The caller then makes its own, which is the first-run path.
        assert_eq!(drawing_display(|_| true, |_| false), None);
    }

    #[test]
    fn only_the_private_range_is_offered() {
        // The user's own session (:0) and the container's compliance agent (:99)
        // are never candidates, however much they are drawing.
        let seen = std::cell::RefCell::new(Vec::new());
        drawing_display(
            |_| true,
            |name| {
                seen.borrow_mut().push(name.to_string());
                false
            },
        );
        let seen = seen.into_inner();
        assert!(!seen.contains(&":0".to_string()));
        assert!(!seen.contains(&":99".to_string()));
        for name in &seen {
            let number: u32 = name.trim_start_matches(':').parse().expect("a number");
            assert!((77..=98).contains(&number), "{name} is outside the range");
        }
    }
}
