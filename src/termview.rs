//! The pure half of the terminal viewer: pixels in, ANSI out, and bytes in,
//! intentions out.
//!
//! WHY IT EXISTS. Signing in to Entra ID needs a *visible* browser: the identity
//! broker knows no device-code flow on Linux (its own binary says
//! "AcquireTokenWithDeviceCodeFlow is not implemented on Linux platform") and
//! renders the sign-in page in an embedded WebKitGTK window on an X display. On a
//! headless server there is nothing to look at, so the choice used to be a VNC
//! viewer or nothing. This module draws that window in the terminal instead.
//!
//! It holds no X11 and no terminal I/O — [`crate::xscreen`] talks to the display
//! and [`crate::login`] owns the tty — so every rule below is unit-tested:
//!
//! * one cell is one `▀`, foreground = the top half, background = the bottom
//!   half, which is why a cell covers `scale` pixels across and `2 × scale` down
//!   (a terminal cell is about twice as tall as it is wide, so pixels stay
//!   square);
//! * a cell averages the pixels it covers rather than sampling one of them,
//!   because a thin glyph stroke disappears under nearest-neighbour and the whole
//!   point is to read a number-matching code;
//! * a repaint emits only the cells that changed, and a colour only when it
//!   differs from the last one written, because a full 200×50 frame is 400 kB of
//!   escape codes and this runs over SSH.

/// One captured X screen: 8-bit RGB, row-major, `width × height` pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// `3 × width × height` bytes, R, G, B.
    pub rgb: Vec<u8>,
}

impl Frame {
    /// A frame of one flat colour. Used by the tests, and by the viewer before
    /// the first capture arrives.
    pub fn flat(width: u32, height: u32, colour: [u8; 3]) -> Self {
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..width * height {
            rgb.extend_from_slice(&colour);
        }
        Self { width, height, rgb }
    }

    /// The part of the frame inside `box_`, clipped to what the frame holds.
    ///
    /// The browser viewer sends this rather than the whole display: the window is a
    /// third of the screen it sits on, and the rest is desktop nobody drew on. It
    /// costs a copy of the box, which is smaller than the frame it copies from.
    pub fn crop(&self, box_: Bounds) -> Frame {
        let x0 = box_.x.min(self.width);
        let y0 = box_.y.min(self.height);
        let width = box_.width.min(self.width - x0);
        let height = box_.height.min(self.height - y0);
        if width == 0 || height == 0 {
            return Frame {
                width: 0,
                height: 0,
                rgb: Vec::new(),
            };
        }
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for row in 0..height {
            let start = (((y0 + row) * self.width + x0) * 3) as usize;
            let len = (width * 3) as usize;
            match self.rgb.get(start..start + len) {
                Some(bytes) => rgb.extend_from_slice(bytes),
                None => rgb.extend(std::iter::repeat_n(0u8, len)),
            }
        }
        Frame { width, height, rgb }
    }

    fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        if x >= self.width || y >= self.height {
            return [0, 0, 0];
        }
        let i = ((y * self.width + x) * 3) as usize;
        match self.rgb.get(i..i + 3) {
            Some(p) => [p[0], p[1], p[2]],
            None => [0, 0, 0],
        }
    }

    /// The mean colour of the `w × h` source rectangle at (`x`, `y`), clipped to
    /// the frame. Returns black for a rectangle entirely outside it, which is what
    /// the letterboxing around a zoomed-out screen should look like.
    fn average(&self, x: f32, y: f32, w: f32, h: f32) -> [u8; 3] {
        let x0 = x.floor().max(0.0) as u32;
        let y0 = y.floor().max(0.0) as u32;
        let x1 = ((x + w).ceil() as i64).clamp(0, self.width as i64) as u32;
        let y1 = ((y + h).ceil() as i64).clamp(0, self.height as i64) as u32;
        if x1 <= x0 || y1 <= y0 {
            return [0, 0, 0];
        }
        // One pixel is the common case (scale 1, no zoom out): skip the sums.
        if x1 - x0 == 1 && y1 - y0 == 1 {
            return self.pixel(x0, y0);
        }
        let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
        for py in y0..y1 {
            for px in x0..x1 {
                let p = self.pixel(px, py);
                r += p[0] as u32;
                g += p[1] as u32;
                b += p[2] as u32;
                n += 1;
            }
        }
        [(r / n) as u8, (g / n) as u8, (b / n) as u8]
    }
}

/// A rectangle of the frame, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A pixel this dark in every channel is the empty desktop, not content. The
/// portal's own background is near-white and every dialog has a border, so the
/// threshold only has to clear black.
const DESKTOP: u8 = 24;

