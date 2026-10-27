// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A minimal HTTP/1.1 server for exercising [`HttpBlob`].
//!
//! This is hand-rolled on [`std::net::TcpListener`] rather than built on a real
//! server for two reasons: `disk_blob` depends on `hyper` with only the client
//! features enabled, so a real server would mean new dependencies; and, more
//! importantly, the interesting cases are all responses a well-behaved server
//! would never produce. See [`Behavior`].
//!
//! A [`Script`] can make the server misbehave for the first few requests only,
//! which models the transient failures that [`HttpBlob`] must recover from. The
//! server counts what it sees in [`Stats`], so that tests can wait for it to
//! reach a given point rather than sleep, and can check how many attempts a
//! read took.
//!
//! [`HttpBlob`]: crate::blob::http::HttpBlob

use parking_lot::Condvar;
use parking_lot::Mutex;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

/// How the server should respond to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Behavior {
    /// Answer with exactly the requested range.
    #[default]
    Correct,
    /// Answer with one byte less than was asked for. `Blob::read` is required
    /// to report this as `UnexpectedEof` rather than as a partial success,
    /// which is the guarantee the whole backend rests on.
    ShortBody,
    /// Ignore the `Range` header and return the whole blob with `200 OK`. Real
    /// servers do this.
    IgnoreRange,
    /// Reject the range with `416 Range Not Satisfiable`.
    RangeNotSatisfiable,
    /// Send the headers, then close the connection part way through the body.
    CloseMidBody,
    /// Read the request, then send nothing at all, holding the connection open
    /// until the client closes it or the server is released. This is what a
    /// client sees when the network silently stops delivering a connection's
    /// packets: no data, but no FIN or reset to end the wait either.
    StallBeforeResponse,
    /// Send the headers and half the body, then stall as for
    /// [`Behavior::StallBeforeResponse`].
    StallMidBody,
    /// Reset the connection instead of answering.
    ResetBeforeResponse,
    /// Send the headers and half the body, then reset the connection.
    ResetMidBody,
    /// Answer `503 Service Unavailable`, as an overloaded or throttling server
    /// does.
    ServiceUnavailable,
    /// Answer correctly, but send the body in `pieces` pieces, pausing for
    /// `interval` before each: slow, but never silent for much longer than
    /// `interval`.
    SlowBody { pieces: usize, interval: Duration },
}

/// How the server responds to each request.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// How to respond to the first HEAD requests, in order. Later ones are
    /// answered correctly. A HEAD response has no body, so a behavior that
    /// concerns the body acts as its counterpart that does not: a stall
    /// stalls before the response, a reset resets before it, and the rest
    /// answer correctly.
    pub head: Vec<Behavior>,
    /// How to respond to the first GET requests, in order.
    pub get: Vec<Behavior>,
    /// How to respond to the GET requests after those in `get`.
    pub then: Behavior,
    /// Serve further requests on a connection after a response, as real
    /// servers do, rather than closing it after each one.
    pub keep_alive: bool,
}

/// What the server has seen so far.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// HEAD requests received.
    pub heads: usize,
    /// GET requests received.
    pub gets: usize,
    /// Connections that the server has stalled.
    pub stalled: usize,
    /// Stalled connections that the client has since closed.
    pub abandoned: usize,
}

/// A server bound to an ephemeral port on the loopback interface.
pub struct TestHttpServer {
    port: u16,
    shared: Arc<Shared>,
}

struct Shared {
    content: Vec<u8>,
    script: Script,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    stats: Stats,
    released: bool,
}

impl TestHttpServer {
    /// Starts a server publishing `content`, which answers every range request
    /// as `behavior` says.
    pub fn new(content: Vec<u8>, behavior: Behavior) -> Self {
        Self::with_script(
            content,
            Script {
                then: behavior,
                ..Default::default()
            },
        )
    }

