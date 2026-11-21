// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows physical disk (for example `\\.\PhysicalDriveN`) geometry queries.

// UNSAFETY: Calling DeviceIoControl and GetFileInformationByHandleEx with
// properly sized input and output buffers.
#![expect(unsafe_code)]

use std::fs;
use std::io;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION;
use windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
use windows_sys::Win32::Storage::FileSystem::FILE_ALIGNMENT_INFO;
use windows_sys::Win32::Storage::FileSystem::FileAlignmentInfo;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::DISK_GEOMETRY;
use windows_sys::Win32::System::Ioctl::GET_LENGTH_INFORMATION;
use windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_DRIVE_GEOMETRY;
use windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_LENGTH_INFO;
use windows_sys::Win32::System::Ioctl::IOCTL_STORAGE_QUERY_PROPERTY;
use windows_sys::Win32::System::Ioctl::PropertyStandardQuery;
use windows_sys::Win32::System::Ioctl::STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR;
use windows_sys::Win32::System::Ioctl::STORAGE_PROPERTY_QUERY;
use windows_sys::Win32::System::Ioctl::StorageAccessAlignmentProperty;

/// Size and sector geometry of a physical disk.
pub(crate) struct DiskGeometry {
    pub disk_size: u64,
    pub sector_size: u32,
    pub physical_sector_size: u32,
}

fn ioctl<I, O: Default>(file: &fs::File, code: u32, input: Option<&I>) -> io::Result<O> {
    let mut output = O::default();
    let mut returned = 0;
    let (in_ptr, in_len) = input.map_or((std::ptr::null(), 0), |i| {
        (std::ptr::from_ref(i).cast(), size_of::<I>() as u32)
    });
    // SAFETY: the input and output pointers are valid for the specified
    // lengths for the duration of the synchronous call.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            in_ptr,
            in_len,
            std::ptr::from_mut(&mut output).cast(),
            size_of::<O>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if (returned as usize) < size_of::<O>() {
        return Err(io::Error::from_raw_os_error(ERROR_NOT_SUPPORTED as i32));
    }
    Ok(output)
}

/// Queries the size and sector sizes of a physical disk.
///
/// Fails if the device is not a disk or reports an unusable geometry.
pub(crate) fn query_disk_geometry(file: &fs::File) -> io::Result<DiskGeometry> {
    let length: GET_LENGTH_INFORMATION = ioctl::<(), _>(file, IOCTL_DISK_GET_LENGTH_INFO, None)?;
    let geometry: DISK_GEOMETRY = ioctl::<(), _>(file, IOCTL_DISK_GET_DRIVE_GEOMETRY, None)?;
    let sector_size = geometry.BytesPerSector;
    if !sector_size.is_power_of_two() || sector_size < 512 {
        return Err(invalid_geometry(format!(
            "invalid logical sector size {sector_size}"
        )));
    }
    let disk_size = length.Length as u64;
    if !disk_size.is_multiple_of(sector_size as u64) {
        return Err(invalid_geometry(format!(
            "disk size {disk_size} is not a multiple of the sector size {sector_size}"
        )));
    }

    // Not all devices report a physical sector size. It is advisory, so use
    // the logical sector size when the device does not report one.
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageAccessAlignmentProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let physical_sector_size = match ioctl::<_, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>(
        file,
        IOCTL_STORAGE_QUERY_PROPERTY,
        Some(&query),
    ) {
        Ok(alignment) => alignment.BytesPerPhysicalSector,
        Err(err) if is_unsupported(&err) => sector_size,
        Err(err) => return Err(err),
    };
    if !physical_sector_size.is_power_of_two() || physical_sector_size < sector_size {
        return Err(invalid_geometry(format!(
            "invalid physical sector size {physical_sector_size} for logical sector size {sector_size}"
        )));
    }
    Ok(DiskGeometry {
        disk_size,
        sector_size,
        physical_sector_size,
    })
}

/// Returns the buffer address alignment, in bytes, that the device enforces
/// for I/O on `file`.
///
/// Windows recommends aligning unbuffered I/O buffers to the physical sector
/// size, but only rejects I/O whose buffers violate the device's alignment
/// requirement, which is often smaller.
pub(crate) fn query_buffer_alignment(file: &fs::File) -> io::Result<u32> {
    let mut info = FILE_ALIGNMENT_INFO::default();
    // SAFETY: the output pointer is valid for the specified length for the
    // duration of the call.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAlignmentInfo,
            std::ptr::from_mut(&mut info).cast(),
            size_of::<FILE_ALIGNMENT_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // The requirement is reported as an address mask, such as 511 for
    // 512-byte alignment.
    info.AlignmentRequirement
        .checked_add(1)
        .filter(|alignment| alignment.is_power_of_two())
        .ok_or_else(|| {
            invalid_geometry(format!(
                "invalid buffer alignment requirement {:#x}",
                info.AlignmentRequirement
            ))
        })
}

fn invalid_geometry(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn is_unsupported(err: &io::Error) -> bool {
    err.raw_os_error().is_some_and(|code| {
        code == ERROR_NOT_SUPPORTED as i32 || code == ERROR_INVALID_FUNCTION as i32
    })
}
