// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared host-file I/O for storage backends.

use blocking::unblock;
use disk_backend::DiskError;
use pal_async::driver::Driver;
use parking_lot::Mutex;
use scsi_buffers::RequestBuffers;
use std::borrow::Borrow;
use std::fs;
use std::io;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicUsize;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;

#[cfg(target_os = "linux")]
use io_uring::opcode;
#[cfg(target_os = "linux")]
use io_uring::types;
#[cfg(target_os = "linux")]
use scsi_buffers::BounceBuffer;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

/// Owned-buffer and file-management I/O for a shared host file.
#[derive(Clone)]
pub struct OwnedFile {
    file: Arc<fs::File>,
}

impl OwnedFile {
    /// Wraps an open file.
    pub fn new(file: fs::File) -> Self {
        Self {
            file: Arc::new(file),
        }
    }

    fn from_arc(file: Arc<fs::File>) -> Self {
        Self { file }
    }

    /// Reads exactly `buf.len()` bytes at `offset`.
    pub async fn read_into(&self, offset: u64, mut buf: Vec<u8>) -> io::Result<Vec<u8>> {
        let file = self.file.clone();
        unblock(move || {
            read_exact_at(&file, &mut buf, offset)?;
            Ok(buf)
        })
        .await
    }

    /// Writes all of `buf` at `offset`.
    pub async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> io::Result<()> {
        let file = self.file.clone();
        unblock(move || write_all_at(&file, buf.borrow(), offset)).await
    }

    /// Flushes file data and metadata to stable storage.
    pub async fn flush(&self) -> io::Result<()> {
        let file = self.file.clone();
        unblock(move || file.sync_all()).await
    }

    /// Returns the current file size.
    pub fn file_size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Changes the current file size.
    pub async fn set_file_size(&self, size: u64) -> io::Result<()> {
        let file = self.file.clone();
        unblock(move || file.set_len(size)).await
    }
}

/// Payload I/O scheduler for a shared host file.
pub struct HostFile {
    file: Arc<fs::File>,
    buffers: Mutex<Vec<Vec<u8>>>,
    #[cfg(target_os = "linux")]
    driver: Box<dyn Driver>,
    #[cfg(target_os = "linux")]
    io_uring_available: bool,
    #[cfg(target_os = "linux")]
    in_flight: AtomicUsize,
}

/// Payload I/O operations associated with one frontend request.
pub struct HostFileRequest<'a> {
    file: &'a HostFile,
    #[cfg(target_os = "linux")]
    use_io_uring: bool,
    #[cfg(target_os = "linux")]
    _in_flight: Option<InFlight<'a>>,
}

impl HostFile {
    /// Creates a host-file scheduler using the platform's async I/O engine.
    pub fn new(file: fs::File, driver: impl Driver) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let io_uring_available = driver.io_uring_probe(opcode::Read::CODE)
                && driver.io_uring_probe(opcode::Write::CODE);

