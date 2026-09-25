// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sparse-file helpers: marking a file sparse, querying its allocation,
//! block-cloning its extents, enumerating its allocated ranges, and
//! deallocating zeroed ranges.

use std::fs;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO;
use windows_sys::Win32::Storage::FileSystem::FileStandardInfo;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::DUPLICATE_EXTENTS_DATA;
use windows_sys::Win32::System::Ioctl::FILE_ALLOCATED_RANGE_BUFFER;
use windows_sys::Win32::System::Ioctl::FILE_ZERO_DATA_INFORMATION;
use windows_sys::Win32::System::Ioctl::FSCTL_DUPLICATE_EXTENTS_TO_FILE;
use windows_sys::Win32::System::Ioctl::FSCTL_QUERY_ALLOCATED_RANGES;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_ZERO_DATA;

/// Marks a file as sparse.
pub fn set_sparse(file: &fs::File) -> io::Result<()> {
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
pub fn duplicate_extents(source: &fs::File, destination: &fs::File, length: u64) -> io::Result<()> {
    let byte_count = i64::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file is too large"))?;
    let input = DUPLICATE_EXTENTS_DATA {
        FileHandle: source.as_raw_handle(),
        SourceFileOffset: 0,
        TargetFileOffset: 0,
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
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
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
        for range in &output[..count] {
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
            if offset < cursor || range_length == 0 || end > length {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "allocated-range query returned an invalid range",
                ));
            }
            ranges.push((offset, range_length));
            cursor = end;
        }
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
