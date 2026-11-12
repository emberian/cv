//! The HTTP/1.1 server under `cv serve`: one thread per connection over `std::net::TcpListener`,
//! built for a handful of local pollers and a browser, not for the internet at large.
//!
//! What it guarantees, each pinned by a test here or in `tests/serve.rs`:
//! - **Framing.** A request line or header line over 8 KiB, more than 128 headers or a head over
//!   64 KiB is refused (414/431). Bare LF, a malformed field, a `Content-Length` that is not plain
//!   digits or disagrees with another, `Transfer-Encoding` other than one `chunked`, or both
//!   framings at once is a 400 and the connection closes, so no two parsers can disagree on where
//!   the next request starts.
//! - **Deadlines.** A connection must send its first byte within [`Limits::first_byte`] (also
//!   the keep-alive idle time) and the whole request, body included, within [`Limits::request`]
//!   of that byte. A write that makes no progress for [`WRITE_STALL`] ends the connection.
//! - **Load shedding.** At most [`Limits::connections`] connections are served at once; one more
//!   is answered 503 and closed by the accept loop, which never waits on a busy connection.
//! - **The accept loop never ends.** An accept error (EMFILE, ECONNABORTED, …) backs off and
//!   retries; a thread that cannot be spawned costs one connection. Nothing here unwraps I/O.

use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_LINE: usize = 8192;
const MAX_HEADERS: usize = 128;
const MAX_HEAD: usize = 64 * 1024;
/// An unread request body up to this size is skipped to keep the connection; a larger one closes it.
const DRAIN_LIMIT: u64 = 1 << 20;
const WRITE_STALL: Duration = Duration::from_secs(30);
/// How long a closing connection keeps reading what the client already sent, so the kernel does
/// not answer that unread input with a reset that destroys the response before it is read.
const LINGER: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub first_byte: Duration,
    pub request: Duration,
    pub connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            first_byte: Duration::from_secs(5),
            request: Duration::from_secs(30),
            connections: 64,
        }
    }
}

/// Serve `listener` forever, calling `handler` once per request.
pub(crate) fn serve<H>(listener: TcpListener, limits: Limits, handler: H) -> !
where
    H: Fn(&mut Request<'_>) + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let active = Arc::new(AtomicUsize::new(0));
    let shed = shedder(limits.connections);
    let mut backoff = Duration::ZERO;
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => {
                backoff = Duration::ZERO;
                stream
            }
            Err(e) => {
                backoff = (backoff * 2).clamp(Duration::from_millis(5), Duration::from_secs(1));
                eprintln!("cv serve: accept failed ({e}); retrying in {backoff:?}");
                std::thread::sleep(backoff);
                continue;
            }
        };
        if active.load(Ordering::Acquire) >= limits.connections {
            // Past capacity the shedder answers; when even it is full, the connection is dropped.
            let _ = shed.try_send(stream);
            continue;
        }
        active.fetch_add(1, Ordering::AcqRel);
        let slot = Slot(Arc::clone(&active));
        let handler = Arc::clone(&handler);
        let env = cv_core::env::Env::scoped();
        let spawned = std::thread::Builder::new().name("cv-serve-conn".into()).spawn(move || {
            let _env = env.map(cv_core::env::Env::reenter);
            let _slot = slot;
            connection(stream, limits, &*handler);
        });
        if let Err(e) = spawned {
            eprintln!("cv serve: dropped a connection, no thread for it ({e})");
        }
    }
}

