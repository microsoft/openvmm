// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Flushing modified pages of shared file views to their files.

use super::PAGE_SIZE;
use super::SparseMapping;
use std::io;
use std::io::Error;
use windows_sys::Win32::System::Memory::FlushViewOfFile;

const MIN_FLUSH_BYTES_PER_WORKER: usize = 16 * 1024 * 1024;
const MAX_FLUSH_WORKERS: usize = 8;

fn flush_file_range(
    offset: usize,
    len: usize,
    flush: impl Fn(usize, usize) -> io::Result<()> + Sync,
) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let workers = (len / MIN_FLUSH_BYTES_PER_WORKER).clamp(1, MAX_FLUSH_WORKERS);
    if workers == 1 {
        return flush(offset, len);
    }

    // Fragmented dirty pages make one synchronous flush I/O-latency bound.
    // Use bounded, page-aligned chunks and finish all of them before returning.
    let chunk_size = (len / PAGE_SIZE).div_ceil(workers) * PAGE_SIZE;
    std::thread::scope(|scope| {
        let flush = &flush;
        let mut threads = Vec::with_capacity(workers - 1);
        for start in (chunk_size..len).step_by(chunk_size) {
            let chunk_len = (len - start).min(chunk_size);
            threads.push(
                std::thread::Builder::new()
                    .name("mapped-flush".to_owned())
                    .spawn_scoped(scope, move || flush(offset + start, chunk_len))?,
            );
        }
        let mut result = flush(offset, chunk_size);
        for thread in threads {
            let worker_result = thread.join().expect("mapped-memory flush worker panicked");
            result = result.and(worker_result);
        }
        result
    })
}

impl SparseMapping {
    /// Flushes modified shared file pages in a populated local view.
    pub fn flush(&self, offset: usize, len: usize) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        if self.process.is_some() {
            return Err(Error::new(
                io::ErrorKind::Unsupported,
                "flushing a remote mapped view is unsupported",
            ));
        }
        flush_file_range(offset, len, |offset, len| {
            // SAFETY: `validate_offset_len` proves the range is within this
            // reservation. Callers use this only for populated shared views.
            if unsafe { FlushViewOfFile(self.as_ptr().add(offset), len) } == 0 {
                return Err(Error::last_os_error());
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::MAX_FLUSH_WORKERS;
    use super::MIN_FLUSH_BYTES_PER_WORKER;
    use super::PAGE_SIZE;
    use super::SparseMapping;
    use super::flush_file_range;
    use parking_lot::Condvar;
    use parking_lot::Mutex;
    use std::io;
    use std::io::Read;
    use std::io::Seek;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;
    use std::time::Instant;
    use test_with_tracing::test;

    const SNAPSHOT_RAM_BYTES: usize = 128 * 1024 * 1024;

    #[test]
    fn flush_file_range_covers_each_page_once() {
        for len in [
            0,
            PAGE_SIZE,
            MIN_FLUSH_BYTES_PER_WORKER - PAGE_SIZE,
            MIN_FLUSH_BYTES_PER_WORKER,
            MIN_FLUSH_BYTES_PER_WORKER + PAGE_SIZE,
            MIN_FLUSH_BYTES_PER_WORKER * 2 + PAGE_SIZE,
            SNAPSHOT_RAM_BYTES,
            MIN_FLUSH_BYTES_PER_WORKER * 20 + PAGE_SIZE * 3,
        ] {
            let ranges = Mutex::new(Vec::new());
            flush_file_range(PAGE_SIZE, len, |offset, len| {
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
                (len / MIN_FLUSH_BYTES_PER_WORKER).clamp(1, MAX_FLUSH_WORKERS)
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
    fn fragmented_flush_is_bounded_parallel_and_joins_on_error() {
        let chunk_len = SNAPSHOT_RAM_BYTES / MAX_FLUSH_WORKERS;
        for failing_chunk in [None, Some(0), Some(1)] {
            let (started, receiver) = mpsc::channel();
            let released = Mutex::new(false);
            let release = Condvar::new();
            let completed = AtomicUsize::new(0);
            std::thread::scope(|scope| {
                let released = &released;
                let release = &release;
                let coordinator = scope.spawn(move || {
                    let all_started = (0..MAX_FLUSH_WORKERS)
                        .all(|_| receiver.recv_timeout(Duration::from_secs(10)).is_ok());
                    *released.lock() = true;
                    release.notify_all();
                    all_started
                });
                let result = flush_file_range(0, SNAPSHOT_RAM_BYTES, |offset, len| {
                    if len != chunk_len {
                        return Err(io::Error::other(
                            "fragmented writeback was submitted as one serial flush",
                        ));
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
                assert!(all_started, "flush workers did not overlap");
                let completed = completed.load(Ordering::SeqCst);
                assert!(completed > 1, "large flushes must overlap");
                assert_eq!(completed, MAX_FLUSH_WORKERS);
            });
        }
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
        flush_fragmented_file(MIN_FLUSH_BYTES_PER_WORKER * 2 + PAGE_SIZE * 3, |mapping| {
            mapping.flush(0, mapping.len())
        });
    }

    #[test]
    #[ignore = "manual storage timing comparison; run without concurrent workloads"]
    fn profile_fragmented_file_flush() {
        let mut serial = Vec::new();
        let mut parallel = Vec::new();
        for iteration in 0_usize..5 {
            let order = if iteration.is_multiple_of(2) {
                [false, true]
            } else {
                [true, false]
            };
            for parallel_flush in order {
                let (mapped, file) = flush_fragmented_file(SNAPSHOT_RAM_BYTES, |mapping| {
                    if parallel_flush {
                        mapping.flush(0, mapping.len())
                    } else {
                        // SAFETY: the complete local shared view is populated
                        // and remains mapped until the synchronous call returns.
                        if unsafe { super::FlushViewOfFile(mapping.as_ptr(), mapping.len()) } == 0 {
                            Err(io::Error::last_os_error())
                        } else {
                            Ok(())
                        }
                    }
                });
                eprintln!(
                    "parallel={parallel_flush} mapped_flush_ms={:.3} file_sync_ms={:.3}",
                    mapped.as_secs_f64() * 1000.0,
                    file.as_secs_f64() * 1000.0,
                );
                if parallel_flush {
                    parallel.push(mapped + file);
                } else {
                    serial.push(mapped + file);
                }
            }
        }
        serial.sort_unstable();
        parallel.sort_unstable();
        let serial = serial[serial.len() / 2].as_secs_f64();
        let parallel = parallel[parallel.len() / 2].as_secs_f64();
        eprintln!(
            "fragmented_flush serial_p50_ms={:.3} parallel_p50_ms={:.3} speedup={:.2}",
            serial * 1000.0,
            parallel * 1000.0,
            serial / parallel,
        );
    }
}
