// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test support utilities: in-memory file backing store and I/O interceptors.

use crate::{AsyncFile, AsyncFileExt};
use parking_lot::Mutex;
use std::borrow::Borrow;
use std::sync::Arc;

/// Trait for intercepting I/O operations in tests.
///
/// Default implementations return `Ok(())` (no interception).
pub trait IoInterceptor: Send + Sync {
    /// Called before a read operation.
    fn before_read(&self, offset: u64, len: usize) -> Result<(), std::io::Error> {
        let _ = (offset, len);
        Ok(())
    }

    /// Called before a write operation.
    fn before_write(&self, offset: u64, data: &[u8]) -> Result<(), std::io::Error> {
        let _ = (offset, data);
        Ok(())
    }

    /// Called before a flush operation.
    fn before_flush(&self) -> Result<(), std::io::Error> {
        Ok(())
    }

    /// Called before a set_file_size operation.
    fn before_set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
        let _ = size;
        Ok(())
    }

    /// Returns `true` if the write should be silently discarded (data not
    /// written). The default is `false`.
    fn should_discard_write(&self, offset: u64, data: &[u8]) -> bool {
        let _ = (offset, data);
        false
    }
}

/// An interceptor that fails all I/O operations of specified types.
pub struct FailingInterceptor {
    /// Whether reads should fail.
    pub fail_reads: bool,
    /// Whether writes should fail.
    pub fail_writes: bool,
    /// Whether flushes should fail.
    pub fail_flushes: bool,
    /// Whether set_file_size should fail.
    pub fail_set_file_size: bool,
}

impl IoInterceptor for FailingInterceptor {
    fn before_read(&self, _offset: u64, _len: usize) -> Result<(), std::io::Error> {
        if self.fail_reads {
            return Err(std::io::Error::other("injected I/O failure"));
        }
        Ok(())
    }

    fn before_write(&self, _offset: u64, _data: &[u8]) -> Result<(), std::io::Error> {
        if self.fail_writes {
            return Err(std::io::Error::other("injected I/O failure"));
        }
        Ok(())
    }

    fn before_flush(&self) -> Result<(), std::io::Error> {
        if self.fail_flushes {
            return Err(std::io::Error::other("injected I/O failure"));
        }
        Ok(())
    }

    fn before_set_file_size(&self, _size: u64) -> Result<(), std::io::Error> {
        if self.fail_set_file_size {
            return Err(std::io::Error::other("injected I/O failure"));
        }
        Ok(())
    }
}

/// An interceptor that silently discards writes.
///
/// Reads and flushes pass through normally. Writes appear to succeed
/// but the underlying data is not modified. This simulates a crash
/// where writes were in flight but not persisted.
pub struct DiscardWritesInterceptor;

impl IoInterceptor for DiscardWritesInterceptor {
    fn should_discard_write(&self, _offset: u64, _data: &[u8]) -> bool {
        true
    }
}

/// In-memory file backing store for tests.
///
/// Supports optional I/O interception for failure injection and write
/// discarding (used in crash tests).
pub struct InMemoryFile {
    inner: Mutex<InMemoryFileInner>,
    interceptor: Option<Arc<dyn IoInterceptor>>,
}

struct InMemoryFileInner {
    data: Vec<u8>,
}

impl InMemoryFile {
    /// Creates a zero-filled file of the given size.
    pub fn new(size: u64) -> Self {
        Self {
            inner: Mutex::new(InMemoryFileInner {
                data: vec![0u8; size as usize],
            }),
            interceptor: None,
        }
    }

    /// Creates a zero-filled file with an I/O interceptor.
    pub fn with_interceptor(size: u64, interceptor: Arc<dyn IoInterceptor>) -> Self {
        Self {
            inner: Mutex::new(InMemoryFileInner {
                data: vec![0u8; size as usize],
            }),
            interceptor: Some(interceptor),
        }
    }

