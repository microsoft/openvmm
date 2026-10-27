// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HTTP blob implementation based on [`hyper`], [`tokio`], and
//! [`hyper_tls`].
//!
//! In the future, it may better to use `pal_async` instead. This will require a
//! new, unreleased version of `hyper`, and a bunch of infrastructure to support
//! initiating TCP connections the way `hyper` expects.

use super::Blob;
use anyhow::Context as _;
use async_trait::async_trait;
use http::uri::Scheme;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::Method;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::Uri;
use hyper::body::Incoming;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use inspect::Inspect;
use inspect_counters::SharedCounter;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Debug;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use thiserror::Error;
use tokio::time::Instant;

/// Timeouts and retries for the requests that an [`HttpBlob`] makes.
///
/// A guest waits for its disk reads to complete, typically without a timeout
/// of its own, so a request that never completes hangs the guest. And a request
/// can stop making progress without failing: when the network silently stops
/// delivering a connection's packets, the client is left waiting for data that
/// will never come, with no reset or FIN to end the wait. So every request is
/// bounded in time:
///
/// - Each attempt must receive the response headers within
///   [`response_timeout`](Self::response_timeout), and then more of the body at
///   least every [`body_idle_timeout`](Self::body_idle_timeout). These limit
///   silence rather than duration, so that a connection that has stopped is
///   detected without cutting off a slow response that is still arriving.
/// - An attempt that fails in a way that the next one might not -- by timing
///   out, by losing its connection, or with status 408, 429, 500, 502, 503, or
///   504 -- is retried on a new connection, after a delay that starts at
///   [`initial_backoff`](Self::initial_backoff) and doubles with each retry, up
///   to [`max_backoff`](Self::max_backoff), for up to
///   [`max_attempts`](Self::max_attempts) attempts in all. Any other failure is
///   returned at once.
/// - [`operation_timeout`](Self::operation_timeout) bounds the request as a
///   whole, so that even a response that trickles in too slowly ever to finish
///   fails in bounded time.
///
/// A request that fails returns an error, which the guest sees as a failed
/// read that it can handle, rather than as a read that never completes.
///
/// The defaults suit a blob store such as Azure Blob Storage, where a range
/// request normally completes in tens of milliseconds. A connection that is
/// silent for 20 seconds, a thousand times that, is taken to be dead; a request
/// whose every attempt stalls fails after about 80 seconds; and no request
/// takes more than two minutes. That rides out a transient network failure,
/// but still fails a hopeless request well before a typical test watchdog, such
/// as petri's ten minutes, would intervene.
#[derive(Debug, Clone)]
pub struct HttpBlobOptions {
    /// How long to wait for a TCP connection to be established.
    pub connect_timeout: Duration,
    /// How long an attempt waits for the response headers, including the time
    /// to connect and to send the request.
    pub response_timeout: Duration,
    /// How long an attempt waits for more of the response body.
    pub body_idle_timeout: Duration,
    /// How long a request may take in all, across all its attempts.
    pub operation_timeout: Duration,
    /// How many attempts to make at a request, including the first. At least
    /// one attempt is always made.
    pub max_attempts: u32,
    /// The delay before the first retry.
    pub initial_backoff: Duration,
    /// The longest delay before a retry.
    pub max_backoff: Duration,
}

impl Default for HttpBlobOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            response_timeout: Duration::from_secs(20),
            body_idle_timeout: Duration::from_secs(20),
            operation_timeout: Duration::from_secs(120),
            max_attempts: 4,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(4),
        }
    }
}

/// How long a connection may be idle before TCP keepalive probes check that
/// the server can still be reached. Without them, an idle pooled connection
/// whose network path has died is discovered only when a request is sent on it,
/// at the cost of a response timeout; with them, it is closed after
/// [`TCP_KEEPALIVE_RETRIES`] unanswered probes. The probes also keep the
/// connection alive in middleboxes that drop idle flows.
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(30);
/// The interval between TCP keepalive probes that go unanswered.
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// How many TCP keepalive probes go unanswered before the connection is closed.
const TCP_KEEPALIVE_RETRIES: u32 = 3;

type HttpClient = Client<HttpsConnector<HttpConnector>, Empty<&'static [u8]>>;

/// A blob backed by an HTTP/HTTPS connection.
///
/// Its requests are bounded in time, and retried, as [`HttpBlobOptions`]
/// describes.
#[derive(Debug, Inspect)]
pub struct HttpBlob {
    #[inspect(flatten)]
    requester: Arc<Requester>,
    #[inspect(debug)]
    version: http::Version,
    #[inspect(display)]
    uri: Uri,
    len: u64,
}