    /// Starts a server publishing `content`, which answers requests as `script`
    /// says.
    ///
    /// The listening thread runs until the process exits. Tests are short-lived
    /// and nextest gives each one its own process, so there is nothing to clean
    /// up. Stalled connections are closed when the server is dropped.
    pub fn with_script(content: Vec<u8>, script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared {
            content,
            script,
            state: Mutex::default(),
            changed: Condvar::new(),
        });
        let listener_shared = shared.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let shared = listener_shared.clone();
                // A thread per connection, so that a stalled connection does
                // not hold up the others.
                std::thread::spawn(move || {
                    // A misbehaving response deliberately drops the connection,
                    // so errors here are expected and not worth reporting.
                    let _ = shared.serve(stream);
                });
            }
        });
        Self { port, shared }
    }

    /// The URL of the blob.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/blob", self.port)
    }

    /// Returns what the server has seen so far.
    pub fn stats(&self) -> Stats {
        self.shared.state.lock().stats
    }

    /// Waits up to `timeout` for `done` to return true for the server's
    /// [`Stats`], and returns them. Panics, naming `what` it was waiting for,
    /// if `done` is still false at the end.
    pub fn wait_for(
        &self,
        what: &str,
        timeout: Duration,
        mut done: impl FnMut(&Stats) -> bool,
    ) -> Stats {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock();
        while !done(&state.stats) {
            if self
                .shared
                .changed
                .wait_until(&mut state, deadline)
                .timed_out()
                && !done(&state.stats)
            {
                panic!("timed out waiting for {what}: {:?}", state.stats);
            }
        }
        state.stats
    }

    /// Ends every stall, now and in future, by closing the connection.
    pub fn release(&self) {
        self.shared.state.lock().released = true;
        self.shared.changed.notify_all();
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        // A failing test may leave a client waiting on a stalled connection.
        // Closing the connection ends the wait.
        self.release();
    }
}

impl Shared {
    /// Serves requests on `stream` until it is closed.
    fn serve(&self, mut stream: TcpStream) -> std::io::Result<()> {
        // Send each write at once, so that the client sees the pieces and
        // pauses of a response as the behavior describes them.
        stream.set_nodelay(true)?;
        loop {
            let request = read_headers(&mut stream)?;
            // An empty request means the client has closed the connection.
            if request.is_empty()
                || !self.respond(&mut stream, &request)?
                || !self.script.keep_alive
            {
                return Ok(());
            }
        }
    }

    /// Counts a request, and returns how to respond to it: as the next of the
    /// `scripted` behaviors, if any remain, and otherwise as `then`.
    fn count(
        &self,
        counter: impl FnOnce(&mut Stats) -> &mut usize,
        scripted: &[Behavior],
        then: Behavior,
    ) -> Behavior {
        let mut state = self.state.lock();
        let count = counter(&mut state.stats);
        let behavior = scripted.get(*count).copied().unwrap_or(then);
        *count += 1;
        drop(state);
        self.changed.notify_all();
        behavior
    }

    /// Holds the connection open without sending anything, until the client
    /// closes it or the server is released.
    ///
    /// Returns false, since the connection cannot carry another request.
    fn stall(&self, stream: &mut TcpStream) -> bool {
        self.state.lock().stats.stalled += 1;
        self.changed.notify_all();
        // Poll, rather than block, so that a release is noticed promptly.
        let _ = stream.set_read_timeout(Some(Duration::from_millis(10)));
        let mut buf = [0; 512];
        while !self.state.lock().released {
            match stream.read(&mut buf) {
                // A stalled server ignores anything more that the client sends.
                Ok(n) if n > 0 => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                // The client has closed or reset the connection.
                _ => {
                    self.state.lock().stats.abandoned += 1;
                    self.changed.notify_all();
                    break;
                }
            }
        }
        false
    }

