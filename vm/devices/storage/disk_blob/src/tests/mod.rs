// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the blob disk backend.

mod http_server;

use crate::BlobDisk;
use crate::blob::Blob;
use crate::blob::file::FileBlob;
use crate::blob::http::HttpBlob;
use crate::blob::http::HttpBlobOptions;
use disk_backend::Disk;
use disk_backend::DiskError;
use guestmem::GuestMemory;
use http_server::Behavior;
use http_server::Script;
use http_server::TestHttpServer;
use pal_async::async_test;
use scsi_buffers::OwnedRequestBuffers;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;
use test_with_tracing::test;

const SECTOR_SIZE: usize = 512;
const DISK_SIZE: u64 = 1024 * 1024;

/// A file whose length is exactly the disk size, so that the backing object's
/// bounds coincide with the disk's.
fn raw_file() -> std::fs::File {
    let file = tempfile::tempfile().unwrap();
    file.set_len(DISK_SIZE).unwrap();
    file
}

/// A fixed VHD1 image: `DISK_SIZE` bytes of data followed by a 512-byte footer,
/// so the blob is strictly larger than the disk it presents.
fn fixed_vhd1_file() -> std::fs::File {
    let file = raw_file();
    disk_vhd1::Vhd1Disk::make_fixed(&file).unwrap();
    file
}

fn read_all(mut file: std::fs::File) -> Vec<u8> {
    let mut buf = Vec::new();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_to_end(&mut buf).unwrap();
    buf
}

#[async_test]
async fn sector_range_conformance() {
    let disk = Disk::new(BlobDisk::new(FileBlob::new(raw_file()).unwrap())).unwrap();
    storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
}

#[async_test]
async fn sector_range_conformance_fixed_vhd1() {
    let disk = Disk::new(
        BlobDisk::new_fixed_vhd1(FileBlob::new(fixed_vhd1_file()).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
}

/// Over HTTP, so that the `Blob` implementation under test is `HttpBlob` rather
/// than `FileBlob`. The two are not interchangeable: a defect in one says
/// nothing about the other.
#[async_test]
async fn sector_range_conformance_http() {
    let server = TestHttpServer::new(vec![0; DISK_SIZE as usize], Behavior::Correct);
    let disk = Disk::new(BlobDisk::new(HttpBlob::new(&server.url()).await.unwrap())).unwrap();
    storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
}

#[async_test]
async fn sector_range_conformance_http_fixed_vhd1() {
    let server = TestHttpServer::new(read_all(fixed_vhd1_file()), Behavior::Correct);
    let disk = Disk::new(
        BlobDisk::new_fixed_vhd1(HttpBlob::new(&server.url()).await.unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
}

/// A blob larger than the disk it presents is the case where delegating the
/// range check to the backing object is not sufficient: a read one sector past
/// the end lands in the VHD footer and succeeds, returning data that is not
/// part of the disk at all.
#[async_test]
async fn read_past_end_does_not_return_footer() {
    let disk = Disk::new(
        BlobDisk::new_fixed_vhd1(FileBlob::new(fixed_vhd1_file()).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    let mem = GuestMemory::allocate(SECTOR_SIZE);
    let r = disk
        .read_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_SIZE, true).buffer(&mem),
            disk.sector_count(),
        )
        .await;

    let mut buf = [0; SECTOR_SIZE];
    mem.read_at(0, &mut buf).unwrap();
    assert_ne!(&buf[..8], b"conectix", "read returned the VHD footer");
    assert!(matches!(r, Err(DiskError::IllegalBlock)), "{r:?}");
}

/// `BlobDisk` cannot tell a short read from a successful one by itself, so it
/// relies on the `Blob` contract requiring `UnexpectedEof` rather than partial
/// success. A misbehaving server must therefore surface as an error and never
/// as a read that quietly returns less, or the wrong, data.
///
/// These are responses a well-behaved server would not produce, which is why
/// they are worth testing: they are the cases the contract exists for.
#[async_test]
async fn misbehaving_server_never_reports_success() {
    let content = vec![0xab; DISK_SIZE as usize];
    for behavior in [
        Behavior::ShortBody,
        Behavior::IgnoreRange,
        Behavior::RangeNotSatisfiable,
        Behavior::CloseMidBody,
    ] {
        let server = TestHttpServer::new(content.clone(), behavior);
        let disk = Disk::new(BlobDisk::new(HttpBlob::new(&server.url()).await.unwrap())).unwrap();
        let mem = GuestMemory::allocate(SECTOR_SIZE);
        let r = disk
            .read_vectored(
                &OwnedRequestBuffers::linear(0, SECTOR_SIZE, true).buffer(&mem),
                0,
            )
            .await;

        match behavior {
            // The `Blob` documentation is specific about this one: a buffer
            // that is not completely filled must be reported as
            // `UnexpectedEof`, not as any kind of partial success. Assert the
            // documented error rather than merely that something went wrong,
            // since a short read succeeding is the failure mode that matters.
            Behavior::ShortBody => match r {
                Err(DiskError::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {}
                r => panic!("{behavior:?}: expected UnexpectedEof, got {r:?}"),
            },
            _ => assert!(r.is_err(), "{behavior:?}: reported success"),
        }
    }
}

/// How long a fault test waits for a request. One still pending by then is
/// taken to be hung for good, which is the failure these tests exist to catch.
const LIMIT: Duration = Duration::from_secs(10);

/// Content in which every offset is distinguishable from its neighbors, so that
/// data from the wrong place does not pass for the right data.
fn content() -> Vec<u8> {
    (0..DISK_SIZE).map(|i| (i % 251) as u8).collect()
}

/// Timeouts short enough that a stall is detected in half a second, so that the
/// fault tests are quick, but still far longer than a loopback request takes.
fn fast_options() -> HttpBlobOptions {
    HttpBlobOptions {
        response_timeout: Duration::from_millis(500),
        body_idle_timeout: Duration::from_millis(500),
        operation_timeout: Duration::from_secs(5),
        max_attempts: 3,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(40),
        ..Default::default()
    }
}

/// A future running on a thread of its own. See [`background`].
struct Background<T> {
    recv: mpsc::Receiver<T>,
    thread: JoinHandle<()>,
}

/// Runs `fut` on a thread of its own.
///
/// The test's thread then only waits for the output, for at most [`LIMIT`], so
/// that a request that never completes fails the test rather than hanging the
/// test runner.
fn background<T: 'static + Send>(fut: impl 'static + Send + Future<Output = T>) -> Background<T> {
    let (send, recv) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let output = pal_async::DefaultPool::run_with(async move |_| fut.await);
        // The test may have stopped waiting.
        let _ = send.send(output);
    });
    Background { recv, thread }
}

impl<T> Background<T> {
    /// Waits up to [`LIMIT`] for the output, panicking if there is none.
    fn wait(self, what: &str) -> T {
        match self.recv.recv_timeout(LIMIT) {
            Ok(output) => output,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("{what} did not complete within {LIMIT:?}")
            }
            // The thread panicked. Report that panic rather than this one.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::panic::resume_unwind(self.thread.join().unwrap_err())
            }
        }
    }
}