/// The thread that answers shed connections 503 without holding up the accept loop: each one is
/// written once (a fresh socket's send buffer is empty, so it cannot block), then read without
/// blocking for up to [`LINGER`] so the request the client already sent is not answered by a reset
/// that would destroy the 503 before it is read.
fn shedder(capacity: usize) -> std::sync::mpsc::SyncSender<TcpStream> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<TcpStream>(capacity);
    let spawned = std::thread::Builder::new().name("cv-serve-shed".into()).spawn(move || {
        let mut lingering: Vec<(TcpStream, Instant)> = Vec::new();
        loop {
            let next = if lingering.is_empty() {
                rx.recv().ok()
            } else {
                rx.recv_timeout(Duration::from_millis(10)).ok()
            };
            if let Some(stream) = next {
                let _ = stream.set_nonblocking(true);
                let _ = (&stream)
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = stream.shutdown(Shutdown::Write);
                lingering.push((stream, Instant::now() + LINGER));
            }
            reap_shed(&mut lingering, capacity);
        }
    });
    if let Err(e) = spawned {
        // Without the shedder every excess connection is simply dropped.
        eprintln!("cv serve: no thread to answer excess connections ({e})");
    }
    tx
}

fn reap_shed(lingering: &mut Vec<(TcpStream, Instant)>, capacity: usize) {
    // The input queue alone does not bound retained descriptors when arrivals outrun LINGER.
    lingering.truncate(capacity);
    lingering.retain(|(stream, until)| {
        if Instant::now() >= *until {
            return false;
        }
        // A continuously sending peer cannot monopolize the sole shedder's cleanup pass.
        const QUANTUM: u64 = 64 * 1024;
        match io::copy(&mut stream.take(QUANTUM), &mut io::sink()) {
            Ok(read) => read == QUANTUM,
            Err(error) => error.kind() == io::ErrorKind::WouldBlock,
        }
    });
}

/// One admitted connection; dropping it (thread end, panic or failed spawn) frees its place.
struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The socket under a deadline: every read sets its timeout to the time left, so a client that
/// trickles bytes cannot stretch a request past the deadline.
struct Conn {
    stream: TcpStream,
    deadline: Instant,
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf).map_err(|e| match e.kind() {
            io::ErrorKind::WouldBlock => io::ErrorKind::TimedOut.into(),
            _ => e,
        })
    }
}

fn connection(stream: TcpStream, limits: Limits, handler: &(dyn Fn(&mut Request<'_>) + Sync)) {
    let _ = stream.set_nodelay(true);
    if stream.set_write_timeout(Some(WRITE_STALL)).is_err() {
        return;
    }
    let mut src = BufReader::new(Conn {
        stream,
        deadline: Instant::now() + limits.first_byte,
    });
    loop {
        src.get_mut().deadline = Instant::now() + limits.first_byte;
        // Idle, closed or broken before a request began: nothing is owed, so just close.
        if !matches!(src.fill_buf(), Ok(b) if !b.is_empty()) {
            return;
        }
        src.get_mut().deadline = Instant::now() + limits.request;
        let head = match read_head(&mut src) {
            Ok(head) => head,
            Err(Some(status)) => {
                let _ = Response::empty(status).write_to(&src.get_ref().stream, false, false);
                break;
            }
            Err(None) => break,
        };
        let keep_alive = head.version_11 && !head.has_token("Connection", "close");
        if head.expect_continue && !matches!(head.framing, Framing::Length(0)) {
            let _ = (&src.get_ref().stream).write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        let mut request = Request {
            head,
            src: &mut src,
            keep_alive,
            responded: false,
            broken: false,
        };
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&mut request)));
        if caught.is_err() {
            eprintln!("cv serve: request handler panicked; connection closed");
            request.keep_alive = false;
        }
        if !request.responded {
            request.keep_alive = false;
            request.respond(Response::empty(500));
        }
        // Skip a small unread body to reach the next request; anything else ends the connection.
        let reusable = request.keep_alive
            && !request.broken
            && io::copy(&mut (&mut request).take(DRAIN_LIMIT + 1), &mut io::sink()).is_ok()
            && request.head.framing.is_done();
        if !reusable {
            break;
        }
    }
    let _ = src.get_ref().stream.shutdown(Shutdown::Write);
    src.get_mut().deadline = Instant::now() + LINGER;
    let _ = io::copy(&mut src.take(DRAIN_LIMIT), &mut io::sink());
}

