//! `login --web` — the same sign-in window, drawn in a browser over the tailnet.
//!
//! WHY, GIVEN THERE IS ALREADY A VIEWER. The terminal viewer draws the display
//! with half-block cells, which costs half the vertical resolution and all of the
//! sub-pixel detail: 13-pixel text survives it only when the reader zooms in, and
//! the two-digit Authenticator number is the one thing they came for. A browser
//! draws the same pixels at their own size, and a mouse is a real pointer rather
//! than a cell coordinate.
//!
//! WHAT IT DOES NOT CHANGE. The broker still renders the sign-in in a WebKitGTK
//! view on an X display, so the private [`Xvfb`](crate::xvfb) stays and so does
//! [`XScreen`](crate::xscreen): this module replaces [`crate::termview`], nothing
//! else. Tailscale is the transport and not the display — the window has to exist
//! either way.
//!
//! THE SERVER IS DELIBERATELY SMALL. One thread, no async runtime, no WebSocket:
//!
//! * the browser polls `GET /delta`, which answers with the tiles that changed
//!   since the sequence number it holds (gzip, which every browser inflates on its
//!   own — so the client does no decompression work);
//! * input goes back as `POST /input`, one request per keystroke, so a key never
//!   waits for a frame; and
//! * `poll(2)` over the listener and the open sockets keeps those two independent
//!   without a thread each, and without a partial-write queue.
//!
//! WHAT PROTECTS IT. The listener binds to the tailnet address (or to loopback
//! when there is none), so the socket is reachable only inside WireGuard, and
//! every request must carry a 128-bit token that is minted per session and printed
//! once. The token is what stops another device on the tailnet — or another user on
//! this host, for the loopback case — from watching a password being typed.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::debug;

use crate::login::Session;
use crate::termview::{Frame, Key};
use crate::xscreen::XScreen;
use crate::{login, ops};

/// The port the viewer listens on when none is given.
pub const DEFAULT_PORT: u16 = 6080;

/// Where to serve the sign-in window.
pub struct Options {
    pub bind: IpAddr,
    pub port: u16,
}

impl Options {
    /// The address to listen on, preferring the tailnet: a viewer bound there is
    /// reachable from every device in it and from nothing else, which is the whole
    /// point of serving it rather than drawing it in the terminal.
    pub fn resolved(bind: Option<IpAddr>, port: Option<u16>) -> Self {
        let bind = bind
            .or_else(tailnet_address)
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        Self {
            bind,
            port: port.unwrap_or(DEFAULT_PORT),
        }
    }

    fn socket(&self) -> SocketAddr {
        SocketAddr::new(self.bind, self.port)
    }
}

/// The edge of one update tile, in pixels.
///
/// A whole 1280×800 frame is 3 MB and a caret blinking is a few hundred bytes of
/// it, so the picture is sent in tiles and only the ones that changed go out. 32 is
/// small enough that typing a character sends a strip rather than a screen, and
/// large enough that a full repaint is a thousand tiles rather than a hundred
/// thousand headers.
const TILE: u32 = 32;

/// How stale a capture may be before `/delta` takes a new one. `GetImage` plus the
/// decode is the expensive part of a frame, so two requests arriving together share
/// one capture.
const MIN_CAPTURE: Duration = Duration::from_millis(70);

/// How long `poll` waits when nothing is happening. The loop's whole idle cost.
const POLL_WAIT: Duration = Duration::from_millis(20);

/// How long one request may take to arrive once its socket says it is readable.
/// A browser sends a request in one segment; this bounds what a stalled one can
/// cost the single-threaded loop.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// How long a frame may take to go out before the client is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the container is asked whether the portal is still up. A subprocess
/// probe, so deliberately much rarer than a frame.
const PORTAL_POLL: Duration = Duration::from_secs(3);

/// A browser opens a handful of connections and keeps them alive. The cap is what
/// stops an abandoned session from holding file descriptors for ever.
const MAX_CLIENTS: usize = 8;

/// Serve the display until the reader finishes, the portal closes, or the session
/// is cut from outside.
pub fn serve(screen: &XScreen, options: &Options) -> Result<Session> {
    let token = new_token()?;
    let listener = TcpListener::bind(options.socket())
        .with_context(|| format!("cannot listen on {}", options.socket()))?;
    print_url(options, &token);
    serve_on(screen, listener, token)
}

/// The session loop, on a listener that is already bound.
///
/// Split from [`serve`] so a test can drive the whole server — page, deltas, input
/// and the end of the session — on a port of its own and with a token it knows.
fn serve_on(screen: &XScreen, listener: TcpListener, token: String) -> Result<Session> {
    listener
        .set_nonblocking(true)
        .context("cannot make the listener non-blocking")?;

    let mut server = Server {
        screen,
        token,
        sent: None,
        seq: 0,
        current: None,
        captured: Instant::now() - MIN_CAPTURE,
        finished: false,
        portal_gone: false,
    };
    let mut clients: Vec<TcpStream> = Vec::new();
    let mut last_probe = Instant::now();
    let mut portal_seen = false;

    loop {
        if login::interrupted() {
            return Ok(Session { portal_gone: false });
        }
        if server.finished {
            return Ok(Session { portal_gone: false });
        }

        // The portal going is the end of the session: the reader closed the window,
        // or it closed itself once the sign-in was complete.
        if last_probe.elapsed() >= PORTAL_POLL {
            last_probe = Instant::now();
            if ops::portal_is_running() {
                portal_seen = true;
            } else if portal_seen {
                server.portal_gone = true;
                // Let the page hear about it before the socket closes under it.
                serve_once(&mut server, &mut clients, Duration::from_millis(300));
                return Ok(Session { portal_gone: true });
            }
        }

        accept(&listener, &mut clients);
        serve_once(&mut server, &mut clients, POLL_WAIT);
    }
}