/// Runs `fut` to completion on a thread of its own, failing the test if that
/// takes longer than [`LIMIT`].
fn bounded<T: 'static + Send>(what: &str, fut: impl 'static + Send + Future<Output = T>) -> T {
    background(fut).wait(what)
}

/// Opens the blob that `server` publishes and reads `len` bytes at `offset`
/// from it, within [`LIMIT`].
fn read_blob(
    server: &TestHttpServer,
    options: HttpBlobOptions,
    offset: u64,
    len: usize,
) -> (HttpBlob, std::io::Result<Vec<u8>>) {
    let url = server.url();
    bounded("read", async move {
        let blob = HttpBlob::with_options(&url, options).await.unwrap();
        let mut buf = vec![0; len];
        let r = blob.read(&mut buf, offset).await.map(|()| buf);
        (blob, r)
    })
}

/// Returns the JSON for `path` in the blob's inspection output.
fn inspect_json(blob: &HttpBlob, path: &str) -> String {
    inspect::inspect(path, blob).results().json().to_string()
}

/// Reads through a server that fails the first GET requests as `failures` says,
/// and checks that the read recovers: one retry per failure, the right data,
/// and every stalled connection closed rather than leaked or reused.
fn check_recovers(failures: &[Behavior]) {
    let content = content();
    let server = TestHttpServer::with_script(
        content.clone(),
        Script {
            get: failures.to_vec(),
            ..Default::default()
        },
    );
    let (offset, len) = (12345, 64 * 1024);
    let (_blob, r) = read_blob(&server, fast_options(), offset, len);
    let data = r.unwrap_or_else(|err| panic!("{failures:?}: read failed: {err:?}"));
    assert!(
        data == content[offset as usize..][..len],
        "{failures:?}: read returned the wrong data"
    );
    assert_eq!(
        server.stats().gets,
        failures.len() + 1,
        "{failures:?}: wrong number of attempts"
    );
    server.wait_for("the stalled connections to be closed", LIMIT, |stats| {
        stats.abandoned == stats.stalled
    });
}

