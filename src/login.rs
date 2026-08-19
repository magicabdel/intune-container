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
//! `--web` swaps that last part for a browser ([`crate::webview`]): the same
//! display, served over the tailnet at its own resolution. Everything else — the
//! private display, the portal, the automation, the teardown — is shared.
//!
//! Everything it starts, it stops: the portal, the display forwarding and the X
//! server are all torn down on the way out, whether the reader quits or the
//! window closes.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::sys::termios::{self, SetArg, Termios};

use crate::termview::{self, Action, Cell, Frame, Key, Viewport};
use crate::xscreen::XScreen;
use crate::xvfb::Xvfb;
use crate::{display, ops, webview};

/// How the reader asked for the session to look.
pub struct Options {
    /// The size of the private display, in pixels.
    pub width: u32,
    pub height: u32,
    /// Force a display number instead of taking the first free one.
    pub display: Option<u32>,
    /// Drive the window: press Sign in, and fill the form when `credentials` are
    /// given. False hands the window over untouched.
    pub automatic: bool,
    /// The address and password to fill in.
    ///
    /// `None` is the common case and not a lesser one: a device that has signed in
    /// before has its account remembered, so Entra goes straight from Sign in to the
    /// Authenticator prompt and there is no field to fill. Measured on this tenant —
    /// typing an address at that point reaches nothing at all.
    pub credentials: Option<Credentials>,
    /// Serve the window to a browser instead of drawing it in this terminal. The
    /// display, the portal and the teardown are the same either way — only the
    /// viewer changes.
    pub web: Option<webview::Options>,
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
            automatic: true,
            credentials: None,
            web: None,
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
    // Only the terminal viewer needs a terminal. `--web` draws nowhere near this
    // tty, so it stays usable from a script, a systemd unit or a pipe.
    if options.web.is_none()
        && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
    {
        anyhow::bail!(
            "login needs a terminal on both stdin and stdout — run it directly, not through a pipe, \
             or serve the window to a browser with --web"
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

    ops::portal_start(&display_info).context("could not start the Intune portal")?;

    // Fill the form before the terminal is taken over, so every step is a plain
    // line the reader can keep.
    if options.automatic {
        automate(&screen, options.credentials.as_ref())?;
    }
    let outcome = match &options.web {
        Some(web) => webview::serve(&screen, web),
        None => {
            print_keys(&display_name);
            let tty = RawTty::enter()?;
            view(&screen, &tty)
        }
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

/// What the command types into the sign-in page. Deliberately not `Debug`: a
/// derived one would put the password in any log line that printed the options.
pub struct Credentials {
    pub email: String,
    pub password: String,
}

/// Ask for the address and the password on the terminal. The password is read
/// without echo, and neither value is ever logged or written anywhere.
pub fn prompt_credentials(email: Option<String>) -> Result<Credentials> {
    let email = match email {
        Some(email) => email,
        None => {
            eprint!("Work address: ");
            std::io::stderr().flush().ok();
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context("cannot read the address")?;
            line.trim().to_string()
        }
    };
    if email.is_empty() {
        anyhow::bail!("no address given");
    }
    let password =
        rpassword::prompt_password("Password (not shown): ").context("cannot read the password")?;
    if password.is_empty() {
        anyhow::bail!("no password given");
    }
    Ok(Credentials { email, password })
}

/// How long a step waits for the page to answer. Generous: the portal's first
/// window can take half a minute, and Entra's own pages are a network round trip
/// away.
const STEP_TIMEOUT: Duration = Duration::from_secs(90);

/// A screen is "settled" once it has not moved by more than this fraction for
/// [`STILL_FOR`]. Small enough to ignore a text cursor blinking, large enough that
/// a spinner does not hold the flow open for ever.
const STILL_ENOUGH: f32 = 0.01;
const STILL_FOR: Duration = Duration::from_millis(1200);

/// A change this large means the page moved on — a new dialog, a new step.
/// A change this large is a new PAGE, not a focus ring or a caret. Measured: the
/// focus ring Tab draws on the portal moves about 3% of the pixels, and treating
/// that as a press is what made the first version report a sign-in that never
/// happened.
const NEW_PAGE: f32 = 0.15;

/// Drive the sign-in: press the button, fill the address, fill the password.
///
/// WHAT IT CANNOT KNOW. There is no DOM behind this window — the broker renders it
/// in its own WebKit view — so the flow is timed on the PICTURE: type, wait for it
/// to move, wait for it to settle, type again. That is why every step says what it
/// did, and why the viewer opens afterwards instead of the command claiming
/// success: if a page ever appears in a different order, the reader sees the real
/// screen and can finish it by hand.
///
/// The one thing it never guesses is the multi-factor challenge. Approving that is
/// the reader's, on their phone.
fn automate(screen: &XScreen, credentials: Option<&Credentials>) -> Result<()> {
    eprintln!("· waiting for the portal window");
    wait_until_still(screen, STEP_TIMEOUT)?;
    // Place it BEFORE the baseline is taken. place_and_focus moves the window to
    // the origin, which is a huge pixel change and a wholesale change of
    // coordinates: measuring the button first meant clicking where the window used
    // to be, and then reading its own move as the page having opened.
    screen.place_and_focus()?;
    let settled = wait_until_still(screen, STEP_TIMEOUT)?;

    eprintln!("· pressing Sign in");
    let settled = match press_sign_in(screen, &settled)? {
        Some(frame) => frame,
        None => {
            eprintln!("  the window did not react — finish it in the viewer below");
            return Ok(());
        }
    };
    let _ = wait_until_still(screen, STEP_TIMEOUT)?;
    let _ = settled;

    let Some(credentials) = credentials else {
        eprintln!("· signed in as the remembered account — nothing to type");
        eprintln!("  the screen below is what the sign-in shows now");
        return Ok(());
    };

    eprintln!("· typing the address");
    screen.place_and_focus()?;
    report_refused(screen.type_text(&credentials.email)?);
    screen.press(Key::Return)?;
    wait_until_still(screen, STEP_TIMEOUT)?;

    eprintln!("· typing the password");
    screen.place_and_focus()?;
    report_refused(screen.type_text(&credentials.password)?);
    screen.press(Key::Return)?;
    wait_until_still(screen, STEP_TIMEOUT)?;

    eprintln!("· done typing — the screen below is what the sign-in shows now");
    Ok(())
}

/// Where the portal's Sign in button sits inside its window, as a fraction of the
/// window. Measured on the real portal: the button is centred across a 478-pixel
/// dialog and 420 pixels down its 628.
const SIGN_IN_AT: (f32, f32) = (0.50, 0.67);

/// Activate the portal's Sign in button, and return the screen it led to.
///
/// The click comes FIRST, because it is the only attempt that can be verified: a
/// real activation replaces the dialog with the sign-in page, which is a large
/// change, while Tab merely draws a focus ring — and that ring cost an hour, because
/// it changed enough pixels to look exactly like a press. So the threshold here is
/// deliberately high, and the keyboard is the fallback rather than the first try.
///
/// `None` means nothing moved the screen, and the caller hands the window to the
/// reader rather than pretending it signed in.
fn press_sign_in(screen: &XScreen, before: &Frame) -> Result<Option<Frame>> {
    let bounds = termview::content_bounds(before);
    let x = (bounds.x as f32 + bounds.width as f32 * SIGN_IN_AT.0).round() as i16;
    let y = (bounds.y as f32 + bounds.height as f32 * SIGN_IN_AT.1).round() as i16;
    screen.click(x, y, true)?;
    std::thread::sleep(Duration::from_millis(80));
    screen.click(x, y, false)?;
    if let Some(frame) = wait_for_page(screen, before, Duration::from_secs(25))? {
        eprintln!("  clicked the button at {x}, {y}");
        return Ok(Some(frame));
    }

    // Keyboard fallback, for a portal whose button has moved: Tab walks the focus
    // ring, and Enter or space presses whatever holds it.
    for step in 1..=6 {
        screen.press(Key::Tab)?;
        std::thread::sleep(Duration::from_millis(120));
        screen.press(Key::Return)?;
        screen.press(Key::Space)?;
        if let Some(frame) = wait_for_page(screen, before, Duration::from_secs(4))? {
            eprintln!("  activated with the keyboard ({step} tabs)");
            return Ok(Some(frame));
        }
    }
    Ok(None)
}

/// Wait for a change big enough to BE a new page rather than a focus ring.
fn wait_for_page(screen: &XScreen, base: &Frame, timeout: Duration) -> Result<Option<Frame>> {
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let current = screen.capture()?;
        if termview::difference(base, &current) > NEW_PAGE {
            return Ok(Some(current));
        }
        if start.elapsed() >= timeout {
            return Ok(None);
        }
        if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("interrupted");
        }
    }
}

/// Say which characters this keyboard could not type, rather than letting a
/// half-typed password fail as a wrong one.
fn report_refused(refused: Vec<char>) {
    if !refused.is_empty() {
        eprintln!(
            "  warning: this display's keyboard cannot type {refused:?} — that field is incomplete"
        );
    }
}

/// Wait until the picture stops moving, and return it.
fn wait_until_still(screen: &XScreen, timeout: Duration) -> Result<Frame> {
    let start = Instant::now();
    let mut previous = screen.capture()?;
    let mut still_since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let current = screen.capture()?;
        if termview::difference(&previous, &current) > STILL_ENOUGH {
            still_since = Instant::now();
        }
        previous = current;
        if still_since.elapsed() >= STILL_FOR {
            return Ok(previous);
        }
        if start.elapsed() >= timeout {
            // Not an error: a page that never settles is still a page the reader
            // can look at and use.
            return Ok(previous);
        }
        if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("interrupted");
        }
    }
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
///
/// [`crate::screen`] shares it: a screen share is run over the same SSH
/// connection, and it leaves the same three things behind.
pub fn catch_hangups() {
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

/// What the viewer ended on. Either viewer — the terminal one below, or the
/// browser one in [`crate::webview`] — reports the session the same way, so the
/// teardown in [`run`] is written once.
pub struct Session {
    /// True when the portal exited by itself (the reader closed the window),
    /// false when the reader quit the viewer with the window still open.
    pub portal_gone: bool,
}

/// Has the session been cut from outside (a dropped SSH connection, Ctrl+C)?
///
/// Both viewers poll this rather than dying in the signal handler, so the ordinary
/// teardown runs: the portal is closed, the container goes back to headless and the
/// X server is stopped.
pub fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Draw the display and forward input until the reader quits or the window goes.
fn view(screen: &XScreen, tty: &RawTty) -> Result<Session> {
    let (mut cols, mut rows) = tty.size();
    // Fit the WINDOW, not the desktop it sits on. The dialog is a third of the
    // screen, so fitting the screen shrank it past reading.
    let mut viewport = match screen.capture() {
        Ok(frame) => {
            Viewport::at_actual_size(termview::content_bounds(&frame), cols, picture_rows(rows))
        }
        Err(_) => Viewport::fitted(screen.width, screen.height, cols, picture_rows(rows)),
    };
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
                    // Fit the window again, wherever it has moved to — the sign-in
                    // opens a second, differently sized one partway through.
                    viewport = match screen.capture() {
                        Ok(frame) => Viewport::fitted_to(
                            termview::content_bounds(&frame),
                            cols,
                            picture_rows(rows),
                        ),
                        Err(_) => {
                            Viewport::fitted(screen.width, screen.height, cols, picture_rows(rows))
                        }
                    }
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
    eprintln!("The window is now drawn below (display {display}). If a two-digit number is");
    eprintln!("on it, enter that number in Authenticator; press F2 to make it bigger.");
    eprintln!();
    for (keys, what) in termview::HELP {
        eprintln!("  {keys:<20} {what}");
    }
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
        assert!(
            !line.contains("quit1:"),
            "the halves still collide: {line:?}"
        );
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
