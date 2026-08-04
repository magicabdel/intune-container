//! The X11 half of the terminal viewer: read a display's pixels, and put keys
//! and clicks back into it.
//!
//! It talks to an X server that [`crate::login`] owns — a private Xvfb, never the
//! user's own session — and it uses two extensions and nothing else: `GetImage`
//! to read the root window, and XTEST to synthesise input. There is no window of
//! our own, no event loop and no compositor: the sign-in window belongs to the
//! container's Intune portal, which draws into the same display.
//!
//! WHY XTEST AND NOT AN EVENT. A key sent as a synthetic `KeyPress` event is
//! rejected by GTK (it checks `send_event`), so the only way to type into a
//! window we do not own is to make the *server* generate the event. That is what
//! XTEST is for, and it is why the display has to be ours: XTEST input goes
//! wherever the input focus is, which on a shared display would be whatever the
//! user was doing.

use anyhow::{Context, Result};
use tracing::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as _, ImageFormat, InputFocus, MapState, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::termview::{Frame, Key};

/// X11 event types, as XTEST's `fake_input` wants them.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

/// The Shift keysym, needed for every capital letter and every symbol on a
/// shifted key — an email address and a password are full of them.
const SHIFT_L: u32 = 0xffe1;

/// A connection to one X display, with the pixel format and the keyboard map
/// resolved once.
pub struct XScreen {
    conn: RustConnection,
    root: Window,
    pub width: u32,
    pub height: u32,
    /// Bits per pixel of the root depth's image format, and where each channel
    /// sits in the little-endian word. Read from the server, never assumed:
    /// Xvfb's default depth is 24 in a 32-bit word, but a `-depth 16` display
    /// packs 5-6-5 and reading it as BGRX would give a green picture.
    bits_per_pixel: u8,
    channels: [Channel; 3],
    keymap: Keymap,
    shift: u8,
}

#[derive(Debug, Clone, Copy)]
struct Channel {
    shift: u32,
    max: u32,
}

impl Channel {
    fn of(mask: u32) -> Self {
        Self {
            shift: mask.trailing_zeros(),
            max: mask >> mask.trailing_zeros(),
        }
    }

    /// Scale the channel to 0..=255, so a 16-bit display renders with the same
    /// brightness as a 24-bit one.
    fn value(self, word: u32) -> u8 {
        if self.max == 0 {
            return 0;
        }
        let raw = (word >> self.shift) & self.max;
        ((raw * 255) / self.max) as u8
    }
}

/// Keysym → (keycode, needs shift), built from the server's own mapping.
struct Keymap {
    min: u8,
    per_code: u8,
    syms: Vec<u32>,
}

impl Keymap {
    /// The keycode that produces `keysym`, and whether Shift is needed for it.
    /// Only the first two levels are searched: level 0 and 1 are the unshifted
    /// and shifted symbols, and a third-level key needs a modifier this viewer
    /// does not send.
    fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        let per = self.per_code as usize;
        for (index, chunk) in self.syms.chunks(per).enumerate() {
            for level in 0..per.min(2) {
                if chunk.get(level) == Some(&keysym) {
                    let code = self.min as usize + index;
                    return Some((code as u8, level == 1));
                }
            }
        }
        None
    }
}