/// Take whatever connections are waiting, dropping the oldest when the cap is
/// reached — an abandoned tab must not lock out the reader's new one.
fn accept(listener: &TcpListener, clients: &mut Vec<TcpStream>) {
    while let Ok((stream, from)) = listener.accept() {
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let _ = stream.set_nodelay(true);
        if clients.len() >= MAX_CLIENTS {
            clients.remove(0);
        }
        debug!(%from, "a viewer connected");
        clients.push(stream);
    }
}

/// Wait up to `wait` for a request on any open connection, and answer the ones
/// that arrived.
fn serve_once(server: &mut Server, clients: &mut Vec<TcpStream>, wait: Duration) {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    if clients.is_empty() {
        std::thread::sleep(wait);
        return;
    }

    let ready: Vec<usize> = {
        let mut fds: Vec<PollFd> = clients
            .iter()
            .map(|c| PollFd::new(c.as_fd(), PollFlags::POLLIN))
            .collect();
        let millis = wait.as_millis().min(u16::MAX as u128) as u16;
        match poll(&mut fds, PollTimeout::from(millis)) {
            Ok(0) | Err(_) => return,
            Ok(_) => fds
                .iter()
                .enumerate()
                .filter(|(_, fd)| fd.revents().is_some_and(|r| !r.is_empty()))
                .map(|(i, _)| i)
                .collect(),
        }
    };

    let mut done = Vec::new();
    for index in ready {
        let keep = match read_request(&mut clients[index]) {
            Ok(Some(request)) => server.answer(&mut clients[index], &request).is_ok(),
            // A closed or unreadable connection: the browser reopens one.
            _ => false,
        };
        if !keep {
            done.push(index);
        }
    }
    for index in done.into_iter().rev() {
        clients.remove(index);
    }
}

/// The viewer's state between requests.
struct Server<'a> {
    screen: &'a XScreen,
    token: String,
    /// The frame the client holds, which is what a delta is computed against.
    ///
    /// Shared rather than copied: a frame is three megabytes, and the same one is
    /// both the last capture and the last thing sent.
    sent: Option<Arc<Frame>>,
    /// Bumped for every delta sent. A client whose sequence does not match gets a
    /// whole frame, which is the only answer that is right when an update was
    /// missed.
    seq: u32,
    current: Option<Arc<Frame>>,
    captured: Instant,
    /// The reader pressed Finish.
    finished: bool,
    /// The portal has gone; the page is told so it can stop polling.
    portal_gone: bool,
}

/// The magic word at the head of a delta: `ICV1`, so a stale client or a stray
/// proxy response is rejected rather than drawn as noise.
const MAGIC: u32 = 0x4943_5631;

/// A window is on the display.
const FLAG_WINDOW: u16 = 1;
/// The session is over.
const FLAG_GONE: u16 = 2;

