//! `intune-container login` — sign in to Entra ID from a terminal, with no
//! screen, no compositor and no VNC.
//!
//! WHY THIS EXISTS. A headless server (an EC2 box, a build machine) still has to
//! sign in interactively: the identity broker's Primary Refresh Token expires,
//! Entra answers `interaction_required`, and every token call fails until a human
//! completes a sign-in. There is no way around the *window*:
//!
//! * the broker refuses a device-code flow on Linux — its own binary says
//!   "AcquireTokenWithDeviceCodeFlow is not implemented on Linux platform"; and
//! * it renders the sign-in itself, in an embedded WebKitGTK view on an X display
//!   (`Msai::EmbeddedBrowserImpl`, `XOpenDisplay`), which no API can bypass.
//!
//! So this command does not replace the window — it brings it to the terminal. It
//! owns a private [`Xvfb`](crate::xvfb), launches the Intune portal on it, draws
//! that display with half-block characters ([`crate::termview`]) and sends the
//! keys and clicks back with XTEST ([`crate::xscreen`]). The reader types their
//! address and password in the terminal and reads the Authenticator number there.
//!
//! Everything it starts, it stops: the portal, the display forwarding and the X
//! server are all torn down on the way out, whether the reader quits or the
//! window closes.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::sys::termios::{self, SetArg, Termios};

use crate::termview::{self, Action, Cell, Frame, Viewport};
use crate::xscreen::XScreen;
use crate::xvfb::Xvfb;
use crate::{display, ops};

/// How the reader asked for the session to look.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// The size of the private display, in pixels.
    pub width: u32,
    pub height: u32,
    /// Force a display number instead of taking the first free one.
    pub display: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        // 1280×800 is the smallest size at which the Entra sign-in page keeps its
        // desktop layout; narrower and it reflows to the phone one, which puts the
        // number-matching code in a different place every step.
        Self {
            width: 1280,
            height: 800,
            display: None,
        }
    }
}

/// Read a `WIDTHxHEIGHT` geometry.
///
/// Bounded on both sides: under 640×480 the sign-in page has nowhere to put a
/// dialog, and a display far larger than the terminal is pixels nobody can read
/// (and a capture that costs more than it shows).
pub fn parse_geometry(text: &str) -> Result<(u32, u32)> {
    let (w, h) = text
        .split_once(['x', 'X'])
        .with_context(|| format!("geometry {text:?} is not WIDTHxHEIGHT, for example 1280x800"))?;
    let width: u32 = w
        .trim()
        .parse()
        .with_context(|| format!("geometry width {w:?} is not a number"))?;
    let height: u32 = h
        .trim()
        .parse()
        .with_context(|| format!("geometry height {h:?} is not a number"))?;
    if !(640..=3840).contains(&width) || !(480..=2160).contains(&height) {
        anyhow::bail!("geometry {width}x{height} is outside 640x480 … 3840x2160");
    }
    Ok((width, height))
}

/// How long the viewer waits between repaints when nothing is typed. Fast enough
/// that a page transition is not missed, slow enough that a 200×50 diff over SSH
/// costs nothing noticeable.
const FRAME: Duration = Duration::from_millis(400);

/// How long a pass waits for a keystroke before redrawing. Short enough that
/// typing feels immediate, long enough that an idle session costs no CPU.
const INPUT_POLL: Duration = Duration::from_millis(20);

/// How often the container is asked whether the portal is still up. It is a
/// subprocess probe, so it is deliberately much rarer than a frame.
const PORTAL_POLL: Duration = Duration::from_secs(3);

/// Run the whole sign-in session.
pub fn run(options: Options) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "login needs a terminal on both stdin and stdout — run it directly, not through a pipe"
        );
    }

    catch_hangups();

    let xvfb = Xvfb::start(options.width, options.height, options.display)?;
    let display_name = xvfb.display();
    let screen = XScreen::connect(&display_name)?;

    // The container gets the display the same way `edge` and `enroll` get the
    // user's: the socket directory is bound in and DISPLAY names ours.
    let display_info = display::DisplayInfo {
        wayland_socket: None,
        x11_display: Some(display_name.clone()),
        // No cookie: a display we just created has no access control, and handing
        // the container the *user's* Xauthority would be a wider grant than this
        // needs.
        xauthority: None,
        has_abstract_x11: false,
    };

    print_keys(&display_name);
    ops::portal_start(&display_info).context("could not start the Intune portal")?;

    let outcome = {
        let tty = RawTty::enter()?;
        view(&screen, &tty)
    };

    // The tty is restored here, so everything below prints normally.
    let session = outcome?;

    if session.portal_gone {
        eprintln!("The sign-in window closed.");
    } else {
        eprintln!("Leaving the sign-in — closing the window.");
        if let Err(e) = ops::portal_stop() {
            eprintln!("Could not close the portal: {e:#}");
        }
    }
    if let Err(e) = ops::portal_finish() {
        eprintln!("Could not return the container to headless: {e:#}");
    }
    eprintln!();
    eprintln!("Check what the broker holds now with:  intune-container sso-test");
    eprintln!("An app that reads it (teams-lite, a browser with linux-entra-sso) picks");
    eprintln!("the new token up on its own — nothing else to restart.");
    Ok(())
}