struct Head {
    method: String,
    url: String,
    version_11: bool,
    headers: Vec<(String, String)>,
    framing: Framing,
    expect_continue: bool,
}

impl Head {
    fn values<'h>(&'h self, name: &'h str) -> impl Iterator<Item = &'h str> + 'h {
        self.headers
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn has_token(&self, name: &str, token: &str) -> bool {
        self.values(name)
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(token))
    }
}

enum Framing {
    Length(u64),
    Chunked { left: u64, started: bool, done: bool },
}

impl Framing {
    fn is_done(&self) -> bool {
        matches!(self, Framing::Length(0) | Framing::Chunked { done: true, .. })
    }
}

enum LineError {
    TooLong,
    Malformed,
    Io(io::Error),
}

/// One CRLF-terminated line of at most [`MAX_LINE`] bytes, without its CRLF. A bare CR or LF is
/// malformed: it is where two parsers would split the same bytes differently.
fn read_line(src: &mut impl BufRead, out: &mut Vec<u8>) -> Result<(), LineError> {
    out.clear();
    src.take(MAX_LINE as u64 + 1)
        .read_until(b'\n', out)
        .map_err(LineError::Io)?;
    if out.len() > MAX_LINE {
        return Err(LineError::TooLong);
    }
    if out.last() != Some(&b'\n') {
        return Err(LineError::Io(io::ErrorKind::UnexpectedEof.into()));
    }
    out.truncate(out.len() - 1);
    if out.pop() != Some(b'\r') || out.contains(&b'\r') {
        return Err(LineError::Malformed);
    }
    Ok(())
}

fn is_token(s: &[u8]) -> bool {
    !s.is_empty()
        && s.iter()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
}

/// Read and validate a request head. `Err(Some(status))` is answered; `Err(None)` just closes.
fn read_head(src: &mut BufReader<Conn>) -> Result<Head, Option<u16>> {
    let mut line = Vec::new();
    let mut total = 0;
    let fail = |too_long: u16| {
        move |e: LineError| match e {
            LineError::TooLong => Some(too_long),
            LineError::Malformed => Some(400),
            LineError::Io(e) if e.kind() == io::ErrorKind::TimedOut => Some(408),
            LineError::Io(_) => None,
        }
    };
    // A client may precede a request with empty lines (RFC 9112 §2.2); they count toward the head.
    loop {
        read_line(src, &mut line).map_err(fail(414))?;
        total += line.len() + 2;
        if !line.is_empty() {
            break;
        }
        if total > MAX_HEAD {
            return Err(Some(400));
        }
    }
    let request_line = std::str::from_utf8(&line).map_err(|_| Some(400))?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(url), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Some(400));
    };
    let version_11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        v if v.starts_with("HTTP/") => return Err(Some(505)),
        _ => return Err(Some(400)),
    };
    if !is_token(method.as_bytes()) || url.is_empty() || !url.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Some(400));
    }
    let (method, url) = (method.to_string(), url.to_string());
    let mut headers = Vec::new();
    loop {
        read_line(src, &mut line).map_err(fail(431))?;
        if line.is_empty() {
            break;
        }
        total += line.len() + 2;
        if total > MAX_HEAD || headers.len() >= MAX_HEADERS {
            return Err(Some(431));
        }
        let colon = line.iter().position(|&b| b == b':').ok_or(Some(400))?;
        let (name, value) = (&line[..colon], line[colon + 1..].trim_ascii());
        if !is_token(name) || !value.iter().all(|&b| b == b'\t' || (b' '..=b'~').contains(&b)) {
            return Err(Some(400));
        }
        // Both halves are ASCII by the checks above.
        headers.push((
            String::from_utf8_lossy(name).into_owned(),
            String::from_utf8_lossy(value).into_owned(),
        ));
    }
    let mut head = Head {
        method,
        url,
        version_11,
        headers,
        framing: Framing::Length(0),
        expect_continue: false,
    };
    if head.values("Host").count() > 1 {
        return Err(Some(400));
    }
    head.framing = framing(&head).ok_or(Some(400))?;
    let expect_continue = match head.values("Expect").next() {
        None => false,
        Some(v) if v.eq_ignore_ascii_case("100-continue") => true,
        Some(_) => return Err(Some(417)),
    };
    head.expect_continue = expect_continue;
    Ok(head)
}

