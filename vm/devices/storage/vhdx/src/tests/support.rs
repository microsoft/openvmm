// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test support utilities: in-memory file backing store and I/O interceptors.

use crate::{AsyncFile, AsyncFileExt};
use parking_lot::Mutex;
use std::borrow::Borrow;
use std::sync::Arc;

/// Trait for intercepting I/O operations in tests.
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

    /// Called before a set-file-size operation.
    fn before_set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
        let _ = size;
        Ok(())
    }

    /// Returns true if the write should be silently discarded.
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
    /// Whether set-file-size should fail.
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
pub struct DiscardWritesInterceptor;

impl IoInterceptor for DiscardWritesInterceptor {
    fn should_discard_write(&self, _offset: u64, _data: &[u8]) -> bool {
        true
    }
}

/// In-memory file backing store for tests.
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
                data: vec![0; size as usize],
            }),
            interceptor: None,
        }
    }

    /// Creates a zero-filled file with an I/O interceptor.
    pub fn with_interceptor(size: u64, interceptor: Arc<dyn IoInterceptor>) -> Self {
        Self {
            inner: Mutex::new(InMemoryFileInner {
                data: vec![0; size as usize],
            }),
            interceptor: Some(interceptor),
        }
    }

    /// Returns a clone of the current file contents.
    pub fn snapshot(&self) -> Vec<u8> {
        self.inner.lock().data.clone()
    }

    /// Creates an in-memory file from existing data.
    pub fn from_snapshot(data: Vec<u8>) -> Self {
        Self {
            inner: Mutex::new(InMemoryFileInner { data }),
            interceptor: None,
        }
    }

    /// Create a VHDX file in memory with the given disk size and default parameters.
    ///
    /// Returns the `InMemoryFile` and the validated `CreateParams`.
    pub async fn create_test_vhdx(disk_size: u64) -> (Self, crate::create::CreateParams) {
        let file = Self::new(0);
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
        vec![0; len]
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
        self.inner.lock().data.resize(size as usize, 0);
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

        let mut buf = vec![0; data.len()];
        file.read_at(100, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[async_test]
    async fn read_zeros_on_new_file() {
        let file = InMemoryFile::new(256);
        let mut buf = vec![0xff; 256];
        file.read_at(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&byte| byte == 0));
    }

    #[async_test]
    async fn read_beyond_eof_zero_fills() {
        let file = InMemoryFile::new(8);
        file.write_at(0, &[1, 2, 3, 4, 5, 6, 7, 8]).await.unwrap();

        let mut buf = vec![0xff; 12];
        file.read_at(0, &mut buf).await.unwrap();
        assert_eq!(&buf[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&buf[8..], &[0, 0, 0, 0]);

        let mut buf = vec![0xff; 4];
        file.read_at(100, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&byte| byte == 0));
    }

    #[async_test]
    async fn write_beyond_eof_grows() {
        let file = InMemoryFile::new(4);
        assert_eq!(file.file_size().await.unwrap(), 4);

        file.write_at(8, b"hi").await.unwrap();
        assert_eq!(file.file_size().await.unwrap(), 10);

        let mut gap = vec![0xff; 4];
        file.read_at(4, &mut gap).await.unwrap();
        assert!(gap.iter().all(|&byte| byte == 0));

        let mut buf = vec![0; 2];
        file.read_at(8, &mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[async_test]
    async fn set_file_size_grow() {
        let file = InMemoryFile::new(4);
        file.write_at(0, &[1, 2, 3, 4]).await.unwrap();

        file.set_file_size(8).await.unwrap();
        assert_eq!(file.file_size().await.unwrap(), 8);

        let mut buf = vec![0xff; 8];
        file.read_at(0, &mut buf).await.unwrap();
        assert_eq!(&buf, &[1, 2, 3, 4, 0, 0, 0, 0]);
    }

    #[async_test]
    async fn set_file_size_shrink() {
        let file = InMemoryFile::new(8);
        file.write_at(0, &[1, 2, 3, 4, 5, 6, 7, 8]).await.unwrap();

        file.set_file_size(4).await.unwrap();
        assert_eq!(file.file_size().await.unwrap(), 4);
        assert_eq!(&file.snapshot(), &[1, 2, 3, 4]);
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

        let snapshot = file.snapshot();
        assert_eq!(&snapshot, &[1, 2, 3, 4]);

        file.write_at(0, &[9, 9, 9, 9]).await.unwrap();
        assert_eq!(&snapshot, &[1, 2, 3, 4]);
    }

    #[async_test]
    async fn from_snapshot_copies_initial_data() {
        let file = InMemoryFile::from_snapshot(vec![1, 2, 3, 4]);

        let mut buf = vec![0; 4];
        file.read_at(0, &mut buf).await.unwrap();
        assert_eq!(&buf, &[1, 2, 3, 4]);
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

        let mut buf = vec![0; 8];
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
        assert!(file.snapshot().iter().all(|&byte| byte == 0));
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
    async fn failing_interceptor_set_file_size() {
        let file = InMemoryFile::with_interceptor(
            64,
            Arc::new(FailingInterceptor {
                fail_reads: false,
                fail_writes: false,
                fail_flushes: false,
                fail_set_file_size: true,
            }),
        );

        let result = file.set_file_size(128).await;
        assert!(result.is_err());
        assert_eq!(file.file_size().await.unwrap(), 64);
    }

    #[async_test]
    async fn discard_writes_interceptor() {
        let file = InMemoryFile::with_interceptor(8, Arc::new(DiscardWritesInterceptor));

        file.write_at(0, &[1, 2, 3, 4]).await.unwrap();

        let mut buf = vec![0xff; 4];
        file.read_at(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&byte| byte == 0));
    }

    #[async_test]
    async fn flush_is_noop() {
        let file = InMemoryFile::new(64);
        file.flush().await.unwrap();
    }
}
