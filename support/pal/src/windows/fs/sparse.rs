// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sparse-file helpers: marking a file sparse, querying its allocation,
//! block-cloning its extents, enumerating its allocated ranges, and
//! deallocating zeroed ranges.
//! Device controls wait for event-backed completion when used with an
//! overlapped handle.

use crate::windows::Overlapped;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::ptr;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO;
use windows_sys::Win32::Storage::FileSystem::FileStandardInfo;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::IO::GetOverlappedResult;
use windows_sys::Win32::System::Ioctl::DUPLICATE_EXTENTS_DATA;
use windows_sys::Win32::System::Ioctl::FILE_ALLOCATED_RANGE_BUFFER;
use windows_sys::Win32::System::Ioctl::FILE_ZERO_DATA_INFORMATION;
use windows_sys::Win32::System::Ioctl::FSCTL_DUPLICATE_EXTENTS_TO_FILE;
use windows_sys::Win32::System::Ioctl::FSCTL_QUERY_ALLOCATED_RANGES;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_ZERO_DATA;
use windows_sys::Win32::System::Threading::CreateEventW;

// This is aligned to every supported ReFS cluster size and remains below its
// exclusive 4-GiB block-clone limit.
const DUPLICATE_EXTENTS_CHUNK_SIZE: u64 = 2 * 1024 * 1024 * 1024;

struct DeviceIoControlError {
    error: io::Error,
    returned: u32,
}