/// The body framing, or `None` where it is ambiguous or unsupported.
fn framing(head: &Head) -> Option<Framing> {
    let te: Vec<&str> = head.values("Transfer-Encoding").collect();
    let mut lengths = head.values("Content-Length").peekable();
    if !te.is_empty() {
        let only_chunked = te.len() == 1 && te[0].eq_ignore_ascii_case("chunked");
        return (only_chunked && lengths.peek().is_none()).then_some(Framing::Chunked {
            left: 0,
            started: false,
            done: false,
        });
    }
    let mut length = None;
    for v in lengths {
        let n: u64 = v.bytes().all(|b| b.is_ascii_digit()).then(|| v.parse().ok())??;
        if length.is_some_and(|l| l != n) {
            return None;
        }
        length = Some(n);
    }
    Some(Framing::Length(length.unwrap_or(0)))
}

/// One request: its head, and a reader over exactly its body.
pub(crate) struct Request<'c> {
    head: Head,
    src: &'c mut BufReader<Conn>,
    keep_alive: bool,
    responded: bool,
    broken: bool,
}

impl Request<'_> {
    pub(crate) fn method(&self) -> &str {
        &self.head.method
    }

    /// The request target as sent: path plus query.
    pub(crate) fn url(&self) -> &str {
        &self.head.url
    }

    /// The first value of a header, matched case-insensitively.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        let mut found = self.head.headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name));
        found.next().map(|(_, v)| v.as_str())
    }

    /// Send the one response to this request; a second call is ignored.
    pub(crate) fn respond(&mut self, response: Response<'_>) {
        if self.responded {
            return;
        }
        self.responded = true;
        if response.is_sse() {
            self.keep_alive = false;
        }
        let head_only = self.head.method == "HEAD";
        let stream = &self.src.get_ref().stream;
        let peer_closed = || -> bool {
            let mut one = [0u8; 1];
            if stream.set_nonblocking(true).is_err() {
                return false;
            }
            let closed = matches!(stream.peek(&mut one), Ok(0));
            let _ = stream.set_nonblocking(false);
            closed
        };
        if response
            .write_to_with(
                stream,
                self.keep_alive,
                head_only,
                self.head.version_11,
                Some(&peer_closed),
            )
            .is_err()
        {
            self.broken = true;
        }
    }
}

/// The request body, de-chunked; it ends at the framed length, and a short body is an error.
impl Read for Request<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.broken {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.read_body(buf).inspect_err(|_| {
            // Framing cannot recover after consuming malformed bytes. In particular, the
            // post-response drain must never turn an errored chunk into another request.
            self.broken = true;
            self.keep_alive = false;
        })
    }
}

