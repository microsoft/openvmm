// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sparse-file helpers: marking a file sparse, querying its allocation,
//! block-cloning its extents, enumerating its allocated ranges, and
//! deallocating zeroed ranges.

use crate::windows::chk_status;
use std::fs;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use std::ptr::null_mut;
use windows_sys::Wdk::Storage::FileSystem::FILE_MODE_INFORMATION;
use windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_ALERT;
use windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_NONALERT;
use windows_sys::Wdk::Storage::FileSystem::FileModeInformation;
use windows_sys::Wdk::Storage::FileSystem::NtQueryInformationFile;
use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO;
use windows_sys::Win32::Storage::FileSystem::FileStandardInfo;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Ioctl::DUPLICATE_EXTENTS_DATA;
use windows_sys::Win32::System::Ioctl::FILE_ALLOCATED_RANGE_BUFFER;
use windows_sys::Win32::System::Ioctl::FILE_ZERO_DATA_INFORMATION;
use windows_sys::Win32::System::Ioctl::FSCTL_DUPLICATE_EXTENTS_TO_FILE;
use windows_sys::Win32::System::Ioctl::FSCTL_QUERY_ALLOCATED_RANGES;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_ZERO_DATA;

// This is aligned to every supported ReFS cluster size and remains below its
// exclusive 4-GiB block-clone limit.
const DUPLICATE_EXTENTS_CHUNK_SIZE: u64 = 2 * 1024 * 1024 * 1024;

fn ensure_synchronous(file: &fs::File) -> io::Result<()> {
    let mut mode = FILE_MODE_INFORMATION::default();
    let mut io_status = IO_STATUS_BLOCK::default();
    chk_status(unsafe {
        NtQueryInformationFile(
            file.as_raw_handle(),
            &mut io_status,
            (&raw mut mode).cast(),
            size_of::<FILE_MODE_INFORMATION>() as u32,
            FileModeInformation,
        )
    })?;
    if mode.Mode & (FILE_SYNCHRONOUS_IO_ALERT | FILE_SYNCHRONOUS_IO_NONALERT) == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sparse-file operation requires a synchronous file handle",
        ));
    }
    Ok(())
}

fn duplicate_extent_chunks(length: u64) -> impl Iterator<Item = (u64, u64)> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        if offset == length {
            return None;
        }
        let chunk_length = (length - offset).min(DUPLICATE_EXTENTS_CHUNK_SIZE);
        let chunk = (offset, chunk_length);
        offset += chunk_length;
        Some(chunk)
    })
}

/// Marks a file as sparse.
///
/// The file must have been opened for synchronous I/O.
pub fn set_sparse(file: &fs::File) -> io::Result<()> {
    ensure_synchronous(file)?;
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            ptr::null(),
            0,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Returns the physical allocation charged to a file.
pub fn allocation_size(file: &fs::File) -> io::Result<u64> {
    let mut info = FILE_STANDARD_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&raw mut info).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        u64::try_from(info.AllocationSize).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "negative file allocation size")
        })
    }
}

/// Block-clones all source extents into an independently writable destination.
///
/// `length` must end at a file-system cluster boundary. If a later chunk
/// fails, chunks already cloned into the destination are not rolled back.
/// The destination must have been opened for synchronous I/O.
pub fn duplicate_extents(source: &fs::File, destination: &fs::File, length: u64) -> io::Result<()> {
    ensure_synchronous(destination)?;
    i64::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file is too large"))?;

    for (offset, chunk_length) in duplicate_extent_chunks(length) {
        let offset = i64::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file is too large"))?;
        let byte_count = i64::try_from(chunk_length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file is too large"))?;
        let input = DUPLICATE_EXTENTS_DATA {
            FileHandle: source.as_raw_handle(),
            SourceFileOffset: offset,
            TargetFileOffset: offset,
            ByteCount: byte_count,
        };
        let mut returned = 0;
        let result = unsafe {
            DeviceIoControl(
                destination.as_raw_handle(),
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                (&raw const input).cast(),
                size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn append_allocated_ranges(
    output: &[FILE_ALLOCATED_RANGE_BUFFER],
    length: u64,
    cursor: &mut u64,
    ranges: &mut Vec<(u64, u64)>,
) -> io::Result<()> {
    for range in output {
        let offset = u64::try_from(range.FileOffset).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "negative allocated range offset",
            )
        })?;
        let range_length = u64::try_from(range.Length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "negative allocated range length",
            )
        })?;
        let end = offset.checked_add(range_length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "allocated range overflowed u64")
        })?;
        if offset < *cursor || offset >= length || range_length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "allocated-range query returned an invalid range",
            ));
        }
        let clipped_end = end.min(length);
        ranges.push((offset, clipped_end - offset));
        *cursor = clipped_end;
    }
    Ok(())
}