/// The smallest rectangle holding everything that is not empty desktop.
///
/// WHY IT MATTERS. The sign-in dialog is about 480×630 on a 1280×800 display, so
/// two thirds of the screen is black and fitting the whole screen spends most of
/// the terminal's cells on nothing. Cropping to the content is worth about a
/// quarter — measured, not assumed, and less than it sounds because the dialog is
/// TALL while a terminal has some hundred half-rows. That measurement is why
/// [`Viewport::at_actual_size`] and not a fit is what the viewer opens on.
///
/// Returns the whole frame when it is empty, so a caller never has to special-case
/// a blank display.
pub fn content_bounds(frame: &Frame) -> Bounds {
    let (mut x0, mut y0) = (frame.width, frame.height);
    let (mut x1, mut y1) = (0u32, 0u32);
    // Every other pixel: the dialog is hundreds of pixels wide, so a half-
    // resolution scan finds the same box for a quarter of the work.
    for y in (0..frame.height).step_by(2) {
        for x in (0..frame.width).step_by(2) {
            let p = frame.pixel(x, y);
            if p[0] > DESKTOP || p[1] > DESKTOP || p[2] > DESKTOP {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return Bounds {
            x: 0,
            y: 0,
            width: frame.width,
            height: frame.height,
        };
    }
    Bounds {
        x: x0,
        y: y0,
        // +2 for the step, clamped: the scan can miss the last odd row/column.
        width: (x1 - x0 + 2).min(frame.width - x0),
        height: (y1 - y0 + 2).min(frame.height - y0),
    }
}

/// How much two frames differ, as a fraction of the pixels compared.
///
/// This is how the automation knows a page changed without a DOM to ask: it types,
/// then waits for the picture to move and then to settle. Sampled every fourth
/// pixel in both directions, which is 16 times less work and still catches a
/// dialog appearing.
pub fn difference(a: &Frame, b: &Frame) -> f32 {
    if a.width != b.width || a.height != b.height {
        return 1.0;
    }
    let (mut differing, mut total) = (0u32, 0u32);
    for y in (0..a.height).step_by(4) {
        for x in (0..a.width).step_by(4) {
            let p = a.pixel(x, y);
            let q = b.pixel(x, y);
            let delta = (p[0] as i32 - q[0] as i32).abs()
                + (p[1] as i32 - q[1] as i32).abs()
                + (p[2] as i32 - q[2] as i32).abs();
            if delta > 24 {
                differing += 1;
            }
            total += 1;
        }
    }
    if total == 0 {
        return 0.0;
    }
    differing as f32 / total as f32
}

/// Which part of the frame the terminal shows, and how magnified.
///
/// `scale` is source pixels per cell across (and per half-cell down), so a
/// smaller number is a closer view: 1.0 shows every pixel and is what makes a
/// two-digit code legible; `fit` is whatever puts the whole screen on the tty.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub scale: f32,
    pub ox: f32,
    pub oy: f32,
}

/// The scale that fits `width × height` pixels into `cols × rows` cells.
pub fn fit_scale(width: u32, height: u32, cols: u16, rows: u16) -> f32 {
    if cols == 0 || rows == 0 {
        return 1.0;
    }
    let across = width as f32 / cols as f32;
    let down = height as f32 / (rows as f32 * 2.0);
    across.max(down).max(0.01)
}

impl Viewport {
    /// Fit `bounds` — the window rather than the desktop around it — and keep the
    /// rest of the frame reachable by panning.
    pub fn fitted_to(bounds: Bounds, cols: u16, rows: u16) -> Self {
        let scale = fit_scale(bounds.width, bounds.height, cols, rows);
        let (vw, vh) = (cols as f32 * scale, rows as f32 * 2.0 * scale);
        Self {
            scale,
            // Centre the content in the view, which letterboxes rather than
            // cropping when the shapes do not match.
            ox: bounds.x as f32 - (vw - bounds.width as f32) / 2.0,
            oy: bounds.y as f32 - (vh - bounds.height as f32) / 2.0,
        }
    }

    /// One pixel per half cell, anchored at the TOP of `bounds` and centred across
    /// it.
    ///
    /// This is what the viewer opens on, and the reason is arithmetic: a terminal
    /// has about 100 half-rows, the sign-in dialog is some 630 pixels tall, so
    /// fitting it costs a factor of six and 13-pixel text becomes two rows of
    /// colour. At actual size the same text is legible and the multi-factor number
    /// — which is what a reader is actually here to read, and which Entra puts near
    /// the top — is unmistakable. `F4` fits the whole window when a map is what is
    /// wanted.
    pub fn at_actual_size(bounds: Bounds, cols: u16, _rows: u16) -> Self {
        let vw = cols as f32;
        Self {
            scale: 1.0,
            ox: bounds.x as f32 + (bounds.width as f32 - vw) / 2.0,
            oy: bounds.y as f32,
        }
    }

    /// The whole screen, centred.
    pub fn fitted(frame_w: u32, frame_h: u32, cols: u16, rows: u16) -> Self {
        let scale = fit_scale(frame_w, frame_h, cols, rows);
        let mut vp = Self {
            scale,
            ox: 0.0,
            oy: 0.0,
        };
        vp.centre(frame_w, frame_h, cols, rows);
        vp
    }

    /// Centre the view on the frame, then clamp. Called when fitting and after a
    /// zoom, so zooming out never leaves the picture stuck in a corner.
    pub fn centre(&mut self, frame_w: u32, frame_h: u32, cols: u16, rows: u16) {
        let (vw, vh) = self.covered(cols, rows);
        self.ox = (frame_w as f32 - vw) / 2.0;
        self.oy = (frame_h as f32 - vh) / 2.0;
        self.clamp(frame_w, frame_h, cols, rows);
    }

    /// How many source pixels the terminal covers at this scale.
    pub fn covered(&self, cols: u16, rows: u16) -> (f32, f32) {
        (cols as f32 * self.scale, rows as f32 * 2.0 * self.scale)
    }

    /// Keep the view over the frame: no scrolling into the void, and a view
    /// larger than the frame is centred instead of pinned to the origin.
    pub fn clamp(&mut self, frame_w: u32, frame_h: u32, cols: u16, rows: u16) {
        let (vw, vh) = self.covered(cols, rows);
        self.ox = clamp_axis(self.ox, vw, frame_w as f32);
        self.oy = clamp_axis(self.oy, vh, frame_h as f32);
    }

    /// Multiply the scale, keeping the centre of the view where it was, so
    /// zooming in goes *towards* what the reader is looking at.
    pub fn zoom(&mut self, factor: f32, frame_w: u32, frame_h: u32, cols: u16, rows: u16) {
        let (vw, vh) = self.covered(cols, rows);
        let (cx, cy) = (self.ox + vw / 2.0, self.oy + vh / 2.0);
        self.scale = (self.scale * factor).clamp(0.25, 64.0);
        let (nw, nh) = self.covered(cols, rows);
        self.ox = cx - nw / 2.0;
        self.oy = cy - nh / 2.0;
        self.clamp(frame_w, frame_h, cols, rows);
    }

    /// Pan by a fraction of the visible area (a third of a screen per keypress).
    pub fn pan(&mut self, dx: f32, dy: f32, frame_w: u32, frame_h: u32, cols: u16, rows: u16) {
        let (vw, vh) = self.covered(cols, rows);
        self.ox += dx * vw / 3.0;
        self.oy += dy * vh / 3.0;
        self.clamp(frame_w, frame_h, cols, rows);
    }

    /// The frame pixel under a terminal cell, for turning a click into a pointer
    /// position. `row` is a whole cell; the pointer lands in the middle of it.
    pub fn pixel_at(&self, col: u16, row: u16) -> (i16, i16) {
        let x = self.ox + (col as f32 + 0.5) * self.scale;
        let y = self.oy + (row as f32 + 1.0) * self.scale;
        (x.round() as i16, y.round() as i16)
    }
}

fn clamp_axis(offset: f32, view: f32, frame: f32) -> f32 {
    if view >= frame {
        // The whole axis is visible: centre it (a negative offset letterboxes).
        (frame - view) / 2.0
    } else {
        offset.clamp(0.0, frame - view)
    }
}

/// One character cell: the colour of its top half and of its bottom half.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    pub top: [u8; 3],
    pub bottom: [u8; 3],
}