impl Request<'_> {
    fn read_body(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
        let src = &mut *self.src;
        let left = match &mut self.head.framing {
            Framing::Length(left) => left,
            Framing::Chunked { done: true, .. } => return Ok(0),
            Framing::Chunked { left, started, done } => {
                if *left == 0 {
                    let mut line = Vec::new();
                    if *started {
                        read_line(src, &mut line).map_err(line_io)?;
                        if !line.is_empty() {
                            return Err(bad("chunk overruns its size"));
                        }
                    }
                    *started = true;
                    read_line(src, &mut line).map_err(line_io)?;
                    let end = line.iter().position(|&b| b == b';').unwrap_or(line.len());
                    let size = line[..end].trim_ascii_end();
                    if size.is_empty() || !size.iter().all(u8::is_ascii_hexdigit) {
                        return Err(bad("malformed chunk size"));
                    }
                    let size = std::str::from_utf8(size)
                        .ok()
                        .and_then(|s| u64::from_str_radix(s, 16).ok());
                    *left = size.ok_or_else(|| bad("chunk size overflows"))?;
                    if *left == 0 {
                        for _ in 0..MAX_HEADERS {
                            read_line(src, &mut line).map_err(line_io)?;
                            if line.is_empty() {
                                *done = true;
                                return Ok(0);
                            }
                        }
                        return Err(bad("too many trailer fields"));
                    }
                }
                left
            }
        };
        if *left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
        let n = src.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        *left -= n as u64;
        Ok(n)
    }
}

fn line_io(e: LineError) -> io::Error {
    match e {
        LineError::Io(e) => e,
        LineError::TooLong | LineError::Malformed => io::Error::new(io::ErrorKind::InvalidData, "malformed chunk line"),
    }
}

type SseBodyFn<'a> = Box<dyn FnOnce(&mut SseWriter<'_>) -> io::Result<()> + 'a>;

enum ResponseBody<'a> {
    Sized { body: Box<dyn Read + 'a>, len: u64 },
    Sse(SseBodyFn<'a>),
}

/// Framed writer for `text/event-stream` responses (chunked on HTTP/1.1, EOF-delimited on HTTP/1.0).
pub(crate) struct SseWriter<'w> {
    out: &'w mut dyn Write,
    chunked: bool,
    peer_closed: Option<&'w dyn Fn() -> bool>,
    scratch: Vec<u8>,
}

impl<'w> SseWriter<'w> {
    /// True once the client has closed its write half, allowing idle streams to exit promptly.
    pub(crate) fn peer_closed(&self) -> bool {
        self.peer_closed.is_some_and(|check| check())
    }

    /// Emit one SSE event (`id:`, `event:`, one `data:` line per `\n`, blank line) and flush.
    pub(crate) fn event(&mut self, id: Option<&str>, event: Option<&str>, data: &str) -> io::Result<()> {
        self.scratch.clear();
        if let Some(id) = id {
            self.scratch.extend_from_slice(b"id: ");
            push_sse_field(&mut self.scratch, id);
            self.scratch.push(b'\n');
        }
        if let Some(event) = event {
            self.scratch.extend_from_slice(b"event: ");
            push_sse_field(&mut self.scratch, event);
            self.scratch.push(b'\n');
        }
        for line in data.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            self.scratch.extend_from_slice(b"data: ");
            self.scratch.extend_from_slice(line.as_bytes());
            self.scratch.push(b'\n');
        }
        self.scratch.push(b'\n');
        self.flush_frame()
    }

    /// Emit an SSE comment (`: ...\n\n`) heartbeat and flush.
    pub(crate) fn comment(&mut self, text: &str) -> io::Result<()> {
        self.scratch.clear();
        self.scratch.extend_from_slice(b": ");
        push_sse_field(&mut self.scratch, text);
        self.scratch.extend_from_slice(b"\n\n");
        self.flush_frame()
    }

    fn flush_frame(&mut self) -> io::Result<()> {
        if self.scratch.is_empty() {
            return Ok(());
        }
        if self.chunked {
            write!(self.out, "{:x}\r\n", self.scratch.len())?;
            self.out.write_all(&self.scratch)?;
            self.out.write_all(b"\r\n")?;
        } else {
            self.out.write_all(&self.scratch)?;
        }
        self.out.flush()
    }
}

fn push_sse_field(out: &mut Vec<u8>, s: &str) {
    for b in s.bytes() {
        if b != b'\r' && b != b'\n' && b != 0 {
            out.push(b);
        }
    }
}