    /// Answers one request. Returns whether the connection can carry another.
    fn respond(&self, stream: &mut TcpStream, request: &str) -> std::io::Result<bool> {
        let content = &self.content;
        let mut lines = request.lines();
        let method = lines
            .next()
            .and_then(|l| l.split(' ').next())
            .unwrap_or_default()
            .to_owned();
        let close = if self.script.keep_alive {
            ""
        } else {
            "Connection: close\r\n"
        };

        // `HttpBlob::new` probes the length with a HEAD request.
        if method.eq_ignore_ascii_case("HEAD") {
            match self.count(
                |stats| &mut stats.heads,
                &self.script.head,
                Behavior::Correct,
            ) {
                Behavior::StallBeforeResponse | Behavior::StallMidBody => {
                    return Ok(self.stall(stream));
                }
                Behavior::ResetBeforeResponse | Behavior::ResetMidBody => return Ok(reset(stream)),
                Behavior::ServiceUnavailable => return service_unavailable(stream, close),
                _ => {}
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\n\
                 Content-Length: {}\r\n\
                 Accept-Ranges: bytes\r\n\
                 {close}\r\n",
                content.len()
            )?;
            return Ok(true);
        }

        let behavior = self.count(|stats| &mut stats.gets, &self.script.get, self.script.then);

        // Header names are case-insensitive, and hyper does not necessarily send
        // them in the case they were built with.
        let range = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("range")
                .then(|| value.trim().strip_prefix("bytes=").map(str::to_owned))
                .flatten()
        });
        let Some(range) = range else {
            write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n"
            )?;
            return Ok(false);
        };
        let (start, end) = range.split_once('-').unwrap();
        let start: usize = start.parse().unwrap();
        // The range is inclusive at both ends.
        let end: usize = end.parse().unwrap();

        match behavior {
            Behavior::StallBeforeResponse => return Ok(self.stall(stream)),
            Behavior::ResetBeforeResponse => return Ok(reset(stream)),
            Behavior::ServiceUnavailable => return service_unavailable(stream, close),
            _ => {}
        }

        if behavior == Behavior::RangeNotSatisfiable || start >= content.len() {
            write!(
                stream,
                "HTTP/1.1 416 Range Not Satisfiable\r\n\
                 Content-Range: bytes */{}\r\n\
                 Content-Length: 0\r\n\
                 {close}\r\n",
                content.len()
            )?;
            return Ok(true);
        }

        if behavior == Behavior::IgnoreRange {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\n\
                 Content-Length: {}\r\n\
                 {close}\r\n",
                content.len()
            )?;
            stream.write_all(content)?;
            return Ok(true);
        }

        let end = end.min(content.len() - 1);
        let full = &content[start..=end];

        // `ShortBody` sends a well-formed response that is simply shorter than
        // was asked for -- the Content-Length matches what is actually sent.
        // That is precisely the case the `Blob` contract is about: the client
        // sees a successful response, and must not mistake a partly filled
        // buffer for success. A one-byte range cannot be shortened this way,
        // since an empty body has no `Content-Range` that describes it, so send
        // it in full.
        let body = if behavior == Behavior::ShortBody && full.len() > 1 {
            &full[..full.len() - 1]
        } else {
            full
        };

        write!(
            stream,
            "HTTP/1.1 206 Partial Content\r\n\
             Content-Range: bytes {}-{}/{}\r\n\
             Content-Length: {}\r\n\
             {close}\r\n",
            start,
            start + body.len() - 1,
            content.len(),
            body.len(),
        )?;

        // Unlike `ShortBody`, the Content-Length above promises the full body,
        // so sending less and then hanging up, or stalling, leaves the client
        // with a truncated message rather than a well-formed short one.
        let half = &body[..body.len() / 2];
        match behavior {
            Behavior::CloseMidBody => {
                stream.write_all(half)?;
                Ok(false)
            }
            Behavior::StallMidBody => {
                stream.write_all(half)?;
                Ok(self.stall(stream))
            }
            Behavior::ResetMidBody => {
                stream.write_all(half)?;
                Ok(reset(stream))
            }
            Behavior::SlowBody { pieces, interval } => {
                for piece in body.chunks(body.len().div_ceil(pieces.max(1))) {
                    std::thread::sleep(interval);
                    stream.write_all(piece)?;
                }
                Ok(true)
            }
            _ => {
                stream.write_all(body)?;
                Ok(true)
            }
        }
    }
}

/// Makes closing the connection send a reset rather than a FIN: with a zero
/// linger timeout, closing a socket discards any unsent data and aborts the
/// connection.
///
/// Returns false, since the connection cannot carry another request.
fn reset(stream: &mut TcpStream) -> bool {
    let _ = socket2::SockRef::from(&*stream).set_linger(Some(Duration::ZERO));
    false
}

/// Answers `503 Service Unavailable`. Returns whether the connection can carry
/// another request.
fn service_unavailable(stream: &mut TcpStream, close: &str) -> std::io::Result<bool> {
    write!(
        stream,
        "HTTP/1.1 503 Service Unavailable\r\n\
         Content-Length: 0\r\n\
         {close}\r\n"
    )?;
    Ok(true)
}

/// Reads up to and including the blank line that terminates the headers.
fn read_headers(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut request = Vec::new();
    let mut buf = [0; 512];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buf[..n]);
    }
    Ok(String::from_utf8_lossy(&request).into_owned())
}