    /// Returns a clone of the current file contents.
    pub fn snapshot(&self) -> Vec<u8> {
        self.inner.lock().data.clone()
    }

    /// Create an `InMemoryFile` from existing data (e.g. a snapshot).
    pub fn from_snapshot(data: Vec<u8>) -> InMemoryFile {
        InMemoryFile {
            inner: Mutex::new(InMemoryFileInner { data }),
            interceptor: None,
        }
    }

    /// Create a VHDX file in memory with the given disk size and default parameters.
    ///
    /// Returns the `InMemoryFile` and the validated `CreateParams`.
    pub async fn create_test_vhdx(disk_size: u64) -> (InMemoryFile, crate::create::CreateParams) {
        let file = InMemoryFile::new(0);
        let mut params = crate::create::CreateParams {
            disk_size,
            ..Default::default()
        };
        crate::create::create(&file, &mut params).await.unwrap();
        (file, params)
    }
}

impl AsyncFile for InMemoryFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    async fn read_into(&self, offset: u64, mut buf: Vec<u8>) -> Result<Vec<u8>, std::io::Error> {
        if let Some(interceptor) = &self.interceptor {
            interceptor.before_read(offset, buf.len())?;
        }
        let inner = self.inner.lock();
        let offset = offset as usize;
        let file_len = inner.data.len();
        for (i, byte) in buf.iter_mut().enumerate() {
            let pos = offset + i;
            *byte = if pos < file_len { inner.data[pos] } else { 0 };
        }
        Ok(buf)
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> Result<(), std::io::Error> {
        let buf = buf.borrow();
        if let Some(interceptor) = &self.interceptor {
            interceptor.before_write(offset, buf.as_ref())?;
            if interceptor.should_discard_write(offset, buf.as_ref()) {
                return Ok(());
            }
        }
        let mut inner = self.inner.lock();
        let offset = offset as usize;
        let end = offset + buf.len();
        if end > inner.data.len() {
            inner.data.resize(end, 0);
        }
        inner.data[offset..end].copy_from_slice(buf.as_ref());
        Ok(())
    }

    async fn flush(&self) -> Result<(), std::io::Error> {
        if let Some(interceptor) = &self.interceptor {
            interceptor.before_flush()?;
        }
        Ok(())
    }

    async fn file_size(&self) -> Result<u64, std::io::Error> {
        Ok(self.inner.lock().data.len() as u64)
    }

    async fn set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
        if let Some(interceptor) = &self.interceptor {
            interceptor.before_set_file_size(size)?;
        }
        let mut inner = self.inner.lock();
        inner.data.resize(size as usize, 0);
        Ok(())
    }
}

/// A file implementation that separates volatile and durable state,
/// with a write log for verifying operation ordering.
///
/// - `write_at()` → writes to volatile only (reads see it, but it won't
///   survive a crash).
/// - `flush()` → copies volatile to durable (survives crash).
/// - `crash()` → returns durable state; volatile-only writes are lost.
/// - `from_durable(data)` → creates a new file from a crash snapshot.
///
/// The write log records every `write_at`, `flush`, and `set_file_size`
/// call, enabling ordering tests that verify flush barriers exist between
/// data writes and WAL writes.
pub struct CrashTestFile {
    inner: Mutex<CrashTestFileInner>,
}

impl std::fmt::Debug for CrashTestFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("CrashTestFile")
            .field("durable_len", &inner.durable.len())
            .field("volatile_len", &inner.volatile.len())
            .field("flush_count", &inner.flush_count)
            .finish()
    }
}

struct CrashTestFileInner {
    /// Data that has survived flush — survives power failure.
    durable: Vec<u8>,
    /// Data as seen by reads — includes unflushed writes.
    volatile: Vec<u8>,
    /// How many flush() calls have occurred.
    flush_count: u64,
}