/// A server, or the network path to it, that goes silent before the response.
#[test]
fn stall_before_response_is_retried() {
    check_recovers(&[Behavior::StallBeforeResponse]);
}

/// A server, or the network path to it, that goes silent part way through the
/// body.
#[test]
fn stall_mid_body_is_retried() {
    check_recovers(&[Behavior::StallMidBody]);
}

#[test]
fn reset_before_response_is_retried() {
    check_recovers(&[Behavior::ResetBeforeResponse]);
}

#[test]
fn reset_mid_body_is_retried() {
    check_recovers(&[Behavior::ResetMidBody]);
}

#[test]
fn close_mid_body_is_retried() {
    check_recovers(&[Behavior::CloseMidBody]);
}

#[test]
fn service_unavailable_is_retried() {
    check_recovers(&[Behavior::ServiceUnavailable]);
}

/// Every attempt is bounded and retried alike, up to the attempt limit.
#[test]
fn consecutive_failures_are_retried() {
    check_recovers(&[Behavior::ResetBeforeResponse, Behavior::StallMidBody]);
}

/// A guest waits for its disk reads, typically without a timeout of its own,
/// so a read that never completes hangs the guest. When the server never
/// recovers, the read must fail instead, in bounded time.
#[test]
fn persistent_stall_fails_disk_read() {
    let server = TestHttpServer::new(content(), Behavior::StallMidBody);
    let url = server.url();
    let options = fast_options();
    let attempts = options.max_attempts as usize;
    let r = bounded("disk read", async move {
        let blob = HttpBlob::with_options(&url, options).await.unwrap();
        let disk = Disk::new(BlobDisk::new(blob)).unwrap();
        let mem = GuestMemory::allocate(SECTOR_SIZE * 8);
        disk.read_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_SIZE * 8, true).buffer(&mem),
            0,
        )
        .await
    });
    match r {
        Err(DiskError::Io(err)) if err.kind() == std::io::ErrorKind::TimedOut => {}
        r => panic!("expected a timeout, got {r:?}"),
    }
    assert_eq!(server.stats().gets, attempts);
    server.wait_for("the stalled connections to be closed", LIMIT, |stats| {
        stats.abandoned == attempts
    });
}

/// A response that keeps arriving is not a stall, however long it takes.
#[test]
fn slow_body_is_not_a_stall() {
    let content = content();
    let options = fast_options();
    // Each pause is well within the idle timeout, but the body as a whole takes
    // longer than either timeout.
    let interval = options.body_idle_timeout / 4;
    let server = TestHttpServer::new(
        content.clone(),
        Behavior::SlowBody {
            pieces: 8,
            interval,
        },
    );
    let start = Instant::now();
    let (_blob, r) = read_blob(&server, options.clone(), 0, 64 * 1024);
    let elapsed = start.elapsed();
    assert!(r.unwrap() == content[..64 * 1024], "wrong data");
    assert_eq!(server.stats().gets, 1, "the slow response was retried");
    assert!(
        elapsed > options.body_idle_timeout.max(options.response_timeout),
        "the response was not slow: {elapsed:?}"
    );
}

/// A response that keeps arriving, but too slowly ever to finish, is bounded by
/// the operation timeout instead.
#[test]
fn endless_body_is_bounded_by_operation_timeout() {
    let options = HttpBlobOptions {
        operation_timeout: Duration::from_millis(1500),
        ..fast_options()
    };
    // One byte at a time: the body would take minutes.
    let server = TestHttpServer::new(
        content(),
        Behavior::SlowBody {
            pieces: 4096,
            interval: options.body_idle_timeout / 10,
        },
    );
    let start = Instant::now();
    let (_blob, r) = read_blob(&server, options.clone(), 0, 4096);
    let elapsed = start.elapsed();
    match r {
        Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {}
        r => panic!("expected a timeout, got {:?}", r.map(|_| ())),
    }
    assert!(
        elapsed >= options.operation_timeout,
        "timed out early: {elapsed:?}"
    );
    assert_eq!(
        server.stats().gets,
        1,
        "retried after the operation timeout"
    );
}

/// Opening the blob probes it with a HEAD request, which must recover from a
/// stall too.
#[test]
fn stalled_head_is_retried() {
    let server = TestHttpServer::with_script(
        content(),
        Script {
            head: vec![Behavior::StallBeforeResponse],
            ..Default::default()
        },
    );
    let url = server.url();
    let blob = bounded("open", async move {
        HttpBlob::with_options(&url, fast_options()).await.unwrap()
    });
    assert_eq!(blob.len(), DISK_SIZE);
    assert_eq!(server.stats().heads, 2);
}