            Ok(Self {
                file: Arc::new(file),
                buffers: Mutex::new(Vec::new()),
                driver: Box::new(driver),
                io_uring_available,
                in_flight: AtomicUsize::new(0),
            })
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = driver;
            Ok(Self {
                file: Arc::new(file),
                buffers: Mutex::new(Vec::new()),
            })
        }
    }

    /// Returns an owned-buffer interface for metadata and file management.
    pub fn owned_file(&self) -> OwnedFile {
        OwnedFile::from_arc(self.file.clone())
    }

    /// Starts payload I/O for a frontend request.
    pub fn request(&self, len: usize) -> HostFileRequest<'_> {
        #[cfg(target_os = "linux")]
        {
            const MAX_IO_SIZE: usize = 4 * 1024;
            if !self.io_uring_available || len > MAX_IO_SIZE {
                return HostFileRequest {
                    file: self,
                    use_io_uring: false,
                    _in_flight: None,
                };
            }

            let in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
            HostFileRequest {
                file: self,
                use_io_uring: use_io_uring(len, in_flight),
                _in_flight: Some(InFlight(&self.in_flight)),
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = len;
            HostFileRequest { file: self }
        }
    }

    /// Writes zeroes to a positioned file range.
    pub async fn zero_at(&self, file_offset: u64, len: u64) -> Result<(), DiskError> {
        self.zero_buffered_at(file_offset, len).await
    }

    fn take_buffer(&self, len: usize) -> Vec<u8> {
        let mut buffer = {
            let mut buffers = self.buffers.lock();
            buffers
                .iter()
                .position(|buffer| buffer.capacity() >= len)
                .map(|index| buffers.swap_remove(index))
                .or_else(|| buffers.pop())
                .unwrap_or_default()
        };
        buffer.resize(len, 0);
        buffer
    }

    fn return_buffer(&self, mut buffer: Vec<u8>) {
        const MAX_BUFFERS: usize = 64;
        const MAX_BUFFER_CAPACITY: usize = 1024 * 1024;

        if buffer.capacity() <= MAX_BUFFER_CAPACITY {
            buffer.clear();
            let mut buffers = self.buffers.lock();
            if buffers.len() < MAX_BUFFERS {
                buffers.push(buffer);
            }
        }
    }

    async fn zero_buffered_at(&self, mut file_offset: u64, mut len: u64) -> Result<(), DiskError> {
        const CHUNK_SIZE: usize = 64 * 1024;
        while len != 0 {
            let chunk_len = CHUNK_SIZE.min(len as usize);
            self.owned_file()
                .write_from(file_offset, vec![0; chunk_len])
                .await
                .map_err(DiskError::Io)?;
            file_offset += chunk_len as u64;
            len -= chunk_len as u64;
        }
        Ok(())
    }
}

impl HostFileRequest<'_> {
    /// Reads a positioned file range into request buffers.
    pub async fn read_at(
        &self,
        buffers: &RequestBuffers<'_>,
        file_offset: u64,
    ) -> Result<(), DiskError> {
        #[cfg(target_os = "linux")]
        if self.use_io_uring {
            let file = self.file.file.clone();
            let mut bounce = BounceBuffer::new(buffers.len());
            let len = u32::try_from(buffers.len()).map_err(|_| DiskError::InvalidInput)?;

            // SAFETY: The buffer and file remain owned by this async state
            // machine until completion, including if the future is leaked.
            let bytes_read = unsafe {
                self.file.driver.io_uring_submit(
                    opcode::Read::new(
                        types::Fd(file.as_raw_fd()),
                        bounce.as_mut_bytes().as_mut_ptr(),
                        len,
                    )
                    .offset(file_offset)
                    .build(),
                )
            }
            .await
            .map_err(DiskError::Io)?;

            check_io_size(bytes_read, buffers.len(), io::ErrorKind::UnexpectedEof)
                .map_err(DiskError::Io)?;
            use guestmem::MemoryWrite;
            buffers.writer().write(bounce.as_mut_bytes())?;
            return Ok(());
        }

        use guestmem::MemoryWrite;
        let file = self.file.file.clone();
        let mut buf = self.file.take_buffer(buffers.len());
        let (buf, result) = unblock(move || {
            let result = read_exact_at(&file, &mut buf, file_offset);
            (buf, result)
        })
        .await;
        let result = result.map_err(DiskError::Io);
        let write_result = result.and_then(|()| {
            buffers
                .writer()
                .write(&buf)
                .map_err(DiskError::MemoryAccess)
        });
        self.file.return_buffer(buf);
        write_result
    }

    /// Writes request buffers to a positioned file range.
    pub async fn write_at(
        &self,
        buffers: &RequestBuffers<'_>,
        file_offset: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        #[cfg(target_os = "linux")]
        if self.use_io_uring {
            use guestmem::MemoryRead;
            let file = self.file.file.clone();
            let mut bounce = BounceBuffer::new(buffers.len());
            buffers.reader().read(bounce.as_mut_bytes())?;
            let len = u32::try_from(buffers.len()).map_err(|_| DiskError::InvalidInput)?;

            // SAFETY: The buffer and file remain owned by this async state
            // machine until completion, including if the future is leaked.
            let bytes_written = unsafe {
                self.file.driver.io_uring_submit(
                    opcode::Write::new(
                        types::Fd(file.as_raw_fd()),
                        bounce.as_mut_bytes().as_ptr(),
                        len,
                    )
                    .offset(file_offset)
                    .rw_flags(if fua { libc::RWF_DSYNC } else { 0 })
                    .build(),
                )
            }
            .await
            .map_err(DiskError::Io)?;

            return check_io_size(bytes_written, buffers.len(), io::ErrorKind::WriteZero)
                .map_err(DiskError::Io);
        }

        use guestmem::MemoryRead;
        let mut buf = self.file.take_buffer(buffers.len());
        if let Err(err) = buffers.reader().read(&mut buf) {
            self.file.return_buffer(buf);
            return Err(err.into());
        }
        let file = self.file.file.clone();
        let (buf, result) = unblock(move || {
            let result = write_all_at(&file, &buf, file_offset).and_then(|()| {
                if fua {
                    file.sync_all()?;
                }
                Ok(())
            });
            (buf, result)
        })
        .await;
        self.file.return_buffer(buf);
        result.map_err(DiskError::Io)
    }
}