impl CrashTestFile {
    /// Create a CrashTestFile from existing durable data (e.g. from a crash snapshot).
    pub fn from_durable(data: Vec<u8>) -> Self {
        Self {
            inner: Mutex::new(CrashTestFileInner {
                volatile: data.clone(),
                durable: data,
                flush_count: 0,
            }),
        }
    }

    /// Snapshot durable state without consuming the file.
    pub fn durable_snapshot(&self) -> Vec<u8> {
        self.inner.lock().durable.clone()
    }

    /// How many flushes have occurred.
    pub fn flush_count(&self) -> u64 {
        self.inner.lock().flush_count
    }
}

/// A crash-test file that yields during `write_at` and/or `flush`,
/// allowing other tasks to interleave.
///
/// This combines `CrashTestFile`'s durable/volatile split with
/// `YieldingFile`'s yield-point mechanism. When a yield is configured,
/// the file yields (returns Pending once) at the start of the operation,
/// allowing other spawned tasks to run. This creates genuine interleaving
/// between the log task, apply task, and user write tasks.
///
/// # Use cases
///
/// - **`yield_on_write = true`**: The apply task yields before each
///   `write_at`, allowing the log task to process another commit. This
///   creates a crash point where one batch's applies are in progress
///   while another batch is being logged.
///
/// - **`yield_on_flush = true`**: The flush path yields, allowing
///   concurrent writes to reach the log task before the flush completes.
pub struct YieldingCrashFile {
    inner: Mutex<CrashTestFileYieldInner>,
}

struct CrashTestFileYieldInner {
    durable: Vec<u8>,
    volatile: Vec<u8>,
    flush_count: u64,
    yield_on_write: bool,
    yield_on_flush: bool,
}

impl YieldingCrashFile {
    /// Create a `YieldingCrashFile` from existing durable data.
    pub fn from_durable(data: Vec<u8>, yield_on_write: bool, yield_on_flush: bool) -> Self {
        Self {
            inner: Mutex::new(CrashTestFileYieldInner {
                volatile: data.clone(),
                durable: data,
                flush_count: 0,
                yield_on_write,
                yield_on_flush,
            }),
        }
    }

    /// Snapshot durable state without consuming the file.
    pub fn durable_snapshot(&self) -> Vec<u8> {
        self.inner.lock().durable.clone()
    }
}

/// A crash-test file where the crash point is armed dynamically.
///
/// Before arming, the file behaves like a normal `CrashTestFile`: writes
/// go to volatile, flush copies volatile→durable.
///
/// After [`arm(n)`](Self::arm) is called, the file will allow exactly `n`
/// more flushes to succeed (making data durable), then start failing all
/// writes and flushes with I/O errors. The durable state is frozen at
/// the last successful flush.
///
/// # Typical usage
///
/// ```ignore
/// // Create and open writable (flushes during open are unaffected).
/// let file = CrashAfterFlushFile::new(snapshot);
/// let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
///
/// // Do some writes.
/// write_block(&vhdx, 0, bs, 0xAA).await;
///
/// // Arm: allow 1 more flush (the WAL flush), then crash.
/// vhdx.file.arm(1);
///
/// // This flush will: commit → log task writes WAL → flush_sequencer
/// // calls file.flush() (succeeds, armed count decrements to 0) →
/// // apply task tries to write → I/O error → file poisoned.
/// let _ = vhdx.flush().await; // may fail if apply races
/// ```
pub struct CrashAfterFlushFile {
    inner: Mutex<CrashAfterFlushInner>,
}

struct CrashAfterFlushInner {
    /// Data that has survived flush — survives power failure.
    durable: Vec<u8>,
    /// Data as seen by reads — includes unflushed writes.
    volatile: Vec<u8>,
    /// How many flushes have occurred.
    flush_count: u64,
    /// When Some(n), allow n more flushes then crash. None = not armed.
    remaining_flushes: Option<u64>,
    /// Whether the crash has been triggered.
    crashed: bool,
}