/// # Safety
///
/// `input` and `output` must describe buffers valid for their corresponding
/// sizes. This function waits for pending I/O before returning.
unsafe fn device_io_control(
    file: &fs::File,
    control_code: u32,
    input: *const c_void,
    input_size: u32,
    output: *mut c_void,
    output_size: u32,
) -> Result<u32, DeviceIoControlError> {
    // SAFETY: all CreateEventW pointer arguments are null as documented.
    let event = unsafe {
        let event = CreateEventW(null_mut(), true.into(), false.into(), ptr::null());
        if event.is_null() {
            return Err(DeviceIoControlError {
                error: io::Error::last_os_error(),
                returned: 0,
            });
        }
        OwnedHandle::from_raw_handle(event)
    };
    let mut overlapped = Overlapped::new();
    overlapped.set_event(event.as_raw_handle());
    let mut returned = 0;
    // SAFETY: the caller guarantees the buffer contracts, and `overlapped`
    // and its event remain live until completion below.
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            control_code,
            input,
            input_size,
            output,
            output_size,
            &mut returned,
            overlapped.as_ptr(),
        )
    };
    if result != 0 {
        return Ok(returned);
    }

    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
        return Err(DeviceIoControlError { error, returned });
    }
    // SAFETY: this is the same live file and event-backed OVERLAPPED passed
    // to the pending operation above.
    if unsafe {
        GetOverlappedResult(
            file.as_raw_handle(),
            overlapped.as_ptr(),
            &mut returned,
            true.into(),
        )
    } == 0
    {
        return Err(DeviceIoControlError {
            error: io::Error::last_os_error(),
            returned,
        });
    }
    Ok(returned)
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
pub fn set_sparse(file: &fs::File) -> io::Result<()> {
    // SAFETY: this request has no input or output buffer.
    unsafe { device_io_control(file, FSCTL_SET_SPARSE, ptr::null(), 0, null_mut(), 0) }
        .map(|_| ())
        .map_err(|error| error.error)
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
pub fn duplicate_extents(source: &fs::File, destination: &fs::File, length: u64) -> io::Result<()> {
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
        // SAFETY: `input` remains live, and the helper waits for completion.
        unsafe {
            device_io_control(
                destination,
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                (&raw const input).cast(),
                size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                null_mut(),
                0,
            )
        }
        .map_err(|error| error.error)?;
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
pub fn allocated_ranges(file: &fs::File, length: u64) -> io::Result<Vec<(u64, u64)>> {
    const RANGE_CAPACITY: usize = 64;

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
        // SAFETY: both buffers remain live, and the helper waits for
        // completion before returning.
        let (result, returned) = match unsafe {
            device_io_control(
                file,
                FSCTL_QUERY_ALLOCATED_RANGES,
                (&raw const input).cast(),
                size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                output.as_mut_ptr().cast(),
                size_of_val(&output) as u32,
            )
        } {
            Ok(returned) => (true, returned),
            Err(error) => {
                if error.error.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                    return Err(error.error);
                }
                (false, error.returned)
            }
        };
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
        if result {
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
pub fn zero_range(file: &fs::File, start: u64, end: u64) -> io::Result<()> {
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
    // SAFETY: `input` remains live, and the helper waits for completion.
    unsafe {
        device_io_control(
            file,
            FSCTL_SET_ZERO_DATA,
            (&raw const input).cast(),
            size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            null_mut(),
            0,
        )
    }
    .map(|_| ())
    .map_err(|error| error.error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::Write;
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
    fn sparse_controls_track_and_zero_allocated_ranges() {
        const FILE_LENGTH: u64 = 16 * 1024 * 1024;
        const EXTENT_LENGTH: u64 = 64 * 1024;
        const FIRST_OFFSET: u64 = 64 * 1024;
        const SECOND_OFFSET: u64 = 8 * 1024 * 1024;

        let root = TestDirectory::new();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join("sparse"))
            .unwrap();
        set_sparse(&file).unwrap();
        file.set_len(FILE_LENGTH).unwrap();

        let first_contents = vec![0x11; EXTENT_LENGTH as usize];
        let second_contents = vec![0x22; EXTENT_LENGTH as usize];
        file.seek(SeekFrom::Start(FIRST_OFFSET)).unwrap();
        file.write_all(&first_contents).unwrap();
        file.seek(SeekFrom::Start(SECOND_OFFSET)).unwrap();
        file.write_all(&second_contents).unwrap();
        file.sync_all().unwrap();

        let covers = |ranges: &[(u64, u64)], offset| {
            ranges.iter().any(|(start, length)| {
                *start <= offset && start.checked_add(*length).is_some_and(|end| offset < end)
            })
        };
        let ranges = allocated_ranges(&file, FILE_LENGTH).unwrap();
        assert!(covers(&ranges, FIRST_OFFSET));
        assert!(covers(&ranges, SECOND_OFFSET));
        let allocated = allocation_size(&file).unwrap();
        assert!(allocated > 0);
        assert!(allocated < FILE_LENGTH);

        zero_range(&file, FIRST_OFFSET, FIRST_OFFSET + EXTENT_LENGTH).unwrap();
        file.sync_all().unwrap();
        let ranges = allocated_ranges(&file, FILE_LENGTH).unwrap();
        assert!(!covers(&ranges, FIRST_OFFSET));
        assert!(covers(&ranges, SECOND_OFFSET));

        let mut contents = vec![0xff; EXTENT_LENGTH as usize];
        file.seek(SeekFrom::Start(FIRST_OFFSET)).unwrap();
        file.read_exact(&mut contents).unwrap();
        assert_eq!(contents, vec![0; EXTENT_LENGTH as usize]);
    }

    #[test]
    fn overlapped_handles_are_supported() {
        const FILE_LENGTH: u64 = 64 * 1024;

        let root = TestDirectory::new();
        let source = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join("source"))
            .unwrap();
        let destination_path = root.join("destination");
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&destination_path)
            .unwrap()
            .set_len(FILE_LENGTH)
            .unwrap();
        let destination = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(destination_path)
            .unwrap();

        set_sparse(&destination).unwrap();
        allocated_ranges(&destination, FILE_LENGTH).unwrap();
        zero_range(&destination, 0, FILE_LENGTH).unwrap();
        assert!(
            allocated_ranges(&destination, FILE_LENGTH)
                .unwrap()
                .is_empty()
        );
        duplicate_extents(&source, &destination, 0).unwrap();
    }
}