/// Sample the frame into a `cols × rows` grid of cells, row-major.
pub fn sample(frame: &Frame, vp: &Viewport, cols: u16, rows: u16) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(cols as usize * rows as usize);
    for row in 0..rows {
        for col in 0..cols {
            let x = vp.ox + col as f32 * vp.scale;
            let y = vp.oy + row as f32 * 2.0 * vp.scale;
            cells.push(Cell {
                top: frame.average(x, y, vp.scale, vp.scale),
                bottom: frame.average(x, y + vp.scale, vp.scale, vp.scale),
            });
        }
    }
    cells
}

/// The upper half block: its foreground paints the top pixel, its background the
/// bottom one.
const BLOCK: char = '\u{2580}';

/// Paint `next`, writing only what changed since `prev`.
///
/// `prev` of a different length (a resized terminal) is ignored, so the caller
/// can hand over whatever it has without checking.
pub fn paint(prev: Option<&[Cell]>, next: &[Cell], cols: u16, rows: u16) -> String {
    let prev = prev.filter(|p| p.len() == next.len());
    let mut out = String::new();
    // The last colours and position written, so an unchanged run costs one char.
    let mut fg: Option<[u8; 3]> = None;
    let mut bg: Option<[u8; 3]> = None;
    let mut at: Option<(u16, u16)> = None;

    for row in 0..rows {
        for col in 0..cols {
            let i = row as usize * cols as usize + col as usize;
            let Some(cell) = next.get(i) else { continue };
            if let Some(p) = prev {
                if p[i] == *cell {
                    continue;
                }
            }
            if at != Some((row, col)) {
                out.push_str(&format!("\x1b[{};{}H", row + 1, col + 1));
            }
            if fg != Some(cell.top) {
                let [r, g, b] = cell.top;
                out.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
                fg = Some(cell.top);
            }
            if bg != Some(cell.bottom) {
                let [r, g, b] = cell.bottom;
                out.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
                bg = Some(cell.bottom);
            }
            out.push(BLOCK);
            at = Some((row, col + 1));
        }
    }
    out
}

