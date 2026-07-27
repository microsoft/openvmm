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
//! [`HttpBlob`]: disk_blob::blob::http::HttpBlob

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;

/// How the server should respond to range requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Answer with exactly the requested range.
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
}

/// A server bound to an ephemeral port on the loopback interface.
pub struct TestHttpServer {
    port: u16,
}

impl TestHttpServer {
    /// Starts a server publishing `content`.
    ///
    /// The serving thread runs until the process exits. Tests are short-lived
    /// and nextest gives each one its own process, so there is nothing to clean
    /// up.
    pub fn new(content: Vec<u8>, behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                // A misbehaving response deliberately drops the connection, so
                // errors here are expected and not worth reporting.
                let _ = serve(stream, &content, behavior);
            }
        });
        Self { port }
    }

    /// The URL of the blob.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/blob", self.port)
    }
}

/// Reads one request and writes one response, then closes the connection.
fn serve(mut stream: TcpStream, content: &[u8], behavior: Behavior) -> std::io::Result<()> {
    let request = read_headers(&mut stream)?;
    let mut lines = request.lines();
    let method = lines
        .next()
        .and_then(|l| l.split(' ').next())
        .unwrap_or_default()
        .to_owned();

    // `HttpBlob::new` probes the length with a HEAD request.
    if method.eq_ignore_ascii_case("HEAD") {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\n\
             Content-Length: {}\r\n\
             Accept-Ranges: bytes\r\n\
             Connection: close\r\n\r\n",
            content.len()
        )?;
        return Ok(());
    }

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
        return Ok(());
    };
    let (start, end) = range.split_once('-').unwrap();
    let start: usize = start.parse().unwrap();
    // The range is inclusive at both ends.
    let end: usize = end.parse().unwrap();

    if behavior == Behavior::RangeNotSatisfiable || start >= content.len() {
        write!(
            stream,
            "HTTP/1.1 416 Range Not Satisfiable\r\n\
             Content-Range: bytes */{}\r\n\
             Connection: close\r\n\r\n",
            content.len()
        )?;
        return Ok(());
    }

    if behavior == Behavior::IgnoreRange {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            content.len()
        )?;
        return stream.write_all(content);
    }

    let end = end.min(content.len() - 1);
    let full = &content[start..=end];

    // `ShortBody` sends a well-formed response that is simply shorter than was
    // asked for -- the Content-Length matches what is actually sent. That is
    // precisely the case the `Blob` contract is about: the client sees a
    // successful response, and must not mistake a partly filled buffer for
    // success.
    let body = if behavior == Behavior::ShortBody {
        &full[..full.len() - 1]
    } else {
        full
    };

    write!(
        stream,
        "HTTP/1.1 206 Partial Content\r\n\
         Content-Range: bytes {}-{}/{}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        start,
        start + body.len() - 1,
        content.len(),
        body.len(),
    )?;

    if behavior == Behavior::CloseMidBody {
        // Unlike `ShortBody`, the Content-Length above promises the full body.
        // Sending less and hanging up leaves the client with a truncated
        // message rather than a well-formed short one.
        stream.write_all(&body[..body.len() / 2])?;
        return Ok(());
    }

    stream.write_all(body)
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