impl Server<'_> {
    fn answer(&mut self, stream: &mut TcpStream, request: &Request) -> std::io::Result<()> {
        // The one request a browser makes on its own, and the only one that cannot
        // carry the token. Answered with nothing, ahead of the check, so the page's
        // console holds no error to read past. It says no more than the open port
        // already does.
        if request.path == "/favicon.ico" {
            return respond(stream, "204 No Content", "text/plain", &[], b"");
        }
        if !constant_time_eq(request.token.as_bytes(), self.token.as_bytes()) {
            return respond(stream, "403 Forbidden", "text/plain", &[], b"wrong token\n");
        }
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") => {
                let page = page(self.screen.width, self.screen.height);
                respond(
                    stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    &[],
                    page.as_bytes(),
                )
            }
            ("GET", "/delta") => self.delta(stream, request),
            ("POST", "/input") => {
                let applied = self.apply(&request.body);
                match applied {
                    Ok(()) => respond(stream, "200 OK", "text/plain", &[], b"ok\n"),
                    Err(e) => {
                        debug!("input refused: {e:#}");
                        respond(stream, "400 Bad Request", "text/plain", &[], b"bad input\n")
                    }
                }
            }
            ("POST", "/quit") => {
                self.finished = true;
                respond(stream, "200 OK", "text/plain", &[], b"closing\n")
            }
            _ => respond(
                stream,
                "404 Not Found",
                "text/plain",
                &[],
                b"no such path\n",
            ),
        }
    }

    /// Answer with the tiles that changed since the client's sequence number.
    fn delta(&mut self, stream: &mut TcpStream, request: &Request) -> std::io::Result<()> {
        let client_seq: u32 = request
            .query("seq")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let frame = match self.capture() {
            Ok(frame) => frame,
            Err(e) => {
                debug!("cannot read the display: {e:#}");
                return respond(
                    stream,
                    "503 Service Unavailable",
                    "text/plain",
                    &[],
                    b"the display did not answer\n",
                );
            }
        };

        // A stale sequence number means an update was missed, and a diff against a
        // frame the client never drew would leave the picture wrong for ever.
        let against = if client_seq == self.seq {
            self.sent.clone()
        } else {
            None
        };
        let tiles = changed_tiles(against.as_deref(), &frame);

        let mut flags = 0;
        if self.screen.has_window() {
            flags |= FLAG_WINDOW;
        }
        if self.portal_gone {
            flags |= FLAG_GONE;
        }
        if !tiles.is_empty() {
            self.seq = self.seq.wrapping_add(1);
            self.sent = Some(frame.clone());
        }
        let body = encode(self.seq, &frame, flags, &tiles);

        let body = gzip(&body)?;
        respond(
            stream,
            "200 OK",
            "application/octet-stream",
            &["Content-Encoding: gzip"],
            &body,
        )
    }

    /// The display as it is now, re-reading it only when the last read is stale.
    fn capture(&mut self) -> Result<Arc<Frame>> {
        if self.current.is_none() || self.captured.elapsed() >= MIN_CAPTURE {
            // Before the picture, as in the terminal viewer: with no window manager
            // on this display, nothing else moves a new window into view or gives it
            // the keyboard.
            self.screen.place_and_focus()?;
            self.current = Some(Arc::new(self.screen.capture()?));
            self.captured = Instant::now();
        }
        Ok(self.current.clone().expect("just captured"))
    }

    /// Apply one batch of input events.
    fn apply(&mut self, body: &[u8]) -> Result<()> {
        let events: Vec<Event> =
            serde_json::from_slice(body).context("the input was not a list of events")?;
        for event in events {
            match event {
                Event::Text { v } => {
                    self.screen.place_and_focus()?;
                    let refused = self.screen.type_text(&v)?;
                    if !refused.is_empty() {
                        debug!("this display's keyboard cannot type {refused:?}");
                    }
                }
                Event::Key { v } => {
                    let key = named_key(&v)
                        .with_context(|| format!("{v:?} is not a key this viewer sends"))?;
                    self.screen.place_and_focus()?;
                    self.screen.press(key)?;
                }
                Event::Mouse { x, y, down } => {
                    self.screen.click(x, y, down)?;
                    // A click is how a window gets the focus with no window manager
                    // around, so let it, then take the keyboard back.
                    if !down {
                        self.screen.place_and_focus()?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// What the page sends back. Text and keys are separate because a printable
/// character is a keysym the display's map already holds, while a named key is one
/// of the few the viewer knows how to send.
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Event {
    Text { v: String },
    Key { v: String },
    Mouse { x: i16, y: i16, down: bool },
}

/// The named keys the page may ask for, spelled as [`Key`] spells them.
fn named_key(name: &str) -> Option<Key> {
    Some(match name {
        "Return" => Key::Return,
        "Space" => Key::Space,
        "Tab" => Key::Tab,
        "BackTab" => Key::BackTab,
        "BackSpace" => Key::BackSpace,
        "Delete" => Key::Delete,
        "Escape" => Key::Escape,
        "Left" => Key::Left,
        "Right" => Key::Right,
        "Up" => Key::Up,
        "Down" => Key::Down,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        _ => return None,
    })
}

/// One rectangle of the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tile {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// The tiles of `current` that differ from `previous`. All of them when there is no
/// previous frame, or when the display was resized under us.
fn changed_tiles(previous: Option<&Frame>, current: &Frame) -> Vec<Tile> {
    let previous = previous.filter(|p| p.width == current.width && p.height == current.height);
    let mut tiles = Vec::new();
    let mut y = 0;
    while y < current.height {
        let height = TILE.min(current.height - y);
        let mut x = 0;
        while x < current.width {
            let width = TILE.min(current.width - x);
            let tile = Tile {
                x,
                y,
                width,
                height,
            };
            if previous.is_none_or(|p| differs(p, current, &tile)) {
                tiles.push(tile);
            }
            x += TILE;
        }
        y += TILE;
    }
    tiles
}

/// Does one tile differ between two frames of the same size?
fn differs(a: &Frame, b: &Frame, tile: &Tile) -> bool {
    for row in 0..tile.height {
        let start = ((tile.y + row) * a.width + tile.x) as usize * 3;
        let len = tile.width as usize * 3;
        match (a.rgb.get(start..start + len), b.rgb.get(start..start + len)) {
            (Some(left), Some(right)) if left == right => {}
            // A row that is not there in one of them counts as a difference: a short
            // buffer is better redrawn than trusted.
            _ => return true,
        }
    }
    false
}

/// Pack a delta: a fixed header, then every tile with its own rectangle.
///
/// Raw RGB rather than PNG or JPEG, because the response is gzipped anyway and the
/// picture is a form on a flat background — the kind of content deflate is good at.
/// It also keeps the client free of a decoder.
fn encode(seq: u32, frame: &Frame, flags: u16, tiles: &[Tile]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + tiles.len() * (8 + (TILE * TILE * 3) as usize));
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&(frame.width as u16).to_be_bytes());
    out.extend_from_slice(&(frame.height as u16).to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&(tiles.len() as u16).to_be_bytes());
    for tile in tiles {
        out.extend_from_slice(&(tile.x as u16).to_be_bytes());
        out.extend_from_slice(&(tile.y as u16).to_be_bytes());
        out.extend_from_slice(&(tile.width as u16).to_be_bytes());
        out.extend_from_slice(&(tile.height as u16).to_be_bytes());
        for row in 0..tile.height {
            let start = ((tile.y + row) * frame.width + tile.x) as usize * 3;
            let len = tile.width as usize * 3;
            match frame.rgb.get(start..start + len) {
                Some(bytes) => out.extend_from_slice(bytes),
                None => out.extend(std::iter::repeat_n(0u8, len)),
            }
        }
    }
    out
}

/// Compress a body the way every browser can already inflate one, so the client
/// needs no decompression code of its own.
fn gzip(body: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(body)?;
    encoder.finish()
}

/// One HTTP request, reduced to the four things this server reads.
#[derive(Debug, Default)]
struct Request {
    method: String,
    path: String,
    query: String,
    token: String,
    body: Vec<u8>,
}

impl Request {
    fn query(&self, name: &str) -> Option<&str> {
        query_value(&self.query, name)
    }
}

/// One value from a query string. No percent-decoding: every value this server
/// reads is a number or a hex token.
fn query_value<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

/// Headers are bounded so a client cannot make the server hold a large buffer, and
/// the body is bounded well above the largest thing the page sends (a pasted
/// password).
const MAX_HEADERS: usize = 8 * 1024;
const MAX_BODY: usize = 64 * 1024;

/// Read one request from a socket that has said it is readable.
fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    let head = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            break at;
        }
        if buffer.len() > MAX_HEADERS {
            anyhow::bail!("the request headers are too large");
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(e).context("cannot read the request"),
        }
    };

    let parsed = parse_head(&String::from_utf8_lossy(&buffer[..head]));
    if parsed.length > MAX_BODY {
        anyhow::bail!("the request body is too large");
    }

    let mut body = buffer[head + 4..].to_vec();
    while body.len() < parsed.length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(e).context("cannot read the request body"),
        }
    }
    body.truncate(parsed.length);

    Ok(Some(Request {
        method: parsed.method,
        path: parsed.path,
        query: parsed.query,
        token: parsed.token,
        body,
    }))
}

