// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Flushing modified pages of shared file views to their files.

use super::PAGE_SIZE;
use super::SparseMapping;
use std::io;
use std::io::Error;
use windows_sys::Win32::System::Memory::FlushViewOfFile;
use windows_sys::Win32::System::Memory::MEM_COMMIT;
use windows_sys::Win32::System::Memory::MEM_MAPPED;
use windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION;
use windows_sys::Win32::System::Memory::PAGE_EXECUTE_READWRITE;
use windows_sys::Win32::System::Memory::PAGE_READWRITE;
use windows_sys::Win32::System::Memory::VirtualQuery;

const MIN_MARK_BYTES_PER_WORKER: usize = 16 * 1024 * 1024;
const MAX_MARK_WORKERS: usize = 8;

/// Runs `op` on disjoint, page-aligned chunks that cover `offset..offset + len`,
/// on up to [`MAX_MARK_WORKERS`] threads, and waits for every chunk, including
/// when one fails.
fn for_each_chunk(
    offset: usize,
    len: usize,
    op: impl Fn(usize, usize) -> io::Result<()> + Sync,
) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let workers = (len / MIN_MARK_BYTES_PER_WORKER).clamp(1, MAX_MARK_WORKERS);
    if workers == 1 {
        return op(offset, len);
    }

    let chunk_size = (len / PAGE_SIZE).div_ceil(workers) * PAGE_SIZE;
    std::thread::scope(|scope| {
        let op = &op;
        let mut threads = Vec::with_capacity(workers - 1);
        for start in (chunk_size..len).step_by(chunk_size) {
            let chunk_len = (len - start).min(chunk_size);
            threads.push(
                std::thread::Builder::new()
                    .name("mapped-flush".to_owned())
                    .spawn_scoped(scope, move || op(offset + start, chunk_len))?,
            );
        }
        let mut result = op(offset, chunk_size);
        for thread in threads {
            let worker_result = thread.join().expect("mapped-memory flush worker panicked");
            result = result.and(worker_result);
        }
        result
    })
}

/// Marks every page in `offset..offset + len` modified with `mark`, then writes
/// the whole range with one ascending `flush`.
///
/// NTFS zero-fills a file below any write that starts past the file's valid
/// data length, and a newly sized file has none. Flushing disjoint ranges
/// concurrently therefore writes zeros over pages that another range writes
/// again, and flushing scattered modified pages issues one small,
/// latency-bound write per run. Once every page is modified, one ascending
/// flush writes each page once, in large sequential writes.
fn mark_then_flush(
    offset: usize,
    len: usize,
    mark: impl Fn(usize, usize) -> io::Result<()> + Sync,
    flush: impl FnOnce(usize, usize) -> io::Result<()>,
) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    for_each_chunk(offset, len, mark)?;
    flush(offset, len)
}