#[cfg(target_os = "linux")]
struct InFlight<'a>(&'a AtomicUsize);

#[cfg(target_os = "linux")]
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
fn use_io_uring(len: usize, in_flight: usize) -> bool {
    const MIN_IN_FLIGHT: usize = 8;

    assert!(len <= 4 * 1024);
    in_flight >= MIN_IN_FLIGHT
}

#[cfg(target_os = "linux")]
fn check_io_size(actual: i32, expected: usize, kind: io::ErrorKind) -> io::Result<()> {
    if usize::try_from(actual) == Ok(expected) {
        Ok(())
    } else {
        Err(io::Error::new(
            kind,
            format!("host file I/O transferred {actual} of {expected} bytes"),
        ))
    }
}

#[cfg(unix)]
fn file_read_at(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn file_read_at(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(unix)]
fn file_write_at(file: &fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

#[cfg(windows)]
fn file_write_at(file: &fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

fn read_exact_at(file: &fs::File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let count = file_read_at(file, buf, offset)?;
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        offset += count as u64;
        buf = &mut buf[count..];
    }
    Ok(())
}

fn write_all_at(file: &fs::File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let count = file_write_at(file, buf, offset)?;
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        offset += count as u64;
        buf = &buf[count..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use guestmem::GuestMemory;
    use pal_async::DefaultDriver;
    use scsi_buffers::OwnedRequestBuffers;

    #[pal_async::async_test]
    async fn buffer_pool_grows_and_reuses_buffers(driver: DefaultDriver) {
        let file = HostFile::new(tempfile::tempfile().unwrap(), driver).unwrap();
        for _ in 0..64 {
            file.return_buffer(vec![0; 8 * 1024]);
        }
        assert_eq!(file.buffers.lock().len(), 64);

        let buffer = file.take_buffer(128 * 1024);
        assert_eq!(buffer.len(), 128 * 1024);
        assert_eq!(file.buffers.lock().len(), 63);
        let pointer = buffer.as_ptr();
        file.return_buffer(buffer);

        let buffer = file.take_buffer(128 * 1024);
        assert_eq!(buffer.as_ptr(), pointer);
        assert_eq!(buffer.len(), 128 * 1024);
        assert_eq!(file.buffers.lock().len(), 63);
    }

    #[pal_async::async_test]
    async fn payload_round_trip_and_zero(driver: DefaultDriver) {
        let file = tempfile::tempfile().unwrap();
        file.set_len(256 * 1024).unwrap();
        let file = HostFile::new(file, driver).unwrap();
        let memory = GuestMemory::allocate(128 * 1024);
        let pattern: Vec<u8> = (0..128 * 1024).map(|i| (i % 251) as u8).collect();

        memory.write_at(0, &pattern).unwrap();
        file.request(512)
            .write_at(
                &OwnedRequestBuffers::linear(0, 512, false).buffer(&memory),
                0,
                false,
            )
            .await
            .unwrap();
        file.request(128 * 1024)
            .write_at(
                &OwnedRequestBuffers::linear(0, 128 * 1024, false).buffer(&memory),
                64 * 1024,
                true,
            )
            .await
            .unwrap();
        file.request(512)
            .write_at(
                &OwnedRequestBuffers::linear(0, 512, false).buffer(&memory),
                192 * 1024,
                false,
            )
            .await
            .unwrap();
        file.zero_at(192 * 1024, 512).await.unwrap();

        memory.fill_at(0, 0, 128 * 1024).unwrap();
        file.request(128 * 1024)
            .read_at(
                &OwnedRequestBuffers::linear(0, 128 * 1024, true).buffer(&memory),
                64 * 1024,
            )
            .await
            .unwrap();
        let mut actual = vec![0; 128 * 1024];
        memory.read_at(0, &mut actual).unwrap();
        assert_eq!(actual, pattern);

        file.request(512)
            .read_at(
                &OwnedRequestBuffers::linear(0, 512, true).buffer(&memory),
                192 * 1024,
            )
            .await
            .unwrap();
        memory.read_at(0, &mut actual[..512]).unwrap();
        assert_eq!(actual[..512], [0; 512]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn selects_io_uring_for_concurrent_small_io() {
        assert!(!use_io_uring(4096, 7));
        assert!(use_io_uring(4096, 8));
    }

    #[cfg(target_os = "linux")]
    #[pal_async::async_test]
    async fn io_uring_payload_round_trip_with_fua(driver: DefaultDriver) {
        let file = tempfile::tempfile().unwrap();
        file.set_len(4096).unwrap();
        let file = HostFile::new(file, driver).unwrap();
        if !file.io_uring_available {
            eprintln!("skipping io_uring payload test: driver does not support io_uring file I/O");
            return;
        }
        let requests: Vec<_> = (0..8).map(|_| file.request(4096)).collect();
        let request = &requests[7];
        assert!(request.use_io_uring);

        let memory = GuestMemory::allocate(4096);
        let pattern: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        memory.write_at(0, &pattern).unwrap();
        request
            .write_at(
                &OwnedRequestBuffers::linear(0, 4096, false).buffer(&memory),
                0,
                true,
            )
            .await
            .unwrap();
        memory.fill_at(0, 0, 4096).unwrap();
        request
            .read_at(
                &OwnedRequestBuffers::linear(0, 4096, true).buffer(&memory),
                0,
            )
            .await
            .unwrap();
        let mut actual = vec![0; 4096];
        memory.read_at(0, &mut actual).unwrap();
        assert_eq!(actual, pattern);
    }

    #[cfg(target_os = "linux")]
    #[pal_async::async_test]
    async fn payload_without_io_uring() {
        pal_async::local::block_with_io(async |driver| {
            let file = tempfile::tempfile().unwrap();
            file.set_len(4096).unwrap();
            let file = HostFile::new(file, driver).unwrap();
            assert!(!file.io_uring_available);
            let requests: Vec<_> = (0..8).map(|_| file.request(4096)).collect();
            assert!(requests.iter().all(|request| !request.use_io_uring));
            let memory = GuestMemory::allocate(4096);
            memory.fill_at(0, 0xAB, 4096).unwrap();
            requests[7]
                .write_at(
                    &OwnedRequestBuffers::linear(0, 4096, false).buffer(&memory),
                    0,
                    true,
                )
                .await
                .unwrap();
            memory.fill_at(0, 0, 4096).unwrap();
            requests[7]
                .read_at(
                    &OwnedRequestBuffers::linear(0, 4096, true).buffer(&memory),
                    0,
                )
                .await
                .unwrap();
            let mut actual = vec![0; 4096];
            memory.read_at(0, &mut actual).unwrap();
            assert_eq!(actual, [0xAB; 4096]);
            file.zero_at(0, 4096).await.unwrap();
            requests[7]
                .read_at(
                    &OwnedRequestBuffers::linear(0, 4096, true).buffer(&memory),
                    0,
                )
                .await
                .unwrap();
            memory.read_at(0, &mut actual).unwrap();
            assert_eq!(actual, [0; 4096]);
        });
    }
}