impl CrashAfterFlushFile {
    /// Create a new crash-armed file from existing data.
    /// The file starts unarmed; call [`arm()`](Self::arm) to set the crash point.
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            inner: Mutex::new(CrashAfterFlushInner {
                volatile: data.clone(),
                durable: data,
                flush_count: 0,
                remaining_flushes: None,
                crashed: false,
            }),
        }
    }

    /// Arm the crash: allow `n` more successful flushes, then fail.
    ///
    /// - `arm(0)` — the next flush fails immediately.
    /// - `arm(1)` — the next flush succeeds (makes data durable), then
    ///   the one after that fails.
    pub fn arm(&self, remaining_flushes: u64) {
        let mut inner = self.inner.lock();
        inner.remaining_flushes = Some(remaining_flushes);
    }

    /// Snapshot durable state without consuming the file.
    pub fn durable_snapshot(&self) -> Vec<u8> {
        self.inner.lock().durable.clone()
    }
}

impl AsyncFile for CrashAfterFlushFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    async fn read_into(&self, offset: u64, mut buf: Vec<u8>) -> Result<Vec<u8>, std::io::Error> {
        let inner = self.inner.lock();
        let offset = offset as usize;
        let file_len = inner.volatile.len();
        for (i, byte) in buf.iter_mut().enumerate() {
            let pos = offset + i;
            *byte = if pos < file_len {
                inner.volatile[pos]
            } else {
                0
            };
        }
        Ok(buf)
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> Result<(), std::io::Error> {
        let buf = buf.borrow();
        let mut inner = self.inner.lock();
        if inner.crashed {
            return Err(std::io::Error::other("crash: disk unavailable"));
        }
        let off = offset as usize;
        let end = off + buf.len();
        if end > inner.volatile.len() {
            inner.volatile.resize(end, 0);
        }
        inner.volatile[off..end].copy_from_slice(buf.as_ref());
        Ok(())
    }

    async fn flush(&self) -> Result<(), std::io::Error> {
        let mut inner = self.inner.lock();
        if inner.crashed {
            return Err(std::io::Error::other("crash: disk unavailable"));
        }
        // Check if armed and out of remaining flushes.
        if let Some(ref remaining) = inner.remaining_flushes {
            if *remaining == 0 {
                // Crash NOW — don't make data durable, fail the flush.
                inner.crashed = true;
                return Err(std::io::Error::other("crash: disk unavailable"));
            }
        }
        // Make data durable.
        inner.durable = inner.volatile.clone();
        inner.flush_count += 1;
        // Decrement remaining flushes.
        if let Some(ref mut remaining) = inner.remaining_flushes {
            *remaining -= 1;
        }
        Ok(())
    }

    async fn file_size(&self) -> Result<u64, std::io::Error> {
        Ok(self.inner.lock().volatile.len() as u64)
    }

    async fn set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
        let mut inner = self.inner.lock();
        if inner.crashed {
            return Err(std::io::Error::other("crash: disk unavailable"));
        }
        inner.volatile.resize(size as usize, 0);
        inner.durable.resize(size as usize, 0);
        Ok(())
    }
}

/// Yield once to allow other tasks to run, then resume.
async fn yield_once() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if !yielded {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
}

impl AsyncFile for YieldingCrashFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    async fn read_into(&self, offset: u64, mut buf: Vec<u8>) -> Result<Vec<u8>, std::io::Error> {
        let inner = self.inner.lock();
        let offset = offset as usize;
        let file_len = inner.volatile.len();
        for (i, byte) in buf.iter_mut().enumerate() {
            let pos = offset + i;
            *byte = if pos < file_len {
                inner.volatile[pos]
            } else {
                0
            };
        }
        Ok(buf)
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> Result<(), std::io::Error> {
        let should_yield = self.inner.lock().yield_on_write;
        if should_yield {
            yield_once().await;
        }
        let buf = buf.borrow();
        let mut inner = self.inner.lock();
        let off = offset as usize;
        let end = off + buf.len();
        if end > inner.volatile.len() {
            inner.volatile.resize(end, 0);
        }
        inner.volatile[off..end].copy_from_slice(buf.as_ref());
        Ok(())
    }