#[test]
fn persistent_head_stall_fails_open() {
    let options = fast_options();
    let attempts = options.max_attempts as usize;
    let server = TestHttpServer::with_script(
        content(),
        Script {
            head: vec![Behavior::StallBeforeResponse; attempts + 1],
            ..Default::default()
        },
    );
    let url = server.url();
    let r = bounded("open", async move {
        HttpBlob::with_options(&url, options).await.map(drop)
    });
    assert!(r.is_err(), "opened a blob whose server never answered");
    assert_eq!(server.stats().heads, attempts);
}

/// A connection whose request timed out must be closed, not returned to the
/// pool, where a later request would find it and stall in turn.
#[test]
fn stalled_connection_is_not_reused() {
    let content = content();
    let server = TestHttpServer::with_script(
        content.clone(),
        Script {
            get: vec![Behavior::Correct, Behavior::StallBeforeResponse],
            keep_alive: true,
            ..Default::default()
        },
    );
    let url = server.url();
    let (blob, results) = bounded("reads", async move {
        let blob = HttpBlob::with_options(&url, fast_options()).await.unwrap();
        let mut results = Vec::new();
        for i in 0..3 {
            let mut buf = vec![0; 4096];
            results.push(blob.read(&mut buf, i * 4096).await.map(|()| buf));
        }
        (blob, results)
    });
    for (i, r) in results.into_iter().enumerate() {
        let data = r.unwrap_or_else(|err| panic!("read {i} failed: {err:?}"));
        assert!(data == content[i * 4096..][..4096], "read {i}: wrong data");
    }
    // Only the stalled request was retried.
    assert_eq!(inspect_json(&blob, "retries"), "1");
    assert_eq!(inspect_json(&blob, "timeouts"), "1");
    assert_eq!(server.stats().gets, 4);
    server.wait_for("the stalled connection to be closed", LIMIT, |stats| {
        stats.abandoned == 1
    });
}

/// Inspection shows what a request that is taking long is waiting for, so that
/// a hung guest's disk can be told apart from a slow one.
#[test]
fn inspect_shows_stalled_request() {
    let content = content();
    let server = TestHttpServer::with_script(
        content.clone(),
        Script {
            get: vec![Behavior::StallBeforeResponse],
            ..Default::default()
        },
    );
    let url = server.url();
    // Long enough that the stall does not time out while it is inspected.
    // Releasing the server ends it instead.
    let options = HttpBlobOptions {
        response_timeout: LIMIT * 3,
        operation_timeout: LIMIT * 3,
        ..fast_options()
    };
    let blob = Arc::new(bounded("open", async move {
        HttpBlob::with_options(&url, options).await.unwrap()
    }));
    let read = background({
        let blob = blob.clone();
        async move {
            let mut buf = vec![0; 4096];
            blob.read(&mut buf, 8192).await.map(|()| buf)
        }
    });
    server.wait_for("the request to stall", LIMIT, |stats| stats.stalled == 1);
    let in_flight = inspect_json(&blob, "in_flight");
    for expected in [
        r#""request":"GET bytes=8192-12287""#,
        r#""attempt":1"#,
        r#""state":"awaiting_response""#,
    ] {
        assert!(
            in_flight.contains(expected),
            "{expected} not in {in_flight}"
        );
    }

    // Ending the stall fails the attempt, and the retry succeeds.
    server.release();
    let data = read.wait("read").unwrap();
    assert!(data == content[8192..][..4096], "wrong data");
    assert_eq!(inspect_json(&blob, "in_flight"), "{}");
    assert_eq!(inspect_json(&blob, "retries"), "1");
    assert_eq!(inspect_json(&blob, "timeouts"), "0");
}

/// A failure that a new attempt would only repeat is returned at once.
#[test]
fn permanent_errors_are_not_retried() {
    for behavior in [
        Behavior::ShortBody,
        Behavior::IgnoreRange,
        Behavior::RangeNotSatisfiable,
    ] {
        let server = TestHttpServer::new(content(), behavior);
        let (_blob, r) = read_blob(&server, fast_options(), 0, 4096);
        assert!(r.is_err(), "{behavior:?}: reported success");
        assert_eq!(server.stats().gets, 1, "{behavior:?}: retried");
    }
}

/// A range whose last byte would lie past the largest offset that a request
/// can name is refused, without a request, rather than overflowing.
#[test]
fn range_past_the_largest_offset_is_refused() {
    let server = TestHttpServer::new(content(), Behavior::Correct);
    let (_blob, r) = read_blob(&server, fast_options(), u64::MAX - 100, 4096);
    match r {
        Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => {}
        r => panic!("expected InvalidInput, got {:?}", r.map(|_| ())),
    }
    assert_eq!(server.stats().gets, 0);
}