/// What the request line and the headers said.
#[derive(Debug, Default, PartialEq, Eq)]
struct Head {
    method: String,
    path: String,
    query: String,
    token: String,
    length: usize,
}

/// Read the request line and the two headers this server acts on.
///
/// Anything else is ignored on purpose: the only client is the page below, and a
/// server that parsed more of HTTP than it needs would be more to get wrong.
fn parse_head(text: &str) -> Head {
    let mut lines = text.lines();
    let start = lines.next().unwrap_or_default();
    let mut words = start.split_whitespace();
    let method = words.next().unwrap_or_default().to_string();
    let target = words.next().unwrap_or_default();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut head = Head {
        method,
        path,
        query,
        ..Head::default()
    };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            head.length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("x-token") {
            head.token = value.to_string();
        }
    }
    // The page is opened from a link, so the first token can only come in the URL;
    // every request the page makes afterwards sends it as a header.
    if head.token.is_empty() {
        head.token = query_value(&head.query, "k")
            .unwrap_or_default()
            .to_string();
    }
    head
}

/// The first index of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Write one response and keep the connection open for the next request.
fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra: &[&str],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: keep-alive\r\n",
        body.len()
    );
    for line in extra {
        head.push_str(line);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Compare two secrets without leaking where they first differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A fresh 128-bit session token, from the kernel rather than from a clock: it is
/// the only thing between the tailnet and a password being typed.
fn new_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("cannot read /dev/urandom for a session token")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// This host's tailnet address, if Tailscale is up.
///
/// Read from the interfaces rather than from the `tailscale` command, because the
/// CLI is not always installed next to the daemon and this needs no subprocess.
pub fn tailnet_address() -> Option<IpAddr> {
    let addresses = nix::ifaddrs::getifaddrs().ok()?;
    for interface in addresses {
        if !interface.interface_name.starts_with("tailscale") {
            continue;
        }
        let Some(address) = interface.address.and_then(|a| a.as_sockaddr_in().copied()) else {
            continue;
        };
        let ip = address.ip();
        if is_tailnet(ip) {
            return Some(IpAddr::V4(ip));
        }
    }
    None
}

/// Is this one of the addresses Tailscale hands out? `100.64.0.0/10`, the shared
/// address space it uses for a tailnet.
fn is_tailnet(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

/// Print the one link that opens the session, and say what it is reachable from.
fn print_url(options: &Options, token: &str) {
    let url = format!("http://{}:{}/?k={token}", options.bind, options.port);
    eprintln!();
    eprintln!("Open the sign-in window in a browser:");
    eprintln!();
    eprintln!("  {url}");
    eprintln!();
    if options.bind.is_loopback() {
        eprintln!("It listens on loopback only. Reach it from your own machine with:");
        eprintln!("  ssh -N -L {0}:127.0.0.1:{0} <this-host>", options.port);
    } else {
        eprintln!("It listens on the tailnet, so every device in yours can open it — a phone");
        eprintln!("included, which is where the Authenticator prompt arrives.");
    }
    eprintln!();
    eprintln!("The link holds a token that is new for this session. Anything without it is");
    eprintln!("refused. Press Ctrl+C here, or Finish in the page, to close the window.");
    eprintln!();
}

/// The page: a canvas, a poll loop, and the keyboard and mouse sent back.
///
/// It is one file with no build step and no dependency on purpose — the CLI is a
/// single static binary, and a viewer that needed a bundler would not survive the
/// way this crate is published.
fn page(width: u32, height: u32) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Intune sign-in</title>
<style>
  html, body {{ margin: 0; height: 100%; background: #12151a; color: #d7dae0;
    font: 13px/1.5 ui-sans-serif, system-ui, sans-serif; }}
  body {{ display: flex; flex-direction: column; }}
  #frame {{ flex: 1; display: grid; place-items: center; overflow: auto; outline: none; }}
  canvas {{ max-width: 100%; max-height: 100%; background: #000;
    box-shadow: 0 0 0 1px #2a2f37; }}
  #bar {{ display: flex; gap: 16px; align-items: center; padding: 8px 12px;
    background: #1b1f26; border-top: 1px solid #2a2f37; }}
  #state {{ flex: 1; color: #9aa1ad; }}
  button {{ font: inherit; color: #d7dae0; background: #2a2f37; border: 0;
    border-radius: 5px; padding: 5px 12px; cursor: pointer; }}
  button:hover {{ background: #353b45; }}
  kbd {{ background: #2a2f37; border-radius: 3px; padding: 1px 5px; }}
</style>
</head>
<body>
<div id="frame" tabindex="0"><canvas id="screen" width="{width}" height="{height}"></canvas></div>
<div id="bar">
  <span id="state">connecting…</span>
  <span>click the page, then type — <kbd>Ctrl</kbd>+<kbd>V</kbd> pastes</span>
  <button id="finish">Finish</button>
</div>
<script>
const TOKEN = new URLSearchParams(location.search).get('k') || '';
const canvas = document.getElementById('screen');
const ctx = canvas.getContext('2d', {{ alpha: false }});
const state = document.getElementById('state');
const frame = document.getElementById('frame');
const KEYS = {{
  Enter: 'Return', Tab: 'Tab', Backspace: 'BackSpace', Delete: 'Delete',
  Escape: 'Escape', ArrowLeft: 'Left', ArrowRight: 'Right', ArrowUp: 'Up',
  ArrowDown: 'Down', Home: 'Home', End: 'End', PageUp: 'PageUp', PageDown: 'PageDown',
}};
let seq = 0, alive = true, idle = 60;

function say(text) {{ state.textContent = text; }}

async function send(events) {{
  if (!alive) return;
  try {{
    await fetch('/input', {{
      method: 'POST', cache: 'no-store',
      headers: {{ 'X-Token': TOKEN, 'Content-Type': 'application/json' }},
      body: JSON.stringify(events),
    }});
  }} catch (e) {{ say('the viewer stopped answering'); }}
}}

// One tile at a time: an ImageData per rectangle, so a keystroke repaints a strip
// rather than the screen.
function draw(view) {{
  if (view.getUint32(0) !== 0x49435631) return false;
  // magic, seq, width, height, flags, count — then one rectangle per tile.
  seq = view.getUint32(4);
  const flags = view.getUint16(12);
  const count = view.getUint16(14);
  let at = 16;
  for (let n = 0; n < count; n++) {{
    const x = view.getUint16(at), y = view.getUint16(at + 2);
    const w = view.getUint16(at + 4), h = view.getUint16(at + 6);
    at += 8;
    const image = ctx.createImageData(w, h);
    const out = image.data;
    for (let p = 0, q = 0; p < w * h; p++, at += 3, q += 4) {{
      out[q] = view.getUint8(at);
      out[q + 1] = view.getUint8(at + 1);
      out[q + 2] = view.getUint8(at + 2);
      out[q + 3] = 255;
    }}
    ctx.putImageData(image, x, y);
  }}
  if (flags & 2) {{
    alive = false;
    say('the sign-in window closed — you can close this tab');
  }} else if (!(flags & 1)) {{
    say('waiting for the sign-in window…');
  }} else {{
    say('sign-in window · ' + canvas.width + '×' + canvas.height);
  }}
  return count > 0;
}}

async function pump() {{
  while (alive) {{
    try {{
      const answer = await fetch('/delta?seq=' + seq, {{
        cache: 'no-store', headers: {{ 'X-Token': TOKEN }},
      }});
      if (!answer.ok) {{ say('the viewer answered ' + answer.status); await sleep(1000); continue; }}
      const moved = draw(new DataView(await answer.arrayBuffer()));
      idle = moved ? 40 : Math.min(idle * 2, 300);
    }} catch (e) {{
      say('the viewer stopped answering');
      await sleep(1000);
    }}
    await sleep(idle);
  }}
}}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// A pixel of the display, from a click on a canvas the browser may have scaled.
function pixel(event) {{
  const box = canvas.getBoundingClientRect();
  const x = (event.clientX - box.left) * (canvas.width / box.width);
  const y = (event.clientY - box.top) * (canvas.height / box.height);
  return {{ x: Math.max(0, Math.round(x)), y: Math.max(0, Math.round(y)) }};
}}

canvas.addEventListener('mousedown', (e) => {{
  e.preventDefault();
  frame.focus();
  const at = pixel(e);
  send([{{ t: 'mouse', x: at.x, y: at.y, down: true }}]);
}});
canvas.addEventListener('mouseup', (e) => {{
  e.preventDefault();
  const at = pixel(e);
  send([{{ t: 'mouse', x: at.x, y: at.y, down: false }}]);
}});
canvas.addEventListener('contextmenu', (e) => e.preventDefault());

frame.addEventListener('keydown', (e) => {{
  if (e.ctrlKey || e.altKey || e.metaKey) return;   // paste and browser keys stay the browser's
  if (e.key === 'Tab') {{
    e.preventDefault();
    send([{{ t: 'key', v: e.shiftKey ? 'BackTab' : 'Tab' }}]);
    return;
  }}
  const named = KEYS[e.key];
  if (named) {{ e.preventDefault(); send([{{ t: 'key', v: named }}]); return; }}
  if (e.key.length === 1) {{ e.preventDefault(); send([{{ t: 'text', v: e.key }}]); }}
}});

// Pasting is the reason this page beats the terminal viewer for a password: the
// characters are typed into the display one key at a time, from the clipboard.
frame.addEventListener('paste', (e) => {{
  e.preventDefault();
  const text = (e.clipboardData || window.clipboardData).getData('text');
  if (text) send([{{ t: 'text', v: text }}]);
}});

document.getElementById('finish').addEventListener('click', async () => {{
  alive = false;
  say('closing the sign-in window…');
  await fetch('/quit', {{ method: 'POST', headers: {{ 'X-Token': TOKEN }} }});
}});

frame.focus();
pump();
</script>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, colour: [u8; 3]) -> Frame {
        Frame::flat(width, height, colour)
    }

    #[test]
    fn a_first_frame_sends_every_tile() {
        // 64×64 with a 32-pixel tile is exactly four tiles, and a client that holds
        // nothing has to be sent all of them.
        let current = frame(64, 64, [0, 0, 0]);
        assert_eq!(changed_tiles(None, &current).len(), 4);
    }

    #[test]
    fn an_unchanged_frame_sends_nothing() {
        let a = frame(64, 64, [10, 20, 30]);
        let b = frame(64, 64, [10, 20, 30]);
        assert!(changed_tiles(Some(&a), &b).is_empty());
    }

    #[test]
    fn one_changed_pixel_sends_only_its_tile() {
        let previous = frame(64, 64, [0, 0, 0]);
        let mut current = previous.clone();
        // A pixel at 40,40 sits in the tile at 32,32.
        let at = ((40 * 64) + 40) * 3;
        current.rgb[at] = 255;
        let tiles = changed_tiles(Some(&previous), &current);
        assert_eq!(
            tiles,
            vec![Tile {
                x: 32,
                y: 32,
                width: 32,
                height: 32
            }]
        );
    }

    #[test]
    fn a_resized_display_is_redrawn_whole() {
        // Diffing against a frame of another size would read the wrong rows.
        let previous = frame(64, 64, [0, 0, 0]);
        let current = frame(96, 64, [0, 0, 0]);
        assert_eq!(changed_tiles(Some(&previous), &current).len(), 6);
    }

    #[test]
    fn an_edge_tile_is_clipped_to_the_display() {
        // 40 pixels is one whole tile and one 8-pixel remainder; a tile that ran
        // past the edge would read another row's pixels.
        let current = frame(40, 32, [0, 0, 0]);
        let tiles = changed_tiles(None, &current);
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[1].width, 8);
        assert_eq!(tiles[0].height, 32);
    }

    #[test]
    fn a_delta_says_what_it_holds() {
        let current = frame(64, 32, [1, 2, 3]);
        let tiles = changed_tiles(None, &current);
        let body = encode(7, &current, FLAG_WINDOW, &tiles);
        assert_eq!(u32::from_be_bytes(body[0..4].try_into().unwrap()), MAGIC);
        assert_eq!(u32::from_be_bytes(body[4..8].try_into().unwrap()), 7);
        assert_eq!(u16::from_be_bytes(body[8..10].try_into().unwrap()), 64);
        assert_eq!(u16::from_be_bytes(body[10..12].try_into().unwrap()), 32);
        assert_eq!(
            u16::from_be_bytes(body[12..14].try_into().unwrap()),
            FLAG_WINDOW
        );
        assert_eq!(
            u16::from_be_bytes(body[14..16].try_into().unwrap()),
            tiles.len() as u16
        );
        // Header, then two tiles of 32×32 pixels with a rectangle each. The page
        // reads the same offsets, so a header that grows here breaks that test too.
        assert_eq!(body.len(), 16 + 2 * (8 + 32 * 32 * 3));
        let html = page(64, 32);
        assert!(
            html.contains("view.getUint16(12)"),
            "the page reads the flags"
        );
        assert!(
            html.contains("view.getUint16(14)"),
            "the page reads the count"
        );
        assert!(html.contains("let at = 16"), "the page skips the header");
    }

    #[test]
    fn a_request_line_is_split_into_a_path_and_a_query() {
        let head = parse_head("GET /delta?seq=12 HTTP/1.1\r\nX-Token: abc\r\nAccept: */*");
        assert_eq!(head.method, "GET");
        assert_eq!(head.path, "/delta");
        assert_eq!(head.query, "seq=12");
        assert_eq!(head.token, "abc");
        assert_eq!(head.length, 0);
        assert_eq!(query_value(&head.query, "seq"), Some("12"));
        assert_eq!(query_value(&head.query, "k"), None);
    }

    #[test]
    fn a_header_name_is_read_whatever_its_case() {
        // Nothing promises a browser spells a header the way this one does.
        let head = parse_head("POST /input HTTP/1.1\r\ncontent-length: 42\r\nx-token: beef");
        assert_eq!(head.length, 42);
        assert_eq!(head.token, "beef");
    }

    #[test]
    fn the_url_carries_the_token_when_no_header_does() {
        // The first request is a link the reader opened, which can only carry it there.
        let head = parse_head("GET /?k=cafe HTTP/1.1\r\nHost: x");
        assert_eq!(head.path, "/");
        assert_eq!(head.token, "cafe");
    }

    #[test]
    fn a_header_wins_over_the_url() {
        let head = parse_head("GET /delta?k=stale HTTP/1.1\r\nX-Token: fresh");
        assert_eq!(head.token, "fresh");
    }

    #[test]
    fn the_end_of_the_headers_is_found() {
        let raw = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody";
        assert_eq!(find(raw, b"\r\n\r\n"), Some(23));
        assert_eq!(find(b"no end here", b"\r\n\r\n"), None);
    }

    #[test]
    fn a_token_is_compared_whole() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
        // An empty token must never match, or a request with none would pass.
        assert!(!constant_time_eq(b"", b""));
    }

    #[test]
    fn a_session_token_is_new_every_time() {
        let one = new_token().expect("a token");
        let two = new_token().expect("a token");
        assert_eq!(one.len(), 32);
        assert!(one.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(one, two);
    }

    #[test]
    fn only_the_tailnet_range_counts_as_one() {
        assert!(is_tailnet(Ipv4Addr::new(100, 80, 26, 90)));
        assert!(is_tailnet(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_tailnet(Ipv4Addr::new(100, 127, 255, 255)));
        // 100.128.x is outside 100.64.0.0/10, and 10.x is an ordinary private net.
        assert!(!is_tailnet(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!is_tailnet(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(!is_tailnet(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn a_given_address_wins_over_the_tailnet_one() {
        let asked = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let options = Options::resolved(Some(asked), Some(7000));
        assert_eq!(options.bind, asked);
        assert_eq!(options.port, 7000);
        assert_eq!(options.socket().to_string(), "127.0.0.1:7000");
    }

    #[test]
    fn the_default_port_is_used_when_none_is_given() {
        let options = Options::resolved(Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), None);
        assert_eq!(options.port, DEFAULT_PORT);
    }

    #[test]
    fn every_key_the_page_sends_is_known() {
        // The page's own table, which must not drift from the one above it.
        for name in [
            "Return",
            "Tab",
            "BackTab",
            "BackSpace",
            "Delete",
            "Escape",
            "Left",
            "Right",
            "Up",
            "Down",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Space",
        ] {
            assert!(named_key(name).is_some(), "{name} is not mapped");
        }
        assert!(named_key("F13").is_none());
        assert!(named_key("").is_none());
    }

    #[test]
    fn the_page_names_the_display_size_and_the_magic_word() {
        let html = page(1280, 800);
        assert!(html.contains(r#"width="1280""#));
        assert!(html.contains(r#"height="800""#));
        // The client checks the same magic word the encoder writes.
        assert!(html.contains("0x49435631"));
        // The braces of the script survived the format string.
        assert!(html.contains("const TOKEN"));
        assert!(!html.contains("{{"));
    }

    #[test]
    fn input_is_read_as_the_page_sends_it() {
        let body = br#"[{"t":"text","v":"ab"},{"t":"key","v":"Return"},
                       {"t":"mouse","x":10,"y":20,"down":true}]"#;
        let events: Vec<Event> = serde_json::from_slice(body).expect("the events");
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], Event::Text { v } if v == "ab"));
        assert!(matches!(&events[1], Event::Key { v } if v == "Return"));
        assert!(matches!(
            events[2],
            Event::Mouse {
                x: 10,
                y: 20,
                down: true
            }
        ));
    }

    /// One request over a fresh connection, with the body inflated when it came
    /// gzipped. Enough HTTP to be a browser for the length of a test.
    fn ask(addr: SocketAddr, request: &str) -> (u16, Vec<u8>) {
        use std::io::Read as _;
        let mut stream = TcpStream::connect(addr).expect("connect to the viewer");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("a read timeout");
        stream
            .write_all(request.as_bytes())
            .expect("write the request");
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut head = None;
        let mut length = 0usize;
        loop {
            match head {
                None => {
                    if let Some(at) = find(&raw, b"\r\n\r\n") {
                        let parsed = String::from_utf8_lossy(&raw[..at]).to_string();
                        for line in parsed.lines() {
                            if let Some((name, value)) = line.split_once(':') {
                                if name.eq_ignore_ascii_case("content-length") {
                                    length = value.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                        head = Some((at, parsed));
                        continue;
                    }
                }
                Some((at, _)) if raw.len() >= at + 4 + length => break,
                _ => {}
            }
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&chunk[..n]),
                Err(e) => panic!("the viewer stopped answering: {e}"),
            }
        }
        let (at, parsed) = head.expect("a complete response head");
        let status: u16 = parsed
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status code");
        let body = raw[at + 4..].to_vec();
        if parsed.to_lowercase().contains("content-encoding: gzip") {
            let mut back = Vec::new();
            flate2::read::GzDecoder::new(&body[..])
                .read_to_end(&mut back)
                .expect("inflate the body");
            return (status, back);
        }
        (status, body)
    }

    /// The whole server, against a real display and a real window: the page is
    /// served, the picture arrives, a typed line reaches a window that is not ours,
    /// and Finish ends the session.
    ///
    /// Ignored by default because it needs `Xvfb` and `xterm` — run it with
    /// `cargo test --lib -- --ignored --nocapture a_browser_client`. No unit test can
    /// catch what this one can: a header the page reads at the wrong offset, a token
    /// check that lets a request through, or input that goes nowhere.
    #[test]
    #[ignore = "needs Xvfb and xterm on this machine"]
    fn a_browser_client_sees_the_display_and_types_into_it() {
        use std::process::{Command, Stdio};

        for tool in ["Xvfb", "xterm"] {
            let missing = Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {tool}"))
                .stdout(Stdio::null())
                .status()
                .map(|s| !s.success())
                .unwrap_or(true);
            if missing {
                eprintln!("skipped: {tool} is not installed");
                return;
            }
        }

        let proof = format!("/tmp/intune-container-webview-{}", std::process::id());
        let _ = std::fs::remove_file(&proof);

        let xvfb = crate::xvfb::Xvfb::start(800, 600, None).expect("start Xvfb");
        let screen = XScreen::connect(&xvfb.display()).expect("connect to the display");
        let mut xterm = Command::new("xterm")
            .env("DISPLAY", xvfb.display())
            .args(["-geometry", "80x24+0+0", "-bg", "white", "-fg", "black"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn xterm");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a test port");
        let addr = listener.local_addr().expect("the test port");
        let token = "0123456789abcdef0123456789abcdef";

        let expect = proof.clone();
        let client = std::thread::spawn(move || {
            let get = |path: &str, token: &str| {
                ask(
                    addr,
                    &format!(
                        "GET {path} HTTP/1.1\r\nHost: test\r\nX-Token: {token}\r\nConnection: close\r\n\r\n"
                    ),
                )
            };

            // A request with the wrong token, and one with none, must both be
            // refused before anything of the display is sent.
            let (status, _) = get("/delta?seq=0", "not-the-token");
            assert_eq!(status, 403, "a wrong token was served");
            let (status, _) = get("/delta?seq=0", "");
            assert_eq!(status, 403, "a missing token was served");
            // The browser asks for this one by itself, with no token to give.
            let (status, _) = get("/favicon.ico", "");
            assert_eq!(status, 204, "the favicon left an error in the console");

            let (status, page) = get("/", token);
            assert_eq!(status, 200);
            let page = String::from_utf8_lossy(&page);
            assert!(page.contains("<canvas"), "the page has no canvas");
            assert!(
                page.contains(r#"width="800""#),
                "the page has the wrong size"
            );

            // The first delta is the whole display: 800×600 is 25×19 tiles.
            let (status, body) = get("/delta?seq=0", token);
            assert_eq!(status, 200);
            assert_eq!(u32::from_be_bytes(body[0..4].try_into().unwrap()), MAGIC);
            let seq = u32::from_be_bytes(body[4..8].try_into().unwrap());
            let count = u16::from_be_bytes(body[14..16].try_into().unwrap());
            assert_eq!(count, 25 * 19, "the first delta was not the whole display");
            assert!(seq > 0, "the first delta carried no sequence number");

            // xterm's background is white, so the picture must hold a white pixel:
            // that is the proof the tiles carry the display and not zeroes.
            let mut white = false;
            for _ in 0..100 {
                let (_, body) = get("/delta?seq=0", token);
                let flags = u16::from_be_bytes(body[12..14].try_into().unwrap());
                white = body[16..].chunks(3).any(|p| p == [255, 255, 255]);
                if white && flags & FLAG_WINDOW != 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(white, "the window never appeared in a delta");

            // Type into a window that belongs to xterm, not to us.
            let events =
                format!(r#"[{{"t":"text","v":"touch {expect}"}},{{"t":"key","v":"Return"}}]"#);
            let (status, _) = ask(
                addr,
                &format!(
                    "POST /input HTTP/1.1\r\nHost: test\r\nX-Token: {token}\r\n\
                     Content-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{events}",
                    events.len()
                ),
            );
            assert_eq!(status, 200, "the input was refused");

            let mut landed = false;
            for _ in 0..100 {
                if std::path::Path::new(&expect).exists() {
                    landed = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }

            let (status, _) = ask(
                addr,
                &format!(
                    "POST /quit HTTP/1.1\r\nHost: test\r\nX-Token: {token}\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            );
            assert_eq!(status, 200);
            landed
        });

        // The server runs here, in the thread that owns the X connection, and
        // returns when the client presses Finish.
        let session = serve_on(&screen, listener, token.to_string()).expect("serve");
        let landed = client.join().expect("the client thread");

        let _ = xterm.kill();
        let _ = xterm.wait();
        let _ = std::fs::remove_file(&proof);
        drop(xvfb);

        assert!(!session.portal_gone, "no portal was running in this test");
        assert!(
            landed,
            "the typed line never reached the window — the input path is broken"
        );
    }

    #[test]
    fn a_gzip_body_carries_the_bytes_back() {
        use std::io::Read as _;
        let body = encode(1, &frame(64, 32, [9, 9, 9]), 0, &[]);
        let packed = gzip(&body).expect("gzip");
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&packed[..])
            .read_to_end(&mut back)
            .expect("inflate");
        assert_eq!(back, body);
    }
}
