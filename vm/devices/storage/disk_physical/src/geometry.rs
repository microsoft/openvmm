// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows physical disk (for example `\\.\PhysicalDriveN`) geometry queries.

// UNSAFETY: Calling DeviceIoControl with properly sized input and output buffers.
#![expect(unsafe_code)]

use std::fs;
use std::io;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::GET_LENGTH_INFORMATION;
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
pub(crate) fn query_disk_geometry(file: &fs::File) -> io::Result<DiskGeometry> {
    let length: GET_LENGTH_INFORMATION = ioctl::<(), _>(file, IOCTL_DISK_GET_LENGTH_INFO, None)?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageAccessAlignmentProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let (sector_size, physical_sector_size) = match ioctl::<_, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>(
        file,
        IOCTL_STORAGE_QUERY_PROPERTY,
        Some(&query),
    ) {
        Ok(alignment) => (
            alignment.BytesPerLogicalSector,
            alignment.BytesPerPhysicalSector,
        ),
        Err(_) => (512, 512),
    };
    let sector_size = if sector_size.is_power_of_two() && sector_size >= 512 {
        sector_size
    } else {
        512
    };
    let physical_sector_size =
        if physical_sector_size.is_power_of_two() && physical_sector_size >= sector_size {
            physical_sector_size
        } else {
            sector_size
        };
    Ok(DiskGeometry {
        disk_size: length.Length as u64,
        sector_size,
        physical_sector_size,
    })
}