/// An X keysym the viewer can send. Only the keys a sign-in page needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Return,
    Space,
    Tab,
    BackTab,
    BackSpace,
    Delete,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

impl Key {
    /// The X11 keysym. `BackTab` is Tab plus Shift, which the sender applies.
    pub fn keysym(self) -> u32 {
        match self {
            Key::Return => 0xff0d,
            // Space activates a focused GTK button, which Enter does not.
            Key::Space => 0x0020,
            Key::Tab | Key::BackTab => 0xff09,
            Key::BackSpace => 0xff08,
            Key::Delete => 0xffff,
            Key::Escape => 0xff1b,
            Key::Left => 0xff51,
            Key::Up => 0xff52,
            Key::Right => 0xff53,
            Key::Down => 0xff54,
            Key::Home => 0xff50,
            Key::End => 0xff57,
            Key::PageUp => 0xff55,
            Key::PageDown => 0xff56,
        }
    }
}

/// What a keystroke means. Viewer commands are F-keys and control characters —
/// never a printable one, which belongs to the page being typed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Send a character to the X display.
    Type(char),
    /// Send a named key.
    Press(Key),
    /// Left button at a terminal cell (`down` false is the release).
    Click {
        col: u16,
        row: u16,
        down: bool,
    },
    ZoomIn,
    ZoomOut,
    Fit,
    Actual,
    Pan(i8, i8),
    Redraw,
    Help,
    Quit,
}

/// Decode as many actions as the buffer holds, returning them and how many bytes
/// were consumed. A trailing partial escape sequence is left for the next read,
/// so a split arrow key is never mistaken for an Escape.
pub fn decode(buf: &[u8]) -> (Vec<Action>, usize) {
    let mut actions = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        match b {
            0x1b => match decode_escape(&buf[i..]) {
                Escape::Incomplete => return (actions, i),
                Escape::Unknown(n) => i += n,
                Escape::Action(a, n) => {
                    actions.push(a);
                    i += n;
                }
            },
            0x0d | 0x0a => {
                actions.push(Action::Press(Key::Return));
                i += 1;
            }
            0x09 => {
                actions.push(Action::Press(Key::Tab));
                i += 1;
            }
            0x7f | 0x08 => {
                actions.push(Action::Press(Key::BackSpace));
                i += 1;
            }
            // Ctrl+Q quit, Ctrl+R redraw, Ctrl+U/Ctrl+D zoom, Ctrl+W fit,
            // Ctrl+G help. Chosen from characters a text field never needs.
            0x11 => {
                actions.push(Action::Quit);
                i += 1;
            }
            0x12 => {
                actions.push(Action::Redraw);
                i += 1;
            }
            0x15 => {
                actions.push(Action::ZoomIn);
                i += 1;
            }
            0x04 => {
                actions.push(Action::ZoomOut);
                i += 1;
            }
            0x17 => {
                actions.push(Action::Fit);
                i += 1;
            }
            0x07 => {
                actions.push(Action::Help);
                i += 1;
            }
            0x20..=0x7e => {
                actions.push(Action::Type(b as char));
                i += 1;
            }
            // Anything else (a stray control byte, a UTF-8 continuation) is
            // dropped rather than guessed at.
            _ => i += 1,
        }
    }
    (actions, i)
}

enum Escape {
    Action(Action, usize),
    Unknown(usize),
    Incomplete,
}

fn decode_escape(buf: &[u8]) -> Escape {
    // A lone ESC with nothing behind it is the Escape key. The caller polls with
    // a timeout, so a real arrow key arrives in one read with its bracket.
    if buf.len() == 1 {
        return Escape::Action(Action::Press(Key::Escape), 1);
    }
    match buf[1] {
        b'[' => decode_csi(buf),
        b'O' if buf.len() >= 3 => match buf[2] {
            // SS3: the F-keys and, on some terminals, the arrows.
            b'P' => Escape::Action(Action::Help, 3),
            b'Q' => Escape::Action(Action::ZoomIn, 3),
            b'R' => Escape::Action(Action::ZoomOut, 3),
            b'S' => Escape::Action(Action::Fit, 3),
            b'A' => Escape::Action(Action::Press(Key::Up), 3),
            b'B' => Escape::Action(Action::Press(Key::Down), 3),
            b'C' => Escape::Action(Action::Press(Key::Right), 3),
            b'D' => Escape::Action(Action::Press(Key::Left), 3),
            _ => Escape::Unknown(3),
        },
        b'O' => Escape::Incomplete,
        // ESC followed by a printable character is Alt+key on most terminals.
        // Nothing here wants it, and swallowing both bytes keeps a stray Alt
        // chord from typing its letter into the page.
        _ => Escape::Unknown(2),
    }
}