impl SparseMapping {
    /// Flushes a populated local view of a shared file to the file.
    ///
    /// This first marks every page of the range's committed, writable views
    /// modified without changing its contents, so the flush writes every such
    /// page, including pages that the view never touched, and writes the range
    /// exactly once. Pages of read-only views are flushed as they are.
    pub fn flush(&self, offset: usize, len: usize) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        if self.process.is_some() {
            return Err(Error::new(
                io::ErrorKind::Unsupported,
                "flushing a remote mapped view is unsupported",
            ));
        }
        mark_then_flush(
            offset,
            len,
            |offset, len| self.mark_writable_pages_modified(offset, len),
            |offset, len| {
                // SAFETY: `validate_offset_len` proves the range is within
                // this reservation. Callers use this only for populated shared
                // views.
                if unsafe { FlushViewOfFile(self.as_ptr().add(offset), len) } == 0 {
                    return Err(Error::last_os_error());
                }
                Ok(())
            },
        )
    }

    /// Writes each page's first byte back to itself in the committed, writable
    /// mapped views of a validated, page-aligned range, so the memory manager
    /// treats every such page as modified.
    fn mark_writable_pages_modified(&self, offset: usize, len: usize) -> io::Result<()> {
        let base = self.as_ptr() as usize;
        let end = offset + len;
        let mut region = offset;
        while region < end {
            let mut info = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: the address is within this reservation, and `info` is
            // valid for writes of its size.
            let written = unsafe {
                VirtualQuery(
                    self.as_ptr().byte_add(region),
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if written == 0 {
                return Err(Error::last_os_error());
            }
            let region_end = (info.BaseAddress as usize + info.RegionSize - base).min(end);
            // Exact protections exclude guard, no-cache, and copy-on-write
            // pages, and deferred-protect views that are still read-only.
            if info.State == MEM_COMMIT
                && info.Type == MEM_MAPPED
                && matches!(info.Protect, PAGE_READWRITE | PAGE_EXECUTE_READWRITE)
            {
                for page in (region..region_end).step_by(PAGE_SIZE) {
                    let byte = self.as_ptr().cast::<u8>().wrapping_add(page);
                    // SAFETY: the page is within this reservation, in a
                    // committed, writable view. A fault is reported.
                    let value =
                        unsafe { trycopy::try_read_volatile(byte) }.map_err(Error::other)?;
                    // A failed exchange means that a concurrent write changed
                    // the byte, which also modified the page.
                    // SAFETY: as above.
                    let _ = unsafe { trycopy::try_compare_exchange(byte, value, value) }
                        .map_err(Error::other)?;
                }
            }
            region = region_end;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MAX_MARK_WORKERS;
    use super::MIN_MARK_BYTES_PER_WORKER;
    use super::PAGE_SIZE;
    use super::SparseMapping;
    use super::for_each_chunk;
    use super::mark_then_flush;
    use pal::windows::fs::sparse::allocated_ranges;
    use pal::windows::fs::sparse::set_sparse;
    use parking_lot::Condvar;
    use parking_lot::Mutex;
    use std::io;
    use std::io::Read;
    use std::io::Seek;
    use std::ops::Range;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;
    use std::time::Instant;
    use test_with_tracing::test;
    use windows_sys::Win32::System::Memory::PAGE_READONLY;

    const MIB: usize = 1024 * 1024;
    const SNAPSHOT_RAM_BYTES: usize = 128 * MIB;

    #[test]
    fn for_each_chunk_covers_each_page_once() {
        for len in [
            0,
            PAGE_SIZE,
            MIN_MARK_BYTES_PER_WORKER - PAGE_SIZE,
            MIN_MARK_BYTES_PER_WORKER,
            MIN_MARK_BYTES_PER_WORKER + PAGE_SIZE,
            MIN_MARK_BYTES_PER_WORKER * 2 + PAGE_SIZE,
            SNAPSHOT_RAM_BYTES,
            MIN_MARK_BYTES_PER_WORKER * 20 + PAGE_SIZE * 3,
        ] {
            let ranges = Mutex::new(Vec::new());
            for_each_chunk(PAGE_SIZE, len, |offset, len| {
                assert!(offset.is_multiple_of(PAGE_SIZE));
                assert!(len.is_multiple_of(PAGE_SIZE));
                assert_ne!(len, 0);
                ranges.lock().push(offset..offset + len);
                Ok(())
            })
            .unwrap();
            let mut ranges = ranges.into_inner();
            ranges.sort_by_key(|range| range.start);
            let expected_workers = if len == 0 {
                0
            } else {
                (len / MIN_MARK_BYTES_PER_WORKER).clamp(1, MAX_MARK_WORKERS)
            };
            assert_eq!(ranges.len(), expected_workers, "length {len}");
            let mut end = PAGE_SIZE;
            for range in ranges {
                assert_eq!(range.start, end);
                end = range.end;
            }
            assert_eq!(end, PAGE_SIZE + len);
        }
    }

    #[test]
    fn chunks_are_bounded_parallel_and_join_on_error() {
        let chunk_len = SNAPSHOT_RAM_BYTES / MAX_MARK_WORKERS;
        for failing_chunk in [None, Some(0), Some(1)] {
            let (started, receiver) = mpsc::channel();
            let released = Mutex::new(false);
            let release = Condvar::new();
            let completed = AtomicUsize::new(0);
            std::thread::scope(|scope| {
                let released = &released;
                let release = &release;
                let coordinator = scope.spawn(move || {
                    let all_started = (0..MAX_MARK_WORKERS)
                        .all(|_| receiver.recv_timeout(Duration::from_secs(10)).is_ok());
                    *released.lock() = true;
                    release.notify_all();
                    all_started
                });
                let result = for_each_chunk(0, SNAPSHOT_RAM_BYTES, |offset, len| {
                    if len != chunk_len {
                        return Err(io::Error::other("a large range ran as one serial chunk"));
                    }
                    started.send(()).unwrap();
                    let mut released = released.lock();
                    while !*released {
                        release.wait(&mut released);
                    }
                    completed.fetch_add(1, Ordering::SeqCst);
                    if failing_chunk == Some(offset / chunk_len) {
                        Err(io::Error::from_raw_os_error(5))
                    } else {
                        Ok(())
                    }
                });
                drop(started);
                let all_started = coordinator.join().unwrap();
                if failing_chunk.is_some() {
                    assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
                } else {
                    result.unwrap();
                }
                assert!(all_started, "chunk workers did not overlap");
                let completed = completed.load(Ordering::SeqCst);
                assert!(completed > 1, "large ranges must run concurrently");
                assert_eq!(completed, MAX_MARK_WORKERS);
            });
        }
    }

    #[test]
    fn marks_every_chunk_before_one_flush() {
        let len = MIN_MARK_BYTES_PER_WORKER * 3 + PAGE_SIZE;
        let marked = Mutex::new(Vec::new());
        let flushed = Mutex::new(Vec::new());
        mark_then_flush(
            PAGE_SIZE,
            len,
            |offset, len| {
                assert!(flushed.lock().is_empty(), "flushed before marking ended");
                marked.lock().push(offset..offset + len);
                Ok(())
            },
            |offset, len| {
                flushed.lock().push(offset..offset + len);
                Ok(())
            },
        )
        .unwrap();
        let mut marked = marked.into_inner();
        marked.sort_by_key(|range| range.start);
        assert_eq!(marked.len(), 3);
        assert_eq!(marked[0].start, PAGE_SIZE);
        assert_eq!(marked[2].end, PAGE_SIZE + len);
        assert_eq!(flushed.into_inner(), [PAGE_SIZE..PAGE_SIZE + len]);

        let error = mark_then_flush(
            0,
            len,
            |offset, _| {
                if offset == 0 {
                    Err(io::Error::from_raw_os_error(5))
                } else {
                    Ok(())
                }
            },
            |_, _| panic!("flushed after a failed mark"),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));

        mark_then_flush(
            0,
            0,
            |_, _| panic!("marked an empty range"),
            |_, _| panic!("flushed an empty range"),
        )
        .unwrap();
    }

    fn allocated_bytes_within(ranges: &[(u64, u64)], window: Range<usize>) -> u64 {
        ranges
            .iter()
            .map(|&(offset, length)| {
                let start = offset.max(window.start as u64);
                let end = (offset + length).min(window.end as u64);
                end.saturating_sub(start)
            })
            .sum()
    }

    #[test]
    fn flush_writes_writable_views_whole_and_read_only_views_as_they_are() {
        let len = 8 * MIB;
        let read_only = 4 * MIB..6 * MIB;
        let mut file = tempfile::tempfile().unwrap();
        // A sparse file allocates only the ranges that are written, so its
        // allocation shows what the flush wrote.
        set_sparse(&file).unwrap();
        file.set_len(len as u64).unwrap();
        let mappable = crate::windows::new_mappable_from_file(&file, true, false).unwrap();
        let mapping = SparseMapping::new(len).unwrap();
        mapping.map_file(0, len, &mappable, 0, true).unwrap();
        mapping.write_at(0, b"first").unwrap();
        mapping.write_at(read_only.start, b"read-only").unwrap();
        mapping.write_at(len - PAGE_SIZE, b"last").unwrap();
        mapping
            .protect(read_only.start, read_only.len(), PAGE_READONLY)
            .unwrap();

        mapping.flush(0, len).unwrap();
        drop(mapping);
        drop(mappable);

        let ranges = allocated_ranges(&file, len as u64).unwrap();
        for writable in [0..read_only.start, read_only.end..len] {
            assert_eq!(
                allocated_bytes_within(&ranges, writable.clone()),
                writable.len() as u64,
                "writable view {writable:x?} was not written whole: {ranges:x?}"
            );
        }
        let read_only_bytes = allocated_bytes_within(&ranges, read_only.clone());
        assert!(
            read_only_bytes > 0 && read_only_bytes < MIB as u64,
            "read-only view wrote {read_only_bytes:#x} bytes: {ranges:x?}"
        );

        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), len);
        assert_eq!(&bytes[..5], b"first");
        assert_eq!(&bytes[read_only.start..read_only.start + 9], b"read-only");
        assert_eq!(&bytes[len - PAGE_SIZE..len - PAGE_SIZE + 4], b"last");
        let nonzero = bytes.iter().filter(|&&byte| byte != 0).count();
        assert_eq!(nonzero, b"first".len() + b"read-only".len() + b"last".len());
    }

    fn flush_fragmented_file(
        len: usize,
        flush: impl FnOnce(&SparseMapping) -> io::Result<()>,
    ) -> (Duration, Duration) {
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(len as u64).unwrap();
        file.sync_all().unwrap();
        let mappable = crate::windows::new_mappable_from_file(&file, true, false).unwrap();
        let mapping = SparseMapping::new(len).unwrap();
        mapping.map_file(0, len, &mappable, 0, true).unwrap();
        for offset in (0..len).step_by(PAGE_SIZE * 2) {
            mapping.fill_at(offset, 0x5a, PAGE_SIZE).unwrap();
        }

        let start = Instant::now();
        flush(&mapping).unwrap();
        let mapped_flush = start.elapsed();
        let start = Instant::now();
        file.sync_all().unwrap();
        let file_flush = start.elapsed();
        drop(mapping);
        drop(mappable);

        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), len);
        for (index, page) in bytes.as_chunks::<PAGE_SIZE>().0.iter().enumerate() {
            let expected = if index.is_multiple_of(2) { 0x5a } else { 0 };
            assert!(page.iter().all(|&byte| byte == expected), "page {index}");
        }
        (mapped_flush, file_flush)
    }

    #[test]
    fn flush_fragmented_file_preserves_bytes() {
        flush_fragmented_file(MIN_MARK_BYTES_PER_WORKER * 2 + PAGE_SIZE * 3, |mapping| {
            mapping.flush(0, mapping.len())
        });
    }

    #[test]
    #[ignore = "manual storage timing comparison; run without concurrent workloads"]
    fn profile_fragmented_file_flush() {
        let mut chunked = Vec::new();
        let mut marked = Vec::new();
        for iteration in 0_usize..5 {
            let order = if iteration.is_multiple_of(2) {
                [false, true]
            } else {
                [true, false]
            };
            for mark in order {
                let (mapped, file) = flush_fragmented_file(SNAPSHOT_RAM_BYTES, |mapping| {
                    if mark {
                        mapping.flush(0, mapping.len())
                    } else {
                        // The former policy: flush disjoint chunks concurrently
                        // without marking their pages first.
                        for_each_chunk(0, mapping.len(), |offset, len| {
                            // SAFETY: the complete local shared view is
                            // populated and remains mapped until the
                            // synchronous call returns.
                            if unsafe {
                                super::FlushViewOfFile(mapping.as_ptr().byte_add(offset), len)
                            } == 0
                            {
                                Err(io::Error::last_os_error())
                            } else {
                                Ok(())
                            }
                        })
                    }
                });
                eprintln!(
                    "marked={mark} mapped_flush_ms={:.3} file_sync_ms={:.3}",
                    mapped.as_secs_f64() * 1000.0,
                    file.as_secs_f64() * 1000.0,
                );
                if mark {
                    marked.push(mapped + file);
                } else {
                    chunked.push(mapped + file);
                }
            }
        }
        chunked.sort_unstable();
        marked.sort_unstable();
        let chunked = chunked[chunked.len() / 2].as_secs_f64();
        let marked = marked[marked.len() / 2].as_secs_f64();
        eprintln!(
            "fragmented_flush chunked_p50_ms={:.3} marked_p50_ms={:.3} speedup={:.2}",
            chunked * 1000.0,
            marked * 1000.0,
            chunked / marked,
        );
    }
}