static TOKIO_RUNTIME: OnceCell<tokio::runtime::Runtime> = OnceCell::new();

impl HttpBlob {
    /// Connects to `url` and returns an object to access it as a blob, with the
    /// default [`HttpBlobOptions`].
    pub async fn new(url: &str) -> anyhow::Result<Self> {
        Self::with_options(url, HttpBlobOptions::default()).await
    }

    /// Connects to `url` and returns an object to access it as a blob, with the
    /// timeouts and retries that `options` specifies.
    pub async fn with_options(url: &str, options: HttpBlobOptions) -> anyhow::Result<Self> {
        let uri: Uri = url.parse()?;

        let handle = TOKIO_RUNTIME
            .get_or_try_init(tokio::runtime::Runtime::new)
            .context("failed to initialize tokio")?
            .handle()
            .clone();

        let requester = Arc::new(Requester::new(options, handle));
        let (uri, response) = requester.run(requester.clone().probe(uri)).await?;

        let len = response
            .headers()
            .get("Content-Length")
            .context("missing blob length")?
            .to_str()
            .context("couldn't parse blob length")?
            .parse()
            .context("couldn't parse blob length")?;

        let version = response.version();

        Ok(Self {
            requester,
            version,
            uri,
            len,
        })
    }
}

#[async_trait]
impl Blob for HttpBlob {
    async fn read(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        // HTTP byte ranges are inclusive at both ends, so an empty one cannot
        // be expressed, and computing its end offset would underflow. There is
        // nothing to fetch in any case.
        if buf.is_empty() {
            return Ok(());
        }
        // A request names the range by its first and last offsets, so the
        // last must be representable.
        if offset.checked_add(buf.len() as u64 - 1).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read range ends past the largest representable offset",
            ));
        }
        let requester = self.requester.clone();
        let uri = self.uri.clone();
        let len = buf.len();
        let data = self
            .requester
            .run(async move { requester.get(uri, offset, len).await })
            .await?;
        buf.copy_from_slice(&data);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.len
    }
}

/// Makes the requests for an [`HttpBlob`], with timeouts and retries.
#[derive(Debug, Inspect)]
struct Requester {
    #[inspect(skip)]
    options: HttpBlobOptions,
    /// Makes first attempts, which may reuse idle connections.
    #[inspect(skip)]
    client: HttpClient,
    /// Makes retries, each on a new connection: whatever broke the previous
    /// attempt's connection may have broken the idle ones too.
    #[inspect(skip)]
    retry_client: HttpClient,
    #[inspect(skip)]
    tokio_handle: tokio::runtime::Handle,
    /// Attempts made, retries included.
    requests: SharedCounter,
    /// Attempts made after a failed attempt.
    retries: SharedCounter,
    /// Attempts cut short by a timeout.
    timeouts: SharedCounter,
    /// Requests that failed, after any retries.
    failures: SharedCounter,
    /// The requests in progress.
    in_flight: InFlightTable,
    #[inspect(skip)]
    next_id: AtomicU64,
}

impl Requester {
    fn new(options: HttpBlobOptions, tokio_handle: tokio::runtime::Handle) -> Self {
        let mut http = HttpConnector::new();
        // The HTTPS connector hands HTTPS URIs to this one to connect.
        http.enforce_http(false);
        http.set_connect_timeout(Some(options.connect_timeout));
        http.set_keepalive(Some(TCP_KEEPALIVE_TIME));
        http.set_keepalive_interval(Some(TCP_KEEPALIVE_INTERVAL));
        http.set_keepalive_retries(Some(TCP_KEEPALIVE_RETRIES));
        let connector = HttpsConnector::new_with_connector(http);
        Self {
            client: Client::builder(TokioExecutor::new()).build(connector.clone()),
            retry_client: Client::builder(TokioExecutor::new())
                .pool_max_idle_per_host(0)
                .build(connector),
            options,
            tokio_handle,
            requests: SharedCounter::new(),
            retries: SharedCounter::new(),
            timeouts: SharedCounter::new(),
            failures: SharedCounter::new(),
            in_flight: InFlightTable::default(),
            next_id: AtomicU64::new(0),
        }
    }