fn decode_csi(buf: &[u8]) -> Escape {
    // CSI mouse in SGR form: ESC [ < b ; x ; y (M|m).
    if buf.len() >= 3 && buf[2] == b'<' {
        return decode_mouse(buf);
    }
    let mut end = 2;
    while end < buf.len() && !(0x40..=0x7e).contains(&buf[end]) {
        end += 1;
    }
    if end >= buf.len() {
        return Escape::Incomplete;
    }
    let params = &buf[2..end];
    let final_byte = buf[end];
    let n = end + 1;
    // A modifier is the second parameter: ";5" is Ctrl, ";2" is Shift.
    let ctrl = params.ends_with(b";5");
    let shift = params.ends_with(b";2");
    let action = match final_byte {
        b'A' if ctrl => Action::Pan(0, -1),
        b'B' if ctrl => Action::Pan(0, 1),
        b'C' if ctrl => Action::Pan(1, 0),
        b'D' if ctrl => Action::Pan(-1, 0),
        b'A' => Action::Press(Key::Up),
        b'B' => Action::Press(Key::Down),
        b'C' => Action::Press(Key::Right),
        b'D' => Action::Press(Key::Left),
        b'H' => Action::Press(Key::Home),
        b'F' => Action::Press(Key::End),
        b'Z' => Action::Press(Key::BackTab),
        b'~' => match first_param(params) {
            Some(1) | Some(7) => Action::Press(Key::Home),
            Some(2) => Action::Press(Key::Escape),
            Some(3) => Action::Press(Key::Delete),
            Some(4) | Some(8) => Action::Press(Key::End),
            Some(5) => Action::Press(Key::PageUp),
            Some(6) => Action::Press(Key::PageDown),
            Some(11) | Some(23) => Action::Help,
            Some(12) | Some(24) => Action::ZoomIn,
            Some(13) => Action::ZoomOut,
            Some(14) => Action::Fit,
            Some(15) => Action::Redraw,
            Some(17) => Action::Actual,
            Some(_) | None => return Escape::Unknown(n),
        },
        b'M' | b'm' => return Escape::Unknown(n),
        _ => return Escape::Unknown(n),
    };
    // Shift+Tab arrives as CSI Z on most terminals and as CSI 1;2 I on some.
    let action = if shift && final_byte == b'I' {
        Action::Press(Key::BackTab)
    } else {
        action
    };
    Escape::Action(action, n)
}

fn first_param(params: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(params).ok()?;
    text.split(';').next()?.parse().ok()
}

fn decode_mouse(buf: &[u8]) -> Escape {
    let mut end = 3;
    while end < buf.len() && buf[end] != b'M' && buf[end] != b'm' {
        end += 1;
    }
    if end >= buf.len() {
        return Escape::Incomplete;
    }
    let n = end + 1;
    let Ok(text) = std::str::from_utf8(&buf[3..end]) else {
        return Escape::Unknown(n);
    };
    let mut parts = text.split(';');
    let button: u32 = match parts.next().and_then(|p| p.parse().ok()) {
        Some(b) => b,
        None => return Escape::Unknown(n),
    };
    let col: u16 = match parts.next().and_then(|p| p.parse().ok()) {
        Some(c) => c,
        None => return Escape::Unknown(n),
    };
    let row: u16 = match parts.next().and_then(|p| p.parse().ok()) {
        Some(r) => r,
        None => return Escape::Unknown(n),
    };
    // Left button only, press (M) and release (m). Wheel (64/65) and drag (32+)
    // are ignored: a page that needs scrolling has PageUp, and a drag has no use
    // on a sign-in form.
    if button != 0 {
        return Escape::Unknown(n);
    }
    Escape::Action(
        Action::Click {
            // The terminal counts from 1; the viewport counts cells from 0.
            col: col.saturating_sub(1),
            row: row.saturating_sub(1),
            down: buf[end] == b'M',
        },
        n,
    )
}