/// Set by [`catch_hangups`] when the session is cut from outside. The viewer polls
/// it so the ordinary teardown runs — the portal is closed, the container goes back
/// to headless and the X server is stopped.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_signal(_: nix::libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Turn a hangup into a normal exit.
///
/// This matters more here than in most commands: a sign-in on a headless box is
/// run over SSH, and a dropped connection sends SIGHUP. Without this the process
/// dies with no unwinding, which leaves an X server running for nobody, a portal
/// drawing into it, and the container still forwarding a display — none of which
/// the next run can tell from a session in progress.
fn catch_hangups() {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
    let action = SigAction::new(
        SigHandler::Handler(on_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for signal in [Signal::SIGHUP, Signal::SIGTERM, Signal::SIGINT] {
        // SAFETY: the handler only stores into an atomic, which is async-signal-safe.
        unsafe {
            let _ = sigaction(signal, &action);
        }
    }
}

/// What the viewer ended on.
struct Session {
    /// True when the portal exited by itself (the reader closed the window),
    /// false when the reader quit the viewer with the window still open.
    portal_gone: bool,
}

/// Draw the display and forward input until the reader quits or the window goes.
fn view(screen: &XScreen, tty: &RawTty) -> Result<Session> {
    let (mut cols, mut rows) = tty.size();
    let mut viewport = Viewport::fitted(screen.width, screen.height, cols, picture_rows(rows));
    let mut previous: Option<Vec<Cell>> = None;
    let mut input = Vec::new();
    let mut help = false;
    let mut last_frame = Instant::now() - FRAME;
    let mut last_probe = Instant::now();
    let mut portal_seen = false;

    loop {
        if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(Session { portal_gone: false });
        }

        // 1. Input first: a keystroke must not wait for the next frame. The poll is
        // what keeps the loop responsive without a non-blocking terminal, and its
        // timeout is the loop's whole idle cost.
        if wait_for_input(INPUT_POLL)? {
            let mut chunk = [0u8; 1024];
            match std::io::stdin().read(&mut chunk) {
                Ok(0) => return Ok(Session { portal_gone: false }),
                Ok(n) => input.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("cannot read the terminal"),
            }
        }

        let (actions, used) = termview::decode(&input);
        input.drain(..used);
        let mut dirty = !actions.is_empty();
        for action in actions {
            if help {
                // Any key dismisses the key list, and the frame under it is
                // repainted whole.
                help = false;
                previous = None;
            }
            match action {
                Action::Quit => return Ok(Session { portal_gone: false }),
                Action::Help => {
                    help = true;
                }
                Action::Redraw => previous = None,
                Action::Fit => {
                    viewport = Viewport::fitted(screen.width, screen.height, cols, picture_rows(rows))
                }
                Action::Actual => {
                    viewport.scale = 1.0;
                    viewport.clamp(screen.width, screen.height, cols, picture_rows(rows));
                }
                Action::ZoomIn => {
                    viewport.zoom(0.5, screen.width, screen.height, cols, picture_rows(rows))
                }
                Action::ZoomOut => {
                    viewport.zoom(2.0, screen.width, screen.height, cols, picture_rows(rows))
                }
                Action::Pan(dx, dy) => viewport.pan(
                    dx as f32,
                    dy as f32,
                    screen.width,
                    screen.height,
                    cols,
                    picture_rows(rows),
                ),
                Action::Type(c) => {
                    screen.place_and_focus()?;
                    if !screen.type_char(c)? {
                        tty.status(&format!("this display's keyboard cannot type {c:?}"))?;
                    }
                }
                Action::Press(key) => {
                    screen.place_and_focus()?;
                    screen.press(key)?;
                }
                Action::Click { col, row, down } => {
                    if row < picture_rows(rows) {
                        let (x, y) = viewport.pixel_at(col, row);
                        screen.click(x, y, down)?;
                        // A click is how a window gets the focus with no window
                        // manager around, so let it, then take it back for keys.
                        if !down {
                            screen.place_and_focus()?;
                        }
                    }
                }
            }
        }

        // 2. A resized terminal invalidates the grid and the fit.
        let (c, r) = tty.size();
        if (c, r) != (cols, rows) {
            cols = c;
            rows = r;
            viewport.clamp(screen.width, screen.height, cols, picture_rows(rows));
            previous = None;
            dirty = true;
        }

        // 3. Has the window come or gone?
        if last_probe.elapsed() >= PORTAL_POLL {
            last_probe = Instant::now();
            let running = ops::portal_is_running();
            if running {
                portal_seen = true;
            } else if portal_seen {
                return Ok(Session { portal_gone: true });
            }
        }

        // 4. Paint.
        if dirty || last_frame.elapsed() >= FRAME {
            last_frame = Instant::now();
            // Before the picture, not only before a keystroke: a window that has
            // just appeared has to be moved into view and given the focus, and the
            // reader may look at it for a while before typing anything.
            screen.place_and_focus()?;
            let frame = screen.capture()?;
            let cells = termview::sample(&frame, &viewport, cols, picture_rows(rows));
            let mut out = termview::paint(previous.as_deref(), &cells, cols, picture_rows(rows));
            previous = Some(cells);
            if help {
                out.push_str(&help_overlay(cols));
                previous = None;
            }
            out.push_str(&status_line(&viewport, &frame, screen, cols, rows));
            tty.write(&out)?;
        }

    }
}

/// Is there something to read on the terminal within `timeout`?
///
/// `Ok(false)` on a signal as well as on a timeout: the caller re-checks
/// [`INTERRUPTED`] at the top of every pass, so an interrupted poll is simply the
/// end of this pass.
fn wait_for_input(timeout: Duration) -> Result<bool> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;

    let stdin = std::io::stdin();
    let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
    let millis = timeout.as_millis().min(u16::MAX as u128) as u16;
    match poll(&mut fds, PollTimeout::from(millis)) {
        Ok(ready) => Ok(ready > 0),
        Err(nix::errno::Errno::EINTR) => Ok(false),
        Err(e) => Err(e).context("cannot wait on the terminal"),
    }
}