/// A response: status, headers, and a body of known length or a server-sent event stream.
pub(crate) struct Response<'a> {
    status: u16,
    headers: Vec<(String, String)>,
    body: ResponseBody<'a>,
}

impl<'a> Response<'a> {
    pub(crate) fn bytes(status: u16, body: Vec<u8>) -> Response<'static> {
        let len = body.len() as u64;
        Response {
            status,
            headers: Vec::new(),
            body: ResponseBody::Sized {
                body: Box::new(io::Cursor::new(body)),
                len,
            },
        }
    }

    pub(crate) fn empty(status: u16) -> Response<'static> {
        Response::bytes(status, Vec::new())
    }

    /// A streamed body that must yield exactly `len` bytes; a short stream ends the connection.
    pub(crate) fn stream(status: u16, body: Box<dyn Read + 'a>, len: u64) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: ResponseBody::Sized { body, len },
        }
    }

    /// A `text/event-stream` response streamed as HTTP/1.1 chunks with write-stall deadlines.
    pub(crate) fn sse(stream_fn: impl FnOnce(&mut SseWriter<'_>) -> io::Result<()> + 'a) -> Self {
        let mut resp = Response {
            status: 200,
            headers: Vec::new(),
            body: ResponseBody::Sse(Box::new(stream_fn)),
        };
        resp.add_header("Content-Type", "text/event-stream; charset=utf-8");
        resp.add_header("Cache-Control", "no-cache");
        resp.add_header("X-Accel-Buffering", "no");
        resp
    }

    pub(crate) fn is_sse(&self) -> bool {
        matches!(self.body, ResponseBody::Sse(_))
    }

    /// Add a header. One that could not be sent intact (not a token name, or a value outside
    /// printable ASCII and tab) is dropped rather than allowed to split the response.
    pub(crate) fn add_header(&mut self, name: &str, value: &str) {
        if is_token(name.as_bytes()) && value.bytes().all(|b| b == b'\t' || (b' '..=b'~').contains(&b)) {
            self.headers.push((name.to_string(), value.to_string()));
        }
    }

    pub(crate) fn write_to(self, out: impl Write, keep_alive: bool, head_only: bool) -> io::Result<()> {
        self.write_to_with(out, keep_alive, head_only, true, None)
    }

    pub(crate) fn write_to_with(
        self,
        out: impl Write,
        keep_alive: bool,
        head_only: bool,
        version_11: bool,
        peer_closed: Option<&dyn Fn() -> bool>,
    ) -> io::Result<()> {
        let mut out = BufWriter::new(out);
        let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT");
        write!(
            out,
            "HTTP/1.1 {} {}\r\nDate: {date}\r\n",
            self.status,
            reason(self.status)
        )?;
        for (name, value) in &self.headers {
            write!(out, "{name}: {value}\r\n")?;
        }
        let bodiless = self.status == 204 || self.status == 304 || self.status < 200;
        match self.body {
            ResponseBody::Sized { mut body, len } => {
                if !bodiless {
                    write!(out, "Content-Length: {len}\r\n")?;
                }
                if !keep_alive {
                    out.write_all(b"Connection: close\r\n")?;
                }
                out.write_all(b"\r\n")?;
                if !bodiless && !head_only {
                    let sent = io::copy(&mut (&mut body).take(len), &mut out)?;
                    if sent != len {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "response body ended early",
                        ));
                    }
                    // A body longer than its declared length lies as much as a short one; its tail must
                    // not vanish behind a response that looks complete. Every body is a staged file or a
                    // buffer, so this is one read that answers EOF when the length is right.
                    let mut surplus = [0u8; 1];
                    if body.read(&mut surplus)? != 0 {
                        out.flush()?;
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "response body exceeds its Content-Length",
                        ));
                    }
                }
                out.flush()
            }
            ResponseBody::Sse(stream_fn) => {
                if version_11 && !bodiless {
                    out.write_all(b"Transfer-Encoding: chunked\r\n")?;
                }
                out.write_all(b"Connection: close\r\n\r\n")?;
                out.flush()?;
                if !bodiless && !head_only {
                    let mut writer = SseWriter {
                        out: &mut out,
                        chunked: version_11,
                        peer_closed,
                        scratch: Vec::with_capacity(256),
                    };
                    stream_fn(&mut writer)?;
                    if version_11 {
                        out.write_all(b"0\r\n\r\n")?;
                    }
                }
                out.flush()
            }
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        414 => "URI Too Long",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_body_longer_than_its_length_is_an_error_after_the_declared_bytes() {
        let mut out = Vec::new();
        let error = super::Response::stream(200, Box::new(std::io::Cursor::new(b"abcdef".to_vec())), 4)
            .write_to(&mut out, true, false)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(out.ends_with(b"\r\n\r\nabcd"), "{}", String::from_utf8_lossy(&out));
        let mut out = Vec::new();
        super::Response::stream(200, Box::new(std::io::Cursor::new(b"abcd".to_vec())), 4)
            .write_to(&mut out, true, false)
            .unwrap();
    }

    use super::*;

    #[test]
    fn shed_connections_cannot_accumulate_past_capacity() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peers = Vec::new();
        let mut lingering = Vec::new();
        for _ in 0..3 {
            let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let (stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(true).unwrap();
            peers.push(peer);
            lingering.push((stream, Instant::now() + LINGER));
        }
        reap_shed(&mut lingering, 1);
        assert_eq!(lingering.len(), 1, "rejected connections bypassed the connection cap");
        for peer in &mut peers[1..] {
            assert_eq!(peer.read(&mut [0]).unwrap(), 0, "excess socket was not closed");
        }
    }

    /// A server with `limits` whose handler echoes the body length, or panics on `/panic`.
    fn spawn(limits: Limits) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            serve(listener, limits, |request| {
                assert_ne!(request.url(), "/panic", "deliberate");
                let mut body = Vec::new();
                let status = if request.read_to_end(&mut body).is_ok() {
                    200
                } else {
                    400
                };
                request.respond(Response::bytes(status, body.len().to_string().into_bytes()));
            })
        });
        port
    }

    fn quick() -> Limits {
        Limits {
            first_byte: Duration::from_millis(300),
            request: Duration::from_millis(600),
            connections: 2,
        }
    }

    fn exchange(port: u16, bytes: &[u8]) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(bytes).unwrap();
        let mut out = String::new();
        let _ = stream.read_to_string(&mut out);
        out
    }

    #[test]
    fn bodies_are_framed_by_length_or_chunks_and_pipelining_survives() {
        let port = spawn(Limits::default());
        let out = exchange(
            port,
            b"POST / HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcPOST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
              2;ext=1\r\nab\r\n3\r\ncde\r\n0\r\nTrailer: x\r\n\r\nGET / HTTP/1.1\r\nConnection: close\r\n\r\n",
        );
        let bodies: Vec<&str> = out
            .split("\r\n\r\n")
            .skip(1)
            .map(|b| b.get(..1).unwrap_or(""))
            .collect();
        assert_eq!(out.matches("HTTP/1.1 200 OK").count(), 3, "{out}");
        assert_eq!(bodies, ["3", "5", "0"], "{out}");
        for bad in [
            "zz\r\n",
            "+5\r\nabcde\r\n0\r\n\r\n",
            "fffffffffffffffff\r\n",
            "2\r\nabc\r\n0\r\n\r\n",
        ] {
            let request = format!("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{bad}");
            assert!(
                exchange(port, request.as_bytes()).starts_with("HTTP/1.1 400"),
                "{bad:?}"
            );
        }
        let continued = exchange(
            port,
            b"POST / HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
        );
        assert!(
            continued.starts_with("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK"),
            "{continued}"
        );
        assert!(exchange(port, b"GET / HTTP/1.1\nConnection: close\n\n").starts_with("HTTP/1.1 400"));
        assert!(exchange(port, b"GET / HTTP/2.0\r\n\r\n").starts_with("HTTP/1.1 505"));
        let long = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_LINE));
        assert!(exchange(port, long.as_bytes()).starts_with("HTTP/1.1 414"));
    }

    #[test]
    fn malformed_body_cannot_resume_parsing_a_pipeline() {
        let port = spawn(quick());
        for broken_chunk in ["zz\r\n", "1\r\nxBAD\r\n"] {
            let request = format!(
                "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{broken_chunk}\r\n0\r\n\r\n\
                 GET / HTTP/1.1\r\nConnection: close\r\n\r\n"
            );
            let out = exchange(port, request.as_bytes());
            assert!(out.starts_with("HTTP/1.1 400"), "{out}");
            assert_eq!(out.matches("HTTP/1.1 ").count(), 1, "{out}");
            assert!(out.contains("Connection: close\r\n"), "{out}");
        }
    }

    #[test]
    fn idle_and_trickling_connections_meet_their_deadlines() {
        let port = spawn(quick());
        let started = Instant::now();
        assert_eq!(exchange(port, b""), "", "an idle connection closes without a response");
        assert!(started.elapsed() < Duration::from_secs(2));
        // A head still unfinished at the request deadline is a 408, however steadily it trickles.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let started = Instant::now();
        let mut out = String::new();
        for _ in 0..12 {
            if stream.write_all(b"X").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = stream.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 408"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(2));
        // A body that stalls is an error the handler sees, not a worker held forever.
        let out = exchange(port, b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc");
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    }

    #[test]
    fn excess_connections_are_shed_and_panics_cost_one_response() {
        let port = spawn(quick());
        let held: Vec<TcpStream> = (0..2)
            .map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap())
            .collect();
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        assert!(exchange(port, b"").starts_with("HTTP/1.1 503"));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "shedding waited on a held connection"
        );
        drop(held);
        std::thread::sleep(Duration::from_millis(400));
        let out = exchange(port, b"GET /panic HTTP/1.1\r\n\r\nGET / HTTP/1.1\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 500") && !out.contains("200 OK"), "{out}");
        assert!(exchange(port, b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n").starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn response_headers_cannot_split_the_response() {
        let mut response = Response::empty(200);
        response.add_header("X-Ok", "fine\tvalue");
        response.add_header("X-Bad", "a\r\nInjected: 1");
        response.add_header("Bad Name", "x");
        let mut out = Vec::new();
        response.write_to(&mut out, true, false).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("X-Ok: fine\tvalue\r\n") && !out.contains("Injected") && !out.contains("Bad Name"));
    }

    #[test]
    fn sse_frames_events_as_http11_chunks_and_http10_eof() {
        let mut out = Vec::new();
        Response::sse(|sse| {
            sse.comment("keepalive")?;
            sse.event(Some("7\r\nbad"), Some("change\n"), "{\"cursor\":7}\nsecond")
        })
        .write_to_with(&mut out, true, false, true, None)
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("Content-Type: text/event-stream; charset=utf-8\r\n"),
            "{text}"
        );
        assert!(
            text.contains("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"),
            "{text}"
        );
        assert!(text.contains("\r\n: keepalive\n\n\r\n"), "{text}");
        assert!(
            text.contains("id: 7bad\nevent: change\ndata: {\"cursor\":7}\ndata: second\n\n\r\n0\r\n\r\n"),
            "{text}"
        );

        let mut out10 = Vec::new();
        Response::sse(|sse| sse.event(Some("1"), None, "ok"))
            .write_to_with(&mut out10, false, false, false, None)
            .unwrap();
        let text10 = String::from_utf8(out10).unwrap();
        assert!(!text10.contains("Transfer-Encoding"), "{text10}");
        assert!(text10.ends_with("\r\n\r\nid: 1\ndata: ok\n\n"), "{text10}");
    }
}