impl XScreen {
    /// Connect to the named display (for example `":77"`).
    ///
    /// The parameter is `name` rather than `display` on purpose: a `tracing` macro
    /// brings its own `display` value-wrapper into scope, so a variable of that
    /// name cannot be logged.
    pub fn connect(name: &str) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(Some(name))
            .with_context(|| format!("cannot connect to the X display {name}"))?;
        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_num)
            .context("the X server reported no screen")?;
        let root = screen.root;
        let width = screen.width_in_pixels as u32;
        let height = screen.height_in_pixels as u32;

        let depth = screen.root_depth;
        let bits_per_pixel = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .map(|f| f.bits_per_pixel)
            .context("the X server reported no image format for its root depth")?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual)
            .context("the X server reported no visual for its root window")?;
        let channels = [
            Channel::of(visual.red_mask),
            Channel::of(visual.green_mask),
            Channel::of(visual.blue_mask),
        ];

        let count = setup.max_keycode - setup.min_keycode + 1;
        let mapping = conn
            .get_keyboard_mapping(setup.min_keycode, count)
            .context("cannot ask the X server for its keyboard map")?
            .reply()
            .context("the X server refused its keyboard map")?;
        let keymap = Keymap {
            min: setup.min_keycode,
            per_code: mapping.keysyms_per_keycode,
            syms: mapping.keysyms,
        };
        let shift = keymap
            .lookup(SHIFT_L)
            .map(|(code, _)| code)
            .context("the X keyboard map has no Shift key")?;

        debug!("connected to the X display {name} ({width}x{height}, {bits_per_pixel} bpp)");

        Ok(Self {
            conn,
            root,
            width,
            height,
            bits_per_pixel,
            channels,
            keymap,
            shift,
        })
    }

    /// Read the whole root window.
    ///
    /// The root is deliberate: it holds whatever is on the display, including
    /// menus and dialogs that are windows of their own, so nothing the reader has
    /// to see can hide from the capture.
    pub fn capture(&self) -> Result<Frame> {
        let image = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                self.width as u16,
                self.height as u16,
                !0,
            )
            .context("cannot ask the X server for the screen")?
            .reply()
            .context("the X server refused the screen image")?;
        Ok(self.decode(&image.data))
    }

    fn decode(&self, data: &[u8]) -> Frame {
        let bytes = (self.bits_per_pixel as usize + 7) / 8;
        let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);
        for y in 0..self.height as usize {
            for x in 0..self.width as usize {
                let i = (y * self.width as usize + x) * bytes;
                let word = match data.get(i..i + bytes) {
                    Some(px) => px
                        .iter()
                        .enumerate()
                        .fold(0u32, |w, (n, b)| w | ((*b as u32) << (8 * n))),
                    None => 0,
                };
                rgb.push(self.channels[0].value(word));
                rgb.push(self.channels[1].value(word));
                rgb.push(self.channels[2].value(word));
            }
        }
        Frame {
            width: self.width,
            height: self.height,
            rgb,
        }
    }

    /// Put the topmost real window where it can be read, and give it the
    /// keyboard.
    ///
    /// The display has no window manager on purpose — one more moving part, and
    /// the portal shows one window at a time — but that means nothing does the two
    /// things a window manager would, and both were real failures:
    ///
    /// * nothing assigns focus, so XTEST keys went to the root window and
    ///   vanished; and
    /// * nothing places a window, so the portal mapped its dialog at its own
    ///   coordinates — 615,375 on a 1280×800 display, which put the Sign in button
    ///   past the bottom edge where no amount of panning could reach it.
    ///
    /// Moving it to the origin fixes the second: every window then starts at a
    /// corner the viewer can always show. It is called before every keystroke
    /// because the password step is a *new* window, which must be placed and
    /// focused in its turn.
    pub fn place_and_focus(&self) -> Result<()> {
        let Some(window) = self.topmost()? else {
            return Ok(());
        };
        if let Ok(Ok(geometry)) = self.conn.get_geometry(window).map(|c| c.reply()) {
            if geometry.x != 0 || geometry.y != 0 {
                let at = ConfigureWindowAux::new().x(0).y(0);
                let _ = self.conn.configure_window(window, &at);
            }
        }
        self.conn
            .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
            .context("cannot set the input focus")?;
        self.conn.flush()?;
        Ok(())
    }

    /// Whether anything worth showing is on the display yet. The portal can take
    /// half a minute to draw its first window, and a viewer that said nothing
    /// would look broken for that whole time.
    pub fn has_window(&self) -> bool {
        matches!(self.topmost(), Ok(Some(_)))
    }

    /// The topmost mapped child of the root that is big enough to be a real
    /// window. Tooltips and the 1×1 helper windows GTK creates are skipped.
    fn topmost(&self) -> Result<Option<Window>> {
        let tree = self
            .conn
            .query_tree(self.root)
            .context("cannot ask the X server for the window tree")?
            .reply()
            .context("the X server refused the window tree")?;
        // `children` is in bottom-to-top stacking order.
        for &child in tree.children.iter().rev() {
            let Ok(cookie) = self.conn.get_window_attributes(child) else {
                continue;
            };
            let Ok(attrs) = cookie.reply() else { continue };
            if attrs.map_state != MapState::VIEWABLE {
                continue;
            }
            let Ok(cookie) = self.conn.get_geometry(child) else {
                continue;
            };
            let Ok(geometry) = cookie.reply() else { continue };
            if geometry.width >= 80 && geometry.height >= 40 {
                return Ok(Some(child));
            }
        }
        Ok(None)
    }

    /// Type one character. Returns `false` for a character the display's keyboard
    /// map cannot produce, so the caller can say so instead of silently dropping
    /// a letter of a password.
    pub fn type_char(&self, c: char) -> Result<bool> {
        let keysym = keysym_for_char(c);
        let Some((code, shifted)) = self.keymap.lookup(keysym) else {
            return Ok(false);
        };
        self.tap(code, shifted)?;
        Ok(true)
    }

    /// Type a whole string, one key at a time.
    ///
    /// Returns the characters this display's keyboard could not produce, so the
    /// caller can say which ones rather than silently signing in with half a
    /// password. Paced deliberately: a web input with JavaScript handlers on every
    /// keystroke drops characters typed at full XTEST speed.
    pub fn type_text(&self, text: &str) -> Result<Vec<char>> {
        let mut refused = Vec::new();
        for c in text.chars() {
            if !self.type_char(c)? {
                refused.push(c);
            }
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        Ok(refused)
    }

    /// Send a named key.
    pub fn press(&self, key: Key) -> Result<bool> {
        let Some((code, shifted)) = self.keymap.lookup(key.keysym()) else {
            return Ok(false);
        };
        // Shift+Tab is the one binding whose shift comes from the key, not from
        // the keymap level.
        self.tap(code, shifted || key == Key::BackTab)?;
        Ok(true)
    }

    fn tap(&self, code: u8, shifted: bool) -> Result<()> {
        if shifted {
            self.fake(KEY_PRESS, self.shift, 0, 0)?;
        }
        self.fake(KEY_PRESS, code, 0, 0)?;
        self.fake(KEY_RELEASE, code, 0, 0)?;
        if shifted {
            self.fake(KEY_RELEASE, self.shift, 0, 0)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Move the pointer to a pixel and press or release the left button there.
    pub fn click(&self, x: i16, y: i16, down: bool) -> Result<()> {
        self.fake(MOTION_NOTIFY, 0, x, y)?;
        self.fake(if down { BUTTON_PRESS } else { BUTTON_RELEASE }, 1, x, y)?;
        self.conn.flush()?;
        Ok(())
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(type_, detail, 0, self.root, x, y, 0)
            .context("cannot synthesise input (is the XTEST extension present?)")?;
        Ok(())
    }
}

/// The keysym for a character. For everything this viewer sends — printable
/// ASCII — the keysym IS the code point, which is why no table is needed.
fn keysym_for_char(c: char) -> u32 {
    let code = c as u32;
    if (0x20..0x7f).contains(&code) {
        code
    } else {
        // Unicode keysyms are the code point plus this offset. Outside ASCII the
        // keymap of a default Xvfb holds none of them, so `type_char` reports
        // false and the caller says the character cannot be typed.
        0x0100_0000 + code
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_scales_to_eight_bits() {
        // 24-bit: the mask already spans 0..255.
        let red = Channel::of(0x00ff_0000);
        assert_eq!(red.value(0x00ab_0000), 0xab);
        // 16-bit 5-6-5: full green must read as 255, not as 63.
        let green = Channel::of(0x0000_07e0);
        assert_eq!(green.value(0x0000_07e0), 255);
        assert_eq!(green.value(0), 0);
    }

    #[test]
    fn an_ascii_char_is_its_own_keysym() {
        assert_eq!(keysym_for_char('a'), 0x61);
        assert_eq!(keysym_for_char('@'), 0x40);
        assert_eq!(keysym_for_char('.'), 0x2e);
    }

    #[test]
    fn a_keymap_finds_shifted_and_unshifted_symbols() {
        // Two keycodes, two levels each: 'a'/'A' and '1'/'!'.
        let keymap = Keymap {
            min: 8,
            per_code: 2,
            syms: vec![0x61, 0x41, 0x31, 0x21],
        };
        assert_eq!(keymap.lookup(0x61), Some((8, false)));
        assert_eq!(keymap.lookup(0x41), Some((8, true)));
        assert_eq!(keymap.lookup(0x31), Some((9, false)));
        assert_eq!(keymap.lookup(0x21), Some((9, true)));
        assert_eq!(keymap.lookup(0xff0d), None);
    }

    /// The whole X half, against a real server: capture returns the screen, and a
    /// typed line reaches a window that is not ours.
    ///
    /// Ignored by default because it needs `Xvfb` and `xterm` on the machine —
    /// run it with `cargo test -- --ignored --nocapture`. It is the only test that
    /// can catch the two failures unit tests cannot: a pixel format read wrong
    /// (the picture is there but the colours are swapped) and input that goes
    /// nowhere because no window holds the focus.
    #[test]
    #[ignore = "needs Xvfb and xterm on this machine"]
    fn typing_reaches_a_window_on_a_real_display() {
        use std::path::Path;
        use std::process::{Command, Stdio};

        for tool in ["Xvfb", "xterm"] {
            if Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {tool}"))
                .stdout(Stdio::null())
                .status()
                .map(|s| !s.success())
                .unwrap_or(true)
            {
                eprintln!("skipped: {tool} is not installed");
                return;
            }
        }

        let proof = format!("/tmp/intune-container-xtest-{}", std::process::id());
        let _ = std::fs::remove_file(&proof);

        let xvfb = crate::xvfb::Xvfb::start(800, 600, None).expect("start Xvfb");
        let screen = XScreen::connect(&xvfb.display()).expect("connect");
        assert_eq!((screen.width, screen.height), (800, 600));

        // A blank root is black; xterm's default background is white, so the
        // capture must change once the window is up. That is the pixel-format
        // check: read with the wrong masks and white does not come back white.
        let blank = screen.capture().expect("capture the blank display");
        assert_eq!(blank.rgb.len(), 800 * 600 * 3);

        let mut xterm = Command::new("xterm")
            .env("DISPLAY", xvfb.display())
            .args(["-geometry", "80x24+0+0", "-bg", "white", "-fg", "black"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn xterm");

        let mut seen_white = false;
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if screen.has_window() {
                let frame = screen.capture().expect("capture with a window");
                // Somewhere in the top-left there must now be a white pixel.
                seen_white = frame
                    .rgb
                    .chunks(3)
                    .any(|p| p[0] > 200 && p[1] > 200 && p[2] > 200);
                if seen_white {
                    break;
                }
            }
        }
        assert!(seen_white, "the window never appeared, or the pixel format is wrong");

        screen.place_and_focus().expect("focus");
        for c in format!("touch {proof}").chars() {
            assert!(screen.type_char(c).expect("type"), "cannot type {c:?}");
        }
        screen.press(Key::Return).expect("press Return");

        let mut landed = false;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if Path::new(&proof).exists() {
                landed = true;
                break;
            }
        }
        let _ = xterm.kill();
        let _ = xterm.wait();
        let _ = std::fs::remove_file(&proof);
        assert!(
            landed,
            "the typed line never reached the window — XTEST input or the focus is wrong"
        );
    }

    #[test]
    fn a_third_level_symbol_is_not_offered() {
        // AltGr symbols sit at level 2+. Reporting one would type the wrong
        // character, because this viewer only ever holds Shift.
        let keymap = Keymap {
            min: 8,
            per_code: 4,
            syms: vec![0x61, 0x41, 0x00e6, 0x00c6],
        };
        assert_eq!(keymap.lookup(0x00e6), None);
    }
}