/// Rows the picture gets: everything but the status line.
fn picture_rows(rows: u16) -> u16 {
    rows.saturating_sub(1).max(1)
}

/// The bar along the bottom: what is on the display, and how to drive it.
fn status_line(
    viewport: &Viewport,
    frame: &Frame,
    screen: &XScreen,
    cols: u16,
    rows: u16,
) -> String {
    let zoom = if viewport.scale <= 1.001 {
        format!("×{:.0}", 1.0 / viewport.scale)
    } else {
        format!("1:{:.1}", viewport.scale)
    };
    let state = if screen.has_window() {
        "sign-in window"
    } else {
        "waiting for the window…"
    };
    let right = format!("{}  {}×{}  {}  ", zoom, frame.width, frame.height, state);
    format!(
        "\x1b[{};1H\x1b[7m\x1b[K{}\x1b[0m",
        rows,
        status_text(&right, cols)
    )
}

/// The status bar's text: the key hints on the left, `right` against the right
/// edge, and spaces between.
///
/// The hints are what gives way when the terminal is narrow — they are a reminder,
/// while the zoom and the window state are the session's only feedback. Without
/// this the two halves ran together on an 80-column terminal ("Ctrl+Q quit1:17.4").
fn status_text(right: &str, cols: u16) -> String {
    const HINTS: &str = " F1 keys · F2/F3 zoom · F4 fit · Ctrl+arrows pan · Ctrl+Q quit";
    let cols = cols as usize;
    let right_width = right.chars().count();
    if right_width >= cols {
        return right.chars().take(cols).collect();
    }
    // At least two spaces between the halves, so they never read as one word.
    let room = cols - right_width;
    let hints: String = HINTS.chars().take(room.saturating_sub(2)).collect();
    let pad = room - hints.chars().count();
    format!("{hints}{}{right}", " ".repeat(pad))
}

/// The key list, drawn over the top-left of the picture.
fn help_overlay(cols: u16) -> String {
    let width = termview::HELP
        .iter()
        .map(|(k, v)| k.chars().count() + v.chars().count() + 3)
        .max()
        .unwrap_or(40)
        .min(cols as usize);
    let mut out = String::from("\x1b[1;1H\x1b[7m");
    for (row, (keys, what)) in termview::HELP.iter().enumerate() {
        let line = format!(" {keys}  {what}");
        let line: String = line.chars().take(width).collect();
        out.push_str(&format!(
            "\x1b[{};1H {:width$} ",
            row + 1,
            line,
            width = width
        ));
    }
    out.push_str("\x1b[0m");
    out
}