/// The key map, shown on F1 and once at startup. One line per binding, so the
/// caller can frame it however it likes.
pub const HELP: &[(&str, &str)] = &[
    ("type / Enter / Tab", "goes to the sign-in page"),
    ("click", "moves and clicks the pointer there"),
    (
        "F2 / Ctrl+U",
        "zoom in — use it to read the Authenticator number",
    ),
    ("F3 / Ctrl+D", "zoom out"),
    ("F4 / Ctrl+W", "fit the whole screen"),
    ("F6", "actual size (one pixel per half cell)"),
    ("Ctrl+arrows", "pan"),
    ("F5 / Ctrl+R", "repaint"),
    ("F1 / Ctrl+G", "this list"),
    ("Ctrl+Q", "quit (the sign-in keeps running)"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with a white rectangle on a black desktop, like the portal's dialog.
    fn frame_with_window(w: u32, h: u32, win: Bounds) -> Frame {
        let mut frame = Frame::flat(w, h, [0, 0, 0]);
        for y in win.y..win.y + win.height {
            for x in win.x..win.x + win.width {
                let i = ((y * w + x) * 3) as usize;
                frame.rgb[i] = 246;
                frame.rgb[i + 1] = 245;
                frame.rgb[i + 2] = 244;
            }
        }
        frame
    }

    #[test]
    fn the_content_box_is_the_window_not_the_desktop() {
        let win = Bounds {
            x: 100,
            y: 60,
            width: 200,
            height: 300,
        };
        let found = content_bounds(&frame_with_window(1280, 800, win));
        // Within the scan's two-pixel step.
        assert!(found.x <= win.x && win.x - found.x <= 2, "{found:?}");
        assert!(found.y <= win.y && win.y - found.y <= 2, "{found:?}");
        assert!(
            found.width >= win.width && found.width <= win.width + 4,
            "{found:?}"
        );
        assert!(
            found.height >= win.height && found.height <= win.height + 4,
            "{found:?}"
        );
    }

    #[test]
    fn a_crop_holds_the_pixels_of_its_box() {
        // A 4×4 frame with one white pixel at 2,1. Cropping to the 2×2 box at 2,1
        // has to put that pixel first and nothing else with it.
        let mut frame = Frame::flat(4, 4, [0, 0, 0]);
        // Row 1, column 2, three bytes per pixel.
        let at = (4 + 2) * 3;
        frame.rgb[at] = 255;
        frame.rgb[at + 1] = 255;
        frame.rgb[at + 2] = 255;
        let cut = frame.crop(Bounds {
            x: 2,
            y: 1,
            width: 2,
            height: 2,
        });
        assert_eq!((cut.width, cut.height), (2, 2));
        assert_eq!(cut.rgb.len(), 2 * 2 * 3);
        assert_eq!(&cut.rgb[0..3], &[255, 255, 255]);
        assert!(cut.rgb[3..].iter().all(|b| *b == 0));
    }

    #[test]
    fn a_crop_is_clipped_to_the_frame_it_cuts_from() {
        // A window may hang off the edge of the display. Reading past the last row
        // would take another window's pixels, or none at all.
        let frame = Frame::flat(8, 8, [7, 7, 7]);
        let cut = frame.crop(Bounds {
            x: 6,
            y: 6,
            width: 100,
            height: 100,
        });
        assert_eq!((cut.width, cut.height), (2, 2));
        assert_eq!(cut.rgb.len(), 2 * 2 * 3);
        // A box entirely outside is empty rather than a panic.
        let none = frame.crop(Bounds {
            x: 8,
            y: 8,
            width: 4,
            height: 4,
        });
        assert_eq!((none.width, none.height), (0, 0));
    }

    #[test]
    fn an_empty_display_reports_the_whole_frame() {
        // Otherwise the viewer would have to special-case a display with no window
        // yet — which is every display for the first half minute.
        let found = content_bounds(&Frame::flat(800, 600, [0, 0, 0]));
        assert_eq!(
            found,
            Bounds {
                x: 0,
                y: 0,
                width: 800,
                height: 600
            }
        );
    }

    /// The real dialog, measured on the tenant: 480×630 at 1280×800.
    const DIALOG: Bounds = Bounds {
        x: 400,
        y: 100,
        width: 480,
        height: 630,
    };

    #[test]
    fn fitting_the_window_beats_fitting_the_screen_but_not_by_much() {
        let whole = Viewport::fitted(1280, 800, 200, 50);
        let cropped = Viewport::fitted_to(DIALOG, 200, 50);
        assert!(
            cropped.scale < whole.scale,
            "cropping lost ground: {} vs {}",
            cropped.scale,
            whole.scale
        );
        // And it is only about a quarter better, because the dialog is TALL and a
        // terminal has ~100 half-rows: the height binds, not the black desktop. This
        // is why the viewer opens at actual size rather than fitted — a `fit` that
        // reads as unusable would look like a broken renderer.
        assert!(
            cropped.scale > whole.scale / 2.0,
            "if cropping ever gains this much, revisit the default view"
        );
        // The window is inside the view.
        let (vw, vh) = cropped.covered(200, 50);
        assert!(cropped.ox <= DIALOG.x as f32 && cropped.oy <= DIALOG.y as f32);
        assert!(cropped.ox + vw >= (DIALOG.x + DIALOG.width) as f32);
        assert!(cropped.oy + vh >= (DIALOG.y + DIALOG.height) as f32);
    }

    #[test]
    fn actual_size_shows_the_top_of_the_window_centred() {
        let vp = Viewport::at_actual_size(DIALOG, 200, 50);
        assert_eq!(vp.scale, 1.0);
        // The top edge, so the challenge text is on screen without panning.
        assert_eq!(vp.oy, DIALOG.y as f32);
        // Centred across the dialog: 480 wide, 200 columns → 140 px of margin each
        // side.
        assert_eq!(vp.ox, DIALOG.x as f32 + 140.0);
        // And a terminal WIDER than the dialog still centres it rather than pushing
        // it off to one side.
        let wide = Viewport::at_actual_size(DIALOG, 600, 50);
        assert_eq!(wide.ox, DIALOG.x as f32 - 60.0);
    }

    #[test]
    fn actual_size_is_legible_where_fitting_is_not() {
        // The measurement behind the default: 13-pixel body text and a ~40-pixel
        // challenge number, in half-rows of terminal.
        let fitted = Viewport::fitted_to(DIALOG, 200, 50);
        let body_rows_fitted = 13.0 / fitted.scale;
        let number_rows_fitted = 40.0 / fitted.scale;
        assert!(body_rows_fitted < 3.0, "body text was legible when fitted?");
        assert!(number_rows_fitted < 8.0);
        let actual = Viewport::at_actual_size(DIALOG, 200, 50);
        assert!(13.0 / actual.scale >= 13.0);
        assert!(40.0 / actual.scale >= 40.0);
    }

    #[test]
    fn difference_sees_a_dialog_appear_and_ignores_a_still_screen() {
        let blank = Frame::flat(400, 300, [0, 0, 0]);
        assert_eq!(difference(&blank, &blank), 0.0);
        let with_window = frame_with_window(
            400,
            300,
            Bounds {
                x: 0,
                y: 0,
                width: 200,
                height: 300,
            },
        );
        // Half the screen changed; anything above a few percent is a real change.
        assert!(difference(&blank, &with_window) > 0.4);
        // A different size is treated as a total change rather than compared.
        assert_eq!(difference(&blank, &Frame::flat(10, 10, [0, 0, 0])), 1.0);
    }

    #[test]
    fn a_cell_averages_the_pixels_it_covers() {
        // Two columns: black and white. One cell over both is mid grey — under
        // nearest-neighbour it would be one or the other, and a thin glyph
        // stroke would vanish.
        let mut frame = Frame::flat(2, 2, [0, 0, 0]);
        for y in 0..2 {
            let i = ((y * 2 + 1) * 3) as usize;
            frame.rgb[i] = 255;
            frame.rgb[i + 1] = 255;
            frame.rgb[i + 2] = 255;
        }
        let vp = Viewport {
            scale: 2.0,
            ox: 0.0,
            oy: 0.0,
        };
        let cells = sample(&frame, &vp, 1, 1);
        assert_eq!(cells[0].top, [127, 127, 127]);
    }

    #[test]
    fn a_cell_holds_two_rows_of_pixels() {
        // Top row red, bottom row blue: one cell at scale 1 must carry both.
        let mut frame = Frame::flat(1, 2, [255, 0, 0]);
        frame.rgb[3] = 0;
        frame.rgb[5] = 255;
        let vp = Viewport {
            scale: 1.0,
            ox: 0.0,
            oy: 0.0,
        };
        let cells = sample(&frame, &vp, 1, 1);
        assert_eq!(cells[0].top, [255, 0, 0]);
        assert_eq!(cells[0].bottom, [0, 0, 255]);
    }

    #[test]
    fn fit_shows_the_whole_screen() {
        // 1280x800 into 200x50 cells: 200 cells * scale >= 1280 and
        // 100 half-cells * scale >= 800.
        let scale = fit_scale(1280, 800, 200, 50);
        assert!(200.0 * scale >= 1280.0, "scale {scale} clips horizontally");
        assert!(100.0 * scale >= 800.0, "scale {scale} clips vertically");
        // And it is the *tightest* such scale, so nothing is wasted.
        assert!(scale <= 8.01, "scale {scale} is looser than it needs to be");
    }

    #[test]
    fn a_repaint_writes_only_what_changed() {
        let cols = 4;
        let rows = 2;
        let a = vec![Cell::default(); (cols * rows) as usize];
        let mut b = a.clone();
        b[5].top = [1, 2, 3];
        let out = paint(Some(&a), &b, cols, rows);
        // One cell means one block character, and no other.
        assert_eq!(out.matches(BLOCK).count(), 1);
        assert!(
            out.contains("\x1b[2;2H"),
            "did not address row 2 col 2: {out:?}"
        );
        assert!(out.contains("38;2;1;2;3"));
    }

    #[test]
    fn a_repaint_of_a_resized_terminal_redraws_everything() {
        let old = vec![Cell::default(); 4];
        let new = vec![Cell::default(); 6];
        // A grid of a different size cannot be diffed against, so every cell is
        // written rather than none — otherwise a resize would show a stale frame.
        let out = paint(Some(&old), &new, 3, 2);
        assert_eq!(out.matches(BLOCK).count(), 6);
    }

    #[test]
    fn a_colour_is_written_once_for_a_run() {
        let cells = vec![
            Cell {
                top: [9, 9, 9],
                bottom: [1, 1, 1],
            };
            3
        ];
        let out = paint(None, &cells, 3, 1);
        assert_eq!(out.matches("38;2;9;9;9").count(), 1);
        assert_eq!(out.matches("48;2;1;1;1").count(), 1);
        assert_eq!(out.matches(BLOCK).count(), 3);
    }

    #[test]
    fn printable_bytes_are_typed_and_control_bytes_are_commands() {
        let (actions, used) = decode(b"ab\x11");
        assert_eq!(used, 3);
        assert_eq!(
            actions,
            vec![Action::Type('a'), Action::Type('b'), Action::Quit]
        );
    }

    #[test]
    fn an_at_sign_and_a_dot_reach_the_page() {
        // The one thing this must never break: typing an email address.
        let (actions, _) = decode(b"a.b@c.d");
        let typed: String = actions
            .iter()
            .filter_map(|a| match a {
                Action::Type(c) => Some(*c),
                _ => None,
            })
            .collect();
        assert_eq!(typed, "a.b@c.d");
    }

    #[test]
    fn arrows_go_to_the_page_and_ctrl_arrows_pan() {
        let (actions, _) = decode(b"\x1b[A");
        assert_eq!(actions, vec![Action::Press(Key::Up)]);
        let (actions, _) = decode(b"\x1b[1;5A");
        assert_eq!(actions, vec![Action::Pan(0, -1)]);
    }

    #[test]
    fn a_split_escape_sequence_is_not_read_as_escape() {
        // The killer bug this guards: half an arrow key arriving in one read.
        // Escape must not be sent to the page, and the bytes must be kept.
        let (actions, used) = decode(b"\x1b[");
        assert!(actions.is_empty());
        assert_eq!(used, 0);
        let (actions, used) = decode(b"\x1b[1;5");
        assert!(actions.is_empty());
        assert_eq!(used, 0);
    }

    #[test]
    fn a_lone_escape_is_the_escape_key() {
        let (actions, used) = decode(b"\x1b");
        assert_eq!(actions, vec![Action::Press(Key::Escape)]);
        assert_eq!(used, 1);
    }

    #[test]
    fn a_click_becomes_a_cell_counted_from_zero() {
        let (actions, used) = decode(b"\x1b[<0;10;5M");
        assert_eq!(used, 10);
        assert_eq!(
            actions,
            vec![Action::Click {
                col: 9,
                row: 4,
                down: true
            }]
        );
        let (actions, _) = decode(b"\x1b[<0;10;5m");
        assert_eq!(
            actions,
            vec![Action::Click {
                col: 9,
                row: 4,
                down: false
            }]
        );
    }

    #[test]
    fn the_wheel_and_a_right_click_are_ignored() {
        let (actions, used) = decode(b"\x1b[<64;10;5M");
        assert!(actions.is_empty());
        assert_eq!(used, 11);
        let (actions, _) = decode(b"\x1b[<2;10;5M");
        assert!(actions.is_empty());
    }

    #[test]
    fn zooming_in_keeps_the_centre_and_clamps_to_the_frame() {
        let (w, h, cols, rows) = (1000, 1000, 100, 25);
        let mut vp = Viewport::fitted(w, h, cols, rows);
        let (vw, vh) = vp.covered(cols, rows);
        let centre = (vp.ox + vw / 2.0, vp.oy + vh / 2.0);
        vp.zoom(0.5, w, h, cols, rows);
        let (nw, nh) = vp.covered(cols, rows);
        assert!((vp.ox + nw / 2.0 - centre.0).abs() < 1.0);
        assert!((vp.oy + nh / 2.0 - centre.1).abs() < 1.0);
        // Zoomed in, the view must stay inside the frame.
        assert!(vp.ox >= 0.0 && vp.oy >= 0.0);
        assert!(vp.ox + nw <= w as f32 + 0.01);
    }

    #[test]
    fn panning_stops_at_the_edge() {
        let (w, h, cols, rows) = (1000, 1000, 100, 25);
        let mut vp = Viewport {
            scale: 1.0,
            ox: 0.0,
            oy: 0.0,
        };
        // Enough presses to cross the frame on BOTH axes: one press is a third of
        // the visible area, and the visible height here is half the width.
        for _ in 0..200 {
            vp.pan(-1.0, -1.0, w, h, cols, rows);
        }
        assert_eq!((vp.ox, vp.oy), (0.0, 0.0));
        for _ in 0..200 {
            vp.pan(1.0, 1.0, w, h, cols, rows);
        }
        let (vw, vh) = vp.covered(cols, rows);
        assert!((vp.ox + vw - w as f32).abs() < 0.01);
        assert!((vp.oy + vh - h as f32).abs() < 0.01);
    }

    #[test]
    fn a_view_wider_than_the_screen_is_centred() {
        // Zoomed out past the frame: the picture is letterboxed, not pinned.
        let mut vp = Viewport {
            scale: 10.0,
            ox: 0.0,
            oy: 0.0,
        };
        vp.clamp(100, 100, 100, 25);
        assert!(vp.ox < 0.0 && vp.oy < 0.0);
        let (vw, _) = vp.covered(100, 25);
        assert!((vp.ox + vw / 2.0 - 50.0).abs() < 0.01);
    }

    #[test]
    fn a_click_maps_back_to_the_pixel_it_covers() {
        let vp = Viewport {
            scale: 4.0,
            ox: 100.0,
            oy: 200.0,
        };
        // Cell (0,0) is the 4x4 block at (100,200) — the pointer goes to its middle.
        assert_eq!(vp.pixel_at(0, 0), (102, 204));
        assert_eq!(vp.pixel_at(10, 5), (142, 224));
    }
}