    /// Runs `fut` on the tokio runtime, whose timers and connections it needs,
    /// and returns its output. Dropping the returned future aborts `fut`.
    async fn run<T: 'static + Send>(&self, fut: impl 'static + Send + Future<Output = T>) -> T {
        let mut task = AbortOnDrop(self.tokio_handle.spawn(fut));
        match (&mut task.0).await {
            Ok(output) => output,
            // The task is only aborted when this future is dropped, so it must
            // have panicked.
            Err(err) => std::panic::resume_unwind(err.into_panic()),
        }
    }

    /// Follows `uri` through any redirects to the blob, and returns the blob's
    /// URI and the response to a HEAD request for it.
    async fn probe(self: Arc<Self>, mut uri: Uri) -> anyhow::Result<(Uri, Response<Incoming>)> {
        // One deadline covers all the redirects.
        let deadline = deadline_after(self.options.operation_timeout);
        let response_timeout = self.options.response_timeout;
        let mut redirect_count = 0;
        loop {
            if redirect_count > 5 {
                anyhow::bail!("too many redirects");
            }

            let response = self
                .request(RequestKind::Head, deadline, |client, _| {
                    head_once(client, uri.clone(), response_timeout)
                })
                .await
                .context("failed to query blob size")?;

            let next_uri: Uri = match response.status() {
                StatusCode::OK => return Ok((uri, response)),
                StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT => response
                    .headers()
                    .get("Location")
                    .context("missing redirect URL")?
                    .to_str()
                    .context("couldn't parse redirect URL")?
                    .parse()
                    .context("couldn't parse redirect URL")?,
                status => {
                    anyhow::bail!("failed to query blob size: {status}");
                }
            };

            if uri.scheme() == Some(&Scheme::HTTPS) && next_uri.scheme() != Some(&Scheme::HTTPS) {
                anyhow::bail!("https redirected to http");
            }

            uri = next_uri;
            redirect_count += 1;
        }
    }

    /// Reads `len` bytes at `offset` from the blob at `uri`.
    async fn get(self: Arc<Self>, uri: Uri, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let deadline = deadline_after(self.options.operation_timeout);
        let response_timeout = self.options.response_timeout;
        let body_idle_timeout = self.options.body_idle_timeout;
        self.request(
            RequestKind::Get { offset, len },
            deadline,
            |client, progress| {
                get_once(
                    client,
                    uri.clone(),
                    offset,
                    len,
                    response_timeout,
                    body_idle_timeout,
                    progress,
                )
            },
        )
        .await
    }

    /// Makes `request`, by calling `attempt` with the client to use and a
    /// handle to report progress through, and retrying it as the options
    /// say, until it succeeds, fails for good, or `deadline` passes.
    async fn request<T, F, Fut>(
        self: &Arc<Self>,
        request: RequestKind,
        deadline: Instant,
        mut attempt: F,
    ) -> io::Result<T>
    where
        F: FnMut(HttpClient, Progress) -> Fut,
        Fut: Future<Output = Result<T, Failure>>,
    {
        let tracker = self.track(request);
        let mut backoff = self.options.initial_backoff.min(self.options.max_backoff);
        let mut attempts = 0;
        loop {
            attempts += 1;
            tracker.0.update(|entry| entry.start_attempt(attempts));
            self.requests.increment();
            let client = if attempts == 1 {
                &self.client
            } else {
                &self.retry_client
            };
            let (err, retry) =
                match tokio::time::timeout_at(deadline, attempt(client.clone(), tracker.0.clone()))
                    .await
                {
                    Ok(Ok(output)) => return Ok(output),
                    Ok(Err(Failure::Transient(err))) => (err, true),
                    Ok(Err(Failure::Timeout(err))) => {
                        self.timeouts.increment();
                        (io::Error::new(io::ErrorKind::TimedOut, err), true)
                    }
                    Ok(Err(Failure::Permanent(err))) => {
                        self.failures.increment();
                        return Err(err);
                    }
                    Err(_) => {
                        self.timeouts.increment();
                        let err = RequestError::OperationTimeout {
                            request,
                            timeout: self.options.operation_timeout,
                        };
                        (io::Error::new(io::ErrorKind::TimedOut, err), false)
                    }
                };

            let next_attempt = Instant::now().checked_add(backoff);
            if retry
                && attempts < self.options.max_attempts
                && next_attempt.is_some_and(|next| next < deadline)
            {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    %request,
                    attempt = attempts,
                    ?backoff,
                    "http blob request failed, retrying"
                );
                self.retries.increment();
                tracker.0.update(|entry| entry.state = State::BackingOff);
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(self.options.max_backoff);
                continue;
            }

            self.failures.increment();
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                %request,
                attempts,
                "http blob request failed"
            );
            return Err(if retry && attempts > 1 {
                io::Error::new(
                    err.kind(),
                    RequestError::Exhausted {
                        request,
                        attempts,
                        source: err,
                    },
                )
            } else {
                err
            });
        }
    }

    /// Lists `request` in [`Requester::in_flight`] until the returned tracker
    /// is dropped.
    fn track(self: &Arc<Self>, request: RequestKind) -> Tracker {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.in_flight.0.lock().insert(
            id,
            InFlight {
                request,
                attempt: 0,
                state: State::AwaitingResponse,
                started: Instant::now(),
                received: 0,
            },
        );
        Tracker(Progress {
            requester: self.clone(),
            id,
        })
    }
}