/// The key list, printed before the viewer takes the screen. Shown once, because
/// a reader who has never used this needs it before the picture arrives — and the
/// portal takes up to half a minute to draw its first window.
fn print_keys(display: &str) {
    eprintln!();
    eprintln!("The sign-in window will be drawn in this terminal (display {display}).");
    eprintln!();
    for (keys, what) in termview::HELP {
        eprintln!("  {keys:<20} {what}");
    }
    eprintln!();
    eprintln!("Type your address, press Enter, type your password, press Enter. When a");
    eprintln!("two-digit number appears, zoom in with F2 and enter it in Authenticator.");
    eprintln!();
}

/// The terminal, in raw mode with the alternate screen and mouse reporting on.
/// Everything it turns on is turned off when it is dropped — including on the
/// error paths, which is why the viewer never leaves a terminal without an echo.
struct RawTty {
    original: Termios,
}

impl RawTty {
    fn enter() -> Result<Self> {
        let original =
            termios::tcgetattr(std::io::stdin()).context("cannot read the terminal settings")?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &raw)
            .context("cannot put the terminal in raw mode")?;

        // The terminal stays BLOCKING, and input is polled instead. O_NONBLOCK on
        // stdin also applies to stdout — one terminal, one open file description —
        // so a frame larger than the tty's buffer came back `EAGAIN` and the
        // session died with "Resource temporarily unavailable" mid-sign-in. That is
        // exactly the shape of failure that only appears over a slow SSH link.
        let tty = Self { original };
        // Alternate screen, cursor hidden, SGR mouse reporting on.
        tty.write("\x1b[?1049h\x1b[2J\x1b[?25l\x1b[?1000h\x1b[?1006h")?;
        Ok(tty)
    }

    fn write(&self, text: &str) -> Result<()> {
        let mut out = std::io::stdout();
        out.write_all(text.as_bytes())?;
        out.flush()?;
        Ok(())
    }

    /// A one-off message on the status line, for something the reader has to know
    /// now (a character this keyboard cannot produce).
    fn status(&self, message: &str) -> Result<()> {
        let (_, rows) = self.size();
        self.write(&format!("\x1b[{rows};1H\x1b[7m\x1b[K {message}\x1b[0m"))
    }

    fn size(&self) -> (u16, u16) {
        terminal_size()
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        let _ = self.write("\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l");
        let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
    }
}

/// The terminal's size in cells, or a conservative default when it has none.
fn terminal_size() -> (u16, u16) {
    let mut size: nix::libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { nix::libc::ioctl(0, nix::libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 && size.ws_row > 1 {
        (size.ws_col, size.ws_row)
    } else {
        (80, 24)
    }
}

/// `std::io::IsTerminal`, imported here so `run` reads plainly.
use std::io::IsTerminal;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_line_fills_the_width_exactly() {
        let right = "1:17.4  1280×800  sign-in window  ";
        for cols in [10u16, 34, 40, 80, 120, 200] {
            let line = status_text(right, cols);
            assert_eq!(
                line.chars().count(),
                cols as usize,
                "cols {cols} produced {line:?}"
            );
        }
    }

    #[test]
    fn the_hints_give_way_before_the_state_does() {
        let right = "1:17.4  1280×800  sign-in window  ";
        // 80 columns is the case that ran the two halves together.
        let line = status_text(right, 80);
        assert!(line.ends_with(right), "the state was truncated: {line:?}");
        assert!(line.contains("  "), "no gap between the halves: {line:?}");
        assert!(!line.contains("quit1:"), "the halves still collide: {line:?}");
        // Narrower than the state alone: the state wins and nothing panics.
        let line = status_text(right, 12);
        assert_eq!(line.chars().count(), 12);
    }

    #[test]
    fn the_picture_leaves_room_for_the_status_line() {
        assert_eq!(picture_rows(50), 49);
        // A one-row terminal still gets a picture row rather than zero, so the
        // renderer is never asked for an empty grid.
        assert_eq!(picture_rows(1), 1);
        assert_eq!(picture_rows(0), 1);
    }

    #[test]
    fn the_default_display_keeps_the_desktop_layout() {
        let options = Options::default();
        assert!(options.width >= 1280 && options.height >= 800);
        // The default must be a value the parser also accepts, or `--geometry`
        // could not spell the default back.
        assert_eq!(
            parse_geometry("1280x800").unwrap(),
            (options.width, options.height)
        );
    }

    #[test]
    fn a_geometry_is_read_or_refused_with_a_reason() {
        assert_eq!(parse_geometry("1024x768").unwrap(), (1024, 768));
        assert_eq!(parse_geometry(" 1600 X 900 ").unwrap(), (1600, 900));
        for bad in ["1280", "1280*800", "axb", "", "320x240", "8000x800"] {
            assert!(parse_geometry(bad).is_err(), "{bad:?} was accepted");
        }
    }
}
