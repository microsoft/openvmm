// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous `io_uring` reads of VMDK backing files on Linux.

// UNSAFETY: Submitting reads while owning the locked guest pages and iovecs.
#![expect(unsafe_code)]

use disk_backend::DiskError;
use io_uring::opcode;
use io_uring::types;
use pal_async::driver::Driver;
use scsi_buffers::RequestBuffers;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;

// Linux limits the number of buffer descriptions in one vectored read.
const MAX_READ_IOVECS: usize = libc::UIO_MAXIOV as usize;

/// Returns whether the driver supports both read opcodes used by this module.
pub(crate) fn is_supported(driver: &dyn Driver) -> bool {
    driver.io_uring_probe(opcode::Read::CODE) && driver.io_uring_probe(opcode::Readv::CODE)
}

/// Fills the guest buffers with file data starting at byte `offset`.
///
/// Reads byte ranges into writable guest memory that can be locked during I/O.
/// The file must be opened without `O_DIRECT`; its length need not be a multiple
/// of the sector size. Reads past the file's end fail instead of adding zeros,
/// so callers must handle padding for partial sectors.
///
/// Splits submissions to fit the opcode's byte-count and vector-count limits.
/// Continues after partial reads and retries interrupted operations.
///
/// # Cancellation
///
/// Dropping this future while I/O is in flight aborts the process, preventing
/// the kernel from accessing released buffers. Await it to completion instead.
pub(crate) async fn read_exact_at(
    driver: &dyn Driver,
    file: Arc<File>,
    buffers: &RequestBuffers<'_>,
    offset: u64,
) -> Result<(), DiskError> {
    // Linux file offsets are signed 64-bit. Validate the entire read range
    // before locking guest memory.
    offset
        .checked_add(buffers.len() as u64)
        .filter(|end| *end <= i64::MAX as u64)
        .ok_or(DiskError::InvalidInput)?;

    // A locked range borrows guest memory, so also retain its owning reference.
    let guest_memory = buffers.guest_memory().clone();
    let mut completed = 0;

    // Fill the request, retrying short or interrupted reads.
    while completed < buffers.len() {
        let remaining = buffers.subrange(completed, buffers.len() - completed);

        // Lock guest buffers for writes and get their I/O vectors.
        // Lists of up to 16 vectors are stored inside the future; longer lists
        // are allocated separately so the future itself stays small.
        let locked = remaining.lock_with_capacity::<16>(true)?;
        let io_vecs = locked.io_vecs();

        let entry = if io_vecs.len() == 1 {
            // Read into one guest buffer using its writable address and byte length.
            // Bound READ's 32-bit length field; the loop handles any remaining bytes.
            opcode::Read::new(
                types::Fd(file.as_raw_fd()),
                io_vecs[0].as_ptr().cast_mut().cast(),
                io_vecs[0].len().min(u32::MAX as usize) as u32,
            )
            .offset(offset + completed as u64)
            .build()
        } else {
            // Read the buffer descriptions, then fill the guest buffers in order.
            // Submit only as many vectors as Linux allows, then continue the rest.
            opcode::Readv::new(
                types::Fd(file.as_raw_fd()),
                io_vecs.as_ptr().cast(),
                io_vecs.len().min(MAX_READ_IOVECS) as u32,
            )
            .offset(offset + completed as u64)
            .build()
        };

        // SAFETY: The guest-memory owner, locked pages, iovec storage, and file
        // handle are owned by this async state machine across the await, even
        // if leaked. Dropping an in-flight submission aborts before release.
        let result = unsafe { driver.io_uring_submit(entry) }.await;

        // Advance both byte positions after a short completion. Only EOF
        // before the requested end fails; it must never become padding.
        match result {
            Ok(0) => {
                return Err(DiskError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "VMDK source file ended before the requested bytes were read",
                )));
            }
            Ok(count) => completed += count as usize,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(DiskError::Io(error)),
        }
    }

    drop(guest_memory);
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use guestmem::GuestMemory;
    use guestmem::MemoryRead;
    use pal_async::async_test;
    use pal_async::driver::PollImpl;
    use scsi_buffers::OwnedRequestBuffers;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    /// Counts submissions in flight through [`InterruptedDriver`].
    #[derive(Default)]
    pub(crate) struct InFlight {
        current: AtomicUsize,
        /// Highest number of submissions in flight at once.
        pub(crate) peak: AtomicUsize,
    }

    /// Test driver that simulates interrupted I/O and unsupported opcodes.
    pub(crate) struct InterruptedDriver {
        /// Real driver used for non-injected operations.
        pub(crate) inner: pal_async::DefaultDriver,
        /// Remaining interruptions to inject, shared with the test.
        pub(crate) interruptions: Arc<AtomicUsize>,
        /// Opcode to report as unsupported, if any.
        pub(crate) unsupported_opcode: Option<u8>,
        /// Counts submissions in flight, if set.
        pub(crate) in_flight: Option<Arc<InFlight>>,
    }

    impl Driver for InterruptedDriver {
        /// Delegates timer creation to the real driver.
        fn new_dyn_timer(&self) -> PollImpl<dyn pal_async::timer::PollTimer> {
            self.inner.new_dyn_timer()
        }

        /// Delegates descriptor readiness to the real driver.
        fn new_dyn_fd_ready(
            &self,
            fd: std::os::fd::RawFd,
        ) -> io::Result<PollImpl<dyn pal_async::fd::PollFdReady>> {
            self.inner.new_dyn_fd_ready(fd)
        }

        /// Delegates socket readiness to the real driver.
        fn new_dyn_socket_ready(
            &self,
            socket: std::os::fd::RawFd,
        ) -> io::Result<PollImpl<dyn pal_async::socket::PollSocketReady>> {
            self.inner.new_dyn_socket_ready(socket)
        }

        /// Delegates waits to the real driver.
        fn new_dyn_wait(
            &self,
            fd: std::os::fd::RawFd,
            read_size: usize,
        ) -> io::Result<PollImpl<dyn pal_async::wait::PollWait>> {
            self.inner.new_dyn_wait(fd, read_size)
        }

        /// Can hide one opcode to exercise setup fallback.
        fn io_uring_probe(&self, opcode: u8) -> bool {
            self.unsupported_opcode != Some(opcode) && self.inner.io_uring_probe(opcode)
        }

        /// Injects EINTR before submitting anything, then delegates unchanged.
        unsafe fn io_uring_submit(
            &self,
            sqe: pal_async::io_uring::Entry,
        ) -> std::pin::Pin<Box<dyn Future<Output = io::Result<i32>> + Send + '_>> {
            Box::pin(async move {
                if self
                    .interruptions
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                        count.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Err(io::ErrorKind::Interrupted.into());
                }

                // Count this submission as in flight until it completes. Real
                // submissions are pending on their first poll, so concurrent
                // reads overlap here.
                if let Some(in_flight) = &self.in_flight {
                    let current = in_flight.current.fetch_add(1, Ordering::SeqCst) + 1;
                    in_flight.peak.fetch_max(current, Ordering::SeqCst);
                }

                // SAFETY: The caller's submission contract is forwarded
                // unchanged to the underlying driver and awaited to completion.
                let result = unsafe { self.inner.io_uring_submit(sqe) }.await;

                if let Some(in_flight) = &self.in_flight {
                    in_flight.current.fetch_sub(1, Ordering::SeqCst);
                }
                result
            })
        }
    }

    // Verifies varied read sizes, unaligned offsets, and the empty-request no-op.
    #[async_test]
    async fn reads_varied_lengths_at_unaligned_offsets(driver: pal_async::DefaultDriver) {
        for length in [
            0,
            1,
            511,
            512,
            513,
            4095,
            4096,
            4097,
            1024 * 1024,
            1024 * 1024 + 17,
            4 * 1024 * 1024 + 17,
        ] {
            // Include the seven-byte prefix skipped by the read.
            let file = tempfile::tempfile_in(".").unwrap();
            let data = (0..length + 7).map(|i| i as u8).collect::<Vec<_>>();
            io::Write::write_all(&mut &file, &data).unwrap();

            let memory = GuestMemory::allocate(length + 3);
            // linear() counts pages from the length alone, so an unaligned
            // range crossing an extra page must explicitly supply those pages.
            let pages = (0..(length + 3).div_ceil(4096) as u64).collect::<Vec<_>>();
            let owned = OwnedRequestBuffers::new_unaligned(&pages, 3, length);

            // Read from file offset 7 into guest offset 3.
            read_exact_at(&driver, Arc::new(file), &owned.buffer(&memory), 7)
                .await
                .unwrap();

            let mut actual = vec![0; length];
            memory.read_at(3, &mut actual).unwrap();
            assert_eq!(actual, data[7..]);
        }
    }

    // Verifies fragmented reads spill inline storage and respect the kernel vector limit.
    #[async_test]
    async fn reads_fragmented_buffers_beyond_inline_capacity(driver: pal_async::DefaultDriver) {
        // Exercise inline spills and both sides of Linux's vector-count limit.
        for page_count in [37, MAX_READ_IOVECS, MAX_READ_IOVECS + 1] {
            // Each page is 4 KiB; leave 3 bytes unused at the start and 6 at the end.
            let length = page_count * 4096 - 9;

            // Include the 7 leading file bytes that the read will skip.
            let data = (0..length + 7).map(|index| index as u8).collect::<Vec<_>>();

            // Write the pattern to a temporary file.
            let file = tempfile::tempfile_in(".").unwrap();
            io::Write::write_all(&mut &file, &data).unwrap();

            // Select every other guest page to keep the destination buffers separate.
            let memory = GuestMemory::allocate(page_count * 2 * 4096);
            let gpns = (0..page_count)
                .map(|page| (page * 2) as u64)
                .collect::<Vec<_>>();

            // Describe a range starting 3 bytes into the first selected guest page.
            let owned = OwnedRequestBuffers::new_unaligned(&gpns, 3, length);
            let buffers = owned.buffer(&memory);

            // Lock for writes; vector lists longer than 16 spill to heap storage.
            let locked = buffers.lock_with_capacity::<16>(true).unwrap();

            // Each separated guest page must produce its own I/O vector.
            assert_eq!(locked.io_vecs().len(), page_count);

            // Release this inspection lock before the read acquires its own.
            drop(locked);

            // Read from a nonzero file offset into the fragmented guest buffers.
            read_exact_at(&driver, Arc::new(file), &buffers, 7)
                .await
                .unwrap();

            // Verify all destination bytes match the requested file range.
            let mut actual = vec![0; length];
            buffers.reader().read(&mut actual).unwrap();
            assert_eq!(actual, data[7..]);
        }
    }

    /// Verifies read errors when the file ends early, the guest destination is
    /// read-only, or the file offset overflows.
    ///
    /// Confirms that a partial read leaves the unread tail unchanged and that
    /// read-only guest buffers are not modified.
    #[async_test]
    async fn reports_early_eof_and_rejects_invalid_requests(driver: pal_async::DefaultDriver) {
        // Create a source shorter than the requested read.
        let file = tempfile::tempfile_in(".").unwrap();
        io::Write::write_all(&mut &file, &[0x41; 513]).unwrap();
        let file = Arc::new(file);

        // Fill the destination with a sentinel to detect unwanted writes.
        let memory = GuestMemory::allocate(1024);
        memory.write_at(0, &[0xa5; 1024]).unwrap();
        let writable = OwnedRequestBuffers::linear(0, 1024, true);

        // Reading past EOF must fail rather than pad the destination.
        let result = read_exact_at(&driver, file.clone(), &writable.buffer(&memory), 0).await;
        assert!(
            matches!(result, Err(DiskError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
        );

        // Verify the available bytes were copied and the remainder is untouched.
        let mut actual = [0; 1024];
        memory.read_at(0, &mut actual).unwrap();
        assert_eq!(&actual[..513], &[0x41; 513]);
        assert_eq!(&actual[513..], &[0xa5; 511]);

        // A forbidden destination must not be modified, including on failure.
        let read_only = OwnedRequestBuffers::linear(0, 512, false);
        assert!(
            read_exact_at(&driver, file.clone(), &read_only.buffer(&memory), 0)
                .await
                .is_err()
        );
        let mut unchanged = [0; 1024];
        memory.read_at(0, &mut unchanged).unwrap();
        assert_eq!(unchanged, actual);

        // Reject an overflowing file offset.
        assert!(matches!(
            read_exact_at(&driver, file, &writable.buffer(&memory), u64::MAX).await,
            Err(DiskError::InvalidInput)
        ));
    }

    // Verifies EINTR retries with the same byte offset and guest range, and
    // capability detection without creating a sector-oriented disk backend.
    #[async_test]
    async fn retries_interrupted_reads_and_checks_opcode_support(driver: pal_async::DefaultDriver) {
        // Create source data with a one-byte prefix to skip.
        let file = tempfile::tempfile_in(".").unwrap();
        io::Write::write_all(&mut &file, &[0x41; 17]).unwrap();
        let file = Arc::new(file);

        // Inject two interruptions before allowing real I/O.
        let interruptions = Arc::new(AtomicUsize::new(2));
        let interrupted_driver = InterruptedDriver {
            inner: driver.clone(),
            interruptions: interruptions.clone(),
            unsupported_opcode: None,
            in_flight: None,
        };
        assert!(is_supported(&interrupted_driver));

        // Read from offset 1, retrying the injected interruptions.
        let memory = GuestMemory::allocate(16);
        read_exact_at(
            &interrupted_driver,
            file,
            &OwnedRequestBuffers::linear(0, 16, true).buffer(&memory),
            1,
        )
        .await
        .unwrap();

        // Verify the read completed and both interruptions were consumed.
        let mut actual = [0; 16];
        memory.read_at(0, &mut actual).unwrap();
        assert_eq!(actual, [0x41; 16]);
        assert_eq!(interruptions.load(Ordering::SeqCst), 0);

        // Missing either required read opcode selects the portable fallback.
        for opcode in [opcode::Read::CODE, opcode::Readv::CODE] {
            assert!(!is_supported(&InterruptedDriver {
                inner: driver.clone(),
                interruptions: Arc::new(AtomicUsize::new(0)),
                unsupported_opcode: Some(opcode),
                in_flight: None,
            }));
        }
    }
}