/// Returns the instant `timeout` from now, or a distant one if that cannot be
/// represented.
fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout)
        .unwrap_or_else(|| now + Duration::from_secs(60 * 60 * 24 * 365))
}

/// Makes one attempt at a HEAD request for `uri`.
async fn head_once(
    client: HttpClient,
    uri: Uri,
    response_timeout: Duration,
) -> Result<Response<Incoming>, Failure> {
    let request = Request::builder()
        .uri(uri)
        .method(Method::HEAD)
        .body(Empty::new())
        .unwrap();
    send(client, request, response_timeout).await
}

/// Makes one attempt at reading `len` bytes at `offset` from the blob at `uri`.
async fn get_once(
    client: HttpClient,
    uri: Uri,
    offset: u64,
    len: usize,
    response_timeout: Duration,
    body_idle_timeout: Duration,
    progress: Progress,
) -> Result<Vec<u8>, Failure> {
    let request = Request::builder()
        .uri(uri)
        .header(
            hyper::header::RANGE,
            format!("bytes={}-{}", offset, offset + len as u64 - 1),
        )
        .body(Empty::new())
        .unwrap();
    let response = send(client, request, response_timeout).await?;

    if !response.status().is_success() {
        return Err(Failure::Permanent(io::Error::other(RequestError::Status(
            response.status(),
        ))));
    }

    progress.update(|entry| entry.state = State::ReceivingBody);
    let mut body = response.into_body();
    let mut data = Vec::with_capacity(len);
    loop {
        let frame = match tokio::time::timeout(body_idle_timeout, body.frame()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(None) => break,
            // The connection failed part way through the body.
            Ok(Some(Err(err))) => return Err(Failure::Transient(io::Error::other(err))),
            Err(_) => {
                return Err(Failure::Timeout(RequestError::BodyTimeout {
                    timeout: body_idle_timeout,
                    received: data.len(),
                    expected: len,
                }));
            }
        };
        if let Some(chunk) = frame.data_ref() {
            if chunk.len() > len - data.len() {
                return Err(Failure::Permanent(io::Error::other(
                    "server did not respect range query",
                )));
            }
            data.extend_from_slice(chunk);
            progress.update(|entry| entry.received = data.len());
        }
    }

    if data.len() < len {
        // A well-formed response that is shorter than was asked for. Another
        // attempt would get the same.
        return Err(Failure::Permanent(io::ErrorKind::UnexpectedEof.into()));
    }

    Ok(data)
}

/// Sends `request`, and waits up to `response_timeout` for the response
/// headers.
async fn send(
    client: HttpClient,
    request: Request<Empty<&'static [u8]>>,
    response_timeout: Duration,
) -> Result<Response<Incoming>, Failure> {
    let response = match tokio::time::timeout(response_timeout, client.request(request)).await {
        Ok(Ok(response)) => response,
        // The connector gave up on connecting.
        Ok(Err(err)) if is_timed_out(&err) => {
            return Err(Failure::Timeout(RequestError::ConnectTimeout(err)));
        }
        // The request is well formed, so this is a failure to connect, or to
        // keep the connection.
        Ok(Err(err)) => return Err(Failure::Transient(io::Error::other(err))),
        Err(_) => {
            return Err(Failure::Timeout(RequestError::ResponseTimeout(
                response_timeout,
            )));
        }
    };

    let status = response.status();
    if matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) {
        return Err(Failure::Transient(io::Error::other(RequestError::Status(
            status,
        ))));
    }

    Ok(response)
}

/// Returns whether `err`, or any error in its chain of sources, is a timeout,
/// as when the connector gives up on connecting.
fn is_timed_out(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut next = Some(err);
    while let Some(err) = next {
        if err
            .downcast_ref::<io::Error>()
            .is_some_and(|err| err.kind() == io::ErrorKind::TimedOut)
        {
            return true;
        }
        next = err.source();
    }
    false
}