/// Returns allocated file ranges clipped to `[0, length)`.
///
/// The file must have been opened for synchronous I/O.
pub fn allocated_ranges(file: &fs::File, length: u64) -> io::Result<Vec<(u64, u64)>> {
    const RANGE_CAPACITY: usize = 64;

    ensure_synchronous(file)?;
    let mut ranges = Vec::new();
    let mut cursor = 0_u64;
    while cursor < length {
        let input = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: i64::try_from(cursor).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "file offset exceeds i64")
            })?,
            Length: i64::try_from(length - cursor).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "file length exceeds i64")
            })?,
        };
        let mut output = [FILE_ALLOCATED_RANGE_BUFFER::default(); RANGE_CAPACITY];
        let mut returned = 0_u32;
        let result = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_QUERY_ALLOCATED_RANGES,
                (&raw const input).cast(),
                size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                output.as_mut_ptr().cast(),
                size_of_val(&output) as u32,
                &mut returned,
                null_mut(),
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return Err(error);
            }
        }
        if !(returned as usize).is_multiple_of(size_of::<FILE_ALLOCATED_RANGE_BUFFER>()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "allocated-range query returned a partial record",
            ));
        }
        let count = returned as usize / size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
        if count > output.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "allocated-range query overflowed its buffer",
            ));
        }
        append_allocated_ranges(&output[..count], length, &mut cursor, &mut ranges)?;
        if result != 0 {
            break;
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "allocated-range query made no progress",
            ));
        }
    }
    Ok(ranges)
}

/// Deallocates a range in a sparse file and makes reads return zeroes.
///
/// The file must have been opened for synchronous I/O.
pub fn zero_range(file: &fs::File, start: u64, end: u64) -> io::Result<()> {
    ensure_synchronous(file)?;
    if start >= end {
        return Ok(());
    }
    let input = FILE_ZERO_DATA_INFORMATION {
        FileOffset: i64::try_from(start).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "zero range offset exceeds i64")
        })?,
        BeyondFinalZero: i64::try_from(end).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "zero range end exceeds i64")
        })?,
    };
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_ZERO_DATA,
            (&raw const input).cast(),
            size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use std::path::PathBuf;
    use test_with_tracing::test;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            loop {
                let mut id = [0; 16];
                getrandom::fill(&mut id).unwrap();
                let path = std::env::temp_dir()
                    .join(format!("pal-sparse-{:032x}", u128::from_ne_bytes(id)));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("failed to create test directory: {error}"),
                }
            }
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn duplicate_extent_chunks_are_refs_compatible() {
        const REFS_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
        const CLUSTER_SIZE: u64 = 64 * 1024;
        let length = REFS_LIMIT + 2 * CLUSTER_SIZE;
        let chunks: Vec<_> = duplicate_extent_chunks(length).collect();

        assert_eq!(
            chunks,
            [
                (0, 2 * 1024 * 1024 * 1024),
                (2 * 1024 * 1024 * 1024, 2 * 1024 * 1024 * 1024),
                (4 * 1024 * 1024 * 1024, 2 * CLUSTER_SIZE),
            ]
        );
        assert!(
            chunks
                .iter()
                .all(|(offset, length)| offset % CLUSTER_SIZE == 0
                    && length % CLUSTER_SIZE == 0
                    && *length < REFS_LIMIT)
        );
    }

    #[test]
    fn allocated_range_is_clipped_to_requested_length() {
        let output = [FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: 0,
            Length: 4096,
        }];
        let mut cursor = 0;
        let mut ranges = Vec::new();

        append_allocated_ranges(&output, 1000, &mut cursor, &mut ranges).unwrap();

        assert_eq!(ranges, [(0, 1000)]);
        assert_eq!(cursor, 1000);
    }

    #[test]
    fn allocated_range_validation_rejects_empty_and_out_of_order_ranges() {
        let empty = [FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: 0,
            Length: 0,
        }];
        let mut cursor = 0;
        let mut ranges = Vec::new();
        assert_eq!(
            append_allocated_ranges(&empty, 1000, &mut cursor, &mut ranges)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let out_of_order = [FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: 99,
            Length: 10,
        }];
        cursor = 100;
        assert_eq!(
            append_allocated_ranges(&out_of_order, 1000, &mut cursor, &mut ranges)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn overlapped_handles_are_rejected() {
        let root = TestDirectory::new();
        let source = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join("source"))
            .unwrap();
        let destination = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(root.join("destination"))
            .unwrap();

        assert_eq!(
            set_sparse(&destination).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            duplicate_extents(&source, &destination, 0)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            allocated_ranges(&destination, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            zero_range(&destination, 0, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