    async fn flush(&self) -> Result<(), std::io::Error> {
        let should_yield = self.inner.lock().yield_on_flush;
        if should_yield {
            yield_once().await;
        }

        let mut inner = self.inner.lock();
        inner.durable = inner.volatile.clone();
        inner.flush_count += 1;
        Ok(())
    }

    async fn file_size(&self) -> Result<u64, std::io::Error> {
        Ok(self.inner.lock().volatile.len() as u64)
    }

    async fn set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
        let mut inner = self.inner.lock();
        inner.volatile.resize(size as usize, 0);
        inner.durable.resize(size as usize, 0);
        Ok(())
    }
}

impl AsyncFile for CrashTestFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    async fn read_into(&self, offset: u64, mut buf: Vec<u8>) -> Result<Vec<u8>, std::io::Error> {
        let inner = self.inner.lock();
        let offset = offset as usize;
        let file_len = inner.volatile.len();
        for (i, byte) in buf.iter_mut().enumerate() {
            let pos = offset + i;
            *byte = if pos < file_len {
                inner.volatile[pos]
            } else {
                0
            };
        }
        Ok(buf)
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> Result<(), std::io::Error> {
        let buf = buf.borrow();
        let mut inner = self.inner.lock();
        let off = offset as usize;
        let end = off + buf.len();
        if end > inner.volatile.len() {
            inner.volatile.resize(end, 0);
        }
        inner.volatile[off..end].copy_from_slice(buf.as_ref());
        Ok(())
    }

    async fn flush(&self) -> Result<(), std::io::Error> {
        let mut inner = self.inner.lock();
        // Copy volatile to durable (all unflushed writes become durable).
        inner.durable = inner.volatile.clone();
        inner.flush_count += 1;
        Ok(())
    }

    async fn file_size(&self) -> Result<u64, std::io::Error> {
        // Return volatile size (latest state as seen by reads).
        Ok(self.inner.lock().volatile.len() as u64)
    }