/// Aborts a tokio task when dropped.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Why an attempt failed, which decides whether to retry it.
enum Failure {
    /// The attempt failed in a way that the next attempt might not.
    Transient(io::Error),
    /// The attempt timed out, which the next attempt might not.
    Timeout(RequestError),
    /// The attempt failed in a way that the next attempt would repeat.
    Permanent(io::Error),
}

#[derive(Debug, Error)]
enum RequestError {
    #[error("connection timed out")]
    ConnectTimeout(#[source] hyper_util::client::legacy::Error),
    #[error("no response within {0:?}")]
    ResponseTimeout(Duration),
    #[error("no data for {timeout:?}, after {received} of {expected} bytes")]
    BodyTimeout {
        timeout: Duration,
        received: usize,
        expected: usize,
    },
    #[error("{request} did not complete within {timeout:?}")]
    OperationTimeout {
        request: RequestKind,
        timeout: Duration,
    },
    #[error("{request} failed after {attempts} attempts")]
    Exhausted {
        request: RequestKind,
        attempts: u32,
        #[source]
        source: io::Error,
    },
    #[error("server returned {0}")]
    Status(StatusCode),
}

/// A request that an [`HttpBlob`] makes.
#[derive(Debug, Copy, Clone)]
enum RequestKind {
    /// A HEAD request, to probe the blob's length.
    Head,
    /// A range request for `len` bytes at `offset`. `len` is not zero, and the
    /// offset of the range's last byte, `offset + len - 1`, does not overflow.
    Get { offset: u64, len: usize },
}

impl fmt::Display for RequestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Head => f.write_str("HEAD"),
            Self::Get { offset, len } => {
                write!(f, "GET bytes={}-{}", offset, offset + len as u64 - 1)
            }
        }
    }
}

/// The requests in progress, by ID.
#[derive(Debug, Default)]
struct InFlightTable(Mutex<BTreeMap<u64, InFlight>>);

impl Inspect for InFlightTable {
    fn inspect(&self, req: inspect::Request<'_>) {
        inspect::iter_by_key(self.0.lock().iter()).inspect(req);
    }
}

/// A request in progress, as inspection shows it.
#[derive(Debug, Inspect)]
struct InFlight {
    /// The request, such as `GET bytes=0-4095`.
    #[inspect(display)]
    request: RequestKind,
    /// The current attempt, counting from 1.
    attempt: u32,
    /// What the request is waiting for.
    state: State,
    /// How long ago the request started.
    #[inspect(rename = "elapsed", with = "Instant::elapsed")]
    started: Instant,
    /// How much of the response body the current attempt has received.
    received: usize,
}

impl InFlight {
    fn start_attempt(&mut self, attempt: u32) {
        self.attempt = attempt;
        self.state = State::AwaitingResponse;
        self.received = 0;
    }
}

/// What a request in progress is waiting for.
#[derive(Debug, Copy, Clone, Inspect)]
enum State {
    /// The response headers, connecting first if need be.
    AwaitingResponse,
    /// More of the response body.
    ReceivingBody,
    /// The delay before the next attempt.
    BackingOff,
}

/// A handle through which an attempt reports its progress to inspection.
#[derive(Clone)]
struct Progress {
    requester: Arc<Requester>,
    id: u64,
}

impl Progress {
    fn update(&self, f: impl FnOnce(&mut InFlight)) {
        if let Some(entry) = self.requester.in_flight.0.lock().get_mut(&self.id) {
            f(entry);
        }
    }
}

/// Removes a request from [`Requester::in_flight`] when dropped.
struct Tracker(Progress);

impl Drop for Tracker {
    fn drop(&mut self) {
        self.0.requester.in_flight.0.lock().remove(&self.0.id);
    }
}

#[cfg(test)]
mod tests {
    use super::is_timed_out;
    use std::io;
    use test_with_tracing::test;

    /// An error caused by another, as the connector's errors are.
    #[derive(Debug, thiserror::Error)]
    #[error("wrapper")]
    struct Wrapper(#[source] Box<dyn std::error::Error + Send + Sync>);

    #[test]
    fn a_timeout_is_found_anywhere_in_the_chain() {
        let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "connect");
        assert!(is_timed_out(&timed_out()));
        assert!(is_timed_out(&Wrapper(Box::new(Wrapper(Box::new(
            timed_out()
        ))))));
        assert!(!is_timed_out(&io::Error::other("refused")));
        assert!(!is_timed_out(&Wrapper(Box::new(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "refused",
        )))));
    }
}