    async fn set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
        let mut inner = self.inner.lock();
        // File size changes are immediately durable (metadata is sync).
        inner.volatile.resize(size as usize, 0);
        inner.durable.resize(size as usize, 0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::async_test;

    #[async_test]
    async fn write_then_read() {
        let file = InMemoryFile::new(1024);
        let data = b"hello, vhdx!";
        file.write_at(100, data).await.unwrap();

        let mut buf = vec![0u8; data.len()];
        file.read_at(100, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[async_test]
    async fn read_zeros_on_new_file() {
        let file = InMemoryFile::new(256);
        let mut buf = vec![0xFFu8; 256];
        file.read_at(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[async_test]
    async fn read_beyond_eof_zero_fills() {
        let file = InMemoryFile::new(8);
        // Write known data to the entire file.
        file.write_at(0, &[1, 2, 3, 4, 5, 6, 7, 8]).await.unwrap();

        // Read a range that extends 4 bytes past EOF.
        let mut buf = vec![0xFFu8; 12];
        file.read_at(0, &mut buf).await.unwrap();
        assert_eq!(&buf[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&buf[8..], &[0, 0, 0, 0]);

        // Read entirely beyond EOF.
        let mut buf2 = vec![0xFFu8; 4];
        file.read_at(100, &mut buf2).await.unwrap();
        assert!(buf2.iter().all(|&b| b == 0));
    }

    #[async_test]
    async fn write_beyond_eof_grows() {
        let file = InMemoryFile::new(4);
        assert_eq!(file.file_size().await.unwrap(), 4);

        file.write_at(8, b"hi").await.unwrap();
        assert_eq!(file.file_size().await.unwrap(), 10);

        // Gap between old EOF (4) and write offset (8) should be zeros.
        let mut gap = vec![0xFFu8; 4];
        file.read_at(4, &mut gap).await.unwrap();
        assert!(gap.iter().all(|&b| b == 0));

        // Written data should be present.
        let mut buf = vec![0u8; 2];
        file.read_at(8, &mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[async_test]
    async fn set_file_size_grow() {
        let file = InMemoryFile::new(4);
        file.write_at(0, &[1, 2, 3, 4]).await.unwrap();

        file.set_file_size(8).await.unwrap();
        assert_eq!(file.file_size().await.unwrap(), 8);

        let mut buf = vec![0xFFu8; 8];
        file.read_at(0, &mut buf).await.unwrap();
        assert_eq!(&buf, &[1, 2, 3, 4, 0, 0, 0, 0]);
    }

    #[async_test]
    async fn set_file_size_shrink() {
        let file = InMemoryFile::new(8);
        file.write_at(0, &[1, 2, 3, 4, 5, 6, 7, 8]).await.unwrap();

        file.set_file_size(4).await.unwrap();
        assert_eq!(file.file_size().await.unwrap(), 4);

        let snapshot = file.snapshot();
        assert_eq!(&snapshot, &[1, 2, 3, 4]);
    }

    #[async_test]
    async fn file_size_reports_correctly() {
        let file = InMemoryFile::new(100);
        assert_eq!(file.file_size().await.unwrap(), 100);

        file.set_file_size(200).await.unwrap();
        assert_eq!(file.file_size().await.unwrap(), 200);

        file.set_file_size(50).await.unwrap();
        assert_eq!(file.file_size().await.unwrap(), 50);
    }

    #[async_test]
    async fn snapshot_returns_copy() {
        let file = InMemoryFile::new(4);
        file.write_at(0, &[1, 2, 3, 4]).await.unwrap();

        let snap = file.snapshot();
        assert_eq!(&snap, &[1, 2, 3, 4]);

        // Subsequent write should not affect the snapshot.
        file.write_at(0, &[9, 9, 9, 9]).await.unwrap();
        assert_eq!(&snap, &[1, 2, 3, 4]);
    }

    #[async_test]
    async fn failing_interceptor_read() {
        let file = InMemoryFile::with_interceptor(
            64,
            Arc::new(FailingInterceptor {
                fail_reads: true,
                fail_writes: false,
                fail_flushes: false,
                fail_set_file_size: false,
            }),
        );

        let mut buf = vec![0u8; 8];
        let result = file.read_at(0, &mut buf).await;
        assert!(result.is_err());
    }

    #[async_test]
    async fn failing_interceptor_write() {
        let file = InMemoryFile::with_interceptor(
            64,
            Arc::new(FailingInterceptor {
                fail_reads: false,
                fail_writes: true,
                fail_flushes: false,
                fail_set_file_size: false,
            }),
        );

        let result = file.write_at(0, &[1, 2, 3, 4]).await;
        assert!(result.is_err());

        // File should not be modified.
        let snapshot = file.snapshot();
        assert!(snapshot.iter().all(|&b| b == 0));
    }

    #[async_test]
    async fn failing_interceptor_flush() {
        let file = InMemoryFile::with_interceptor(
            64,
            Arc::new(FailingInterceptor {
                fail_reads: false,
                fail_writes: false,
                fail_flushes: true,
                fail_set_file_size: false,
            }),
        );

        let result = file.flush().await;
        assert!(result.is_err());
    }

    #[async_test]
    async fn discard_writes_interceptor() {
        let file = InMemoryFile::with_interceptor(8, Arc::new(DiscardWritesInterceptor));

        // Write should appear to succeed.
        file.write_at(0, &[1, 2, 3, 4]).await.unwrap();

        // But the data should not actually be written.
        let mut buf = vec![0xFFu8; 4];
        file.read_at(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[async_test]
    async fn flush_is_noop() {
        let file = InMemoryFile::new(64);
        file.flush().await.unwrap();
    }
}
