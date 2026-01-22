// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! FFI wrapper to provide access to VMGS functions as a cdylib

// UNSAFETY: Exporting no_mangle extern C functions and dealing with the raw
// pointers necessary to do so.
#![expect(unsafe_code)]
#![expect(missing_docs)]

use core::slice;
use disk_backend::Disk;
use disk_vhd1::Vhd1Disk;
use futures::executor::block_on;
use std::ffi::CStr;
use std::ffi::c_char;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use vmgs::EncryptionAlgorithm;
use vmgs::Vmgs;
use vmgs_format::FileId;
use vmgs_format::VMGS_BYTES_PER_BLOCK;
use vmgs_format::VMGS_DEFAULT_CAPACITY;

#[repr(u32)]
pub enum VmgsError {
    Ok = 0,
    NullParam = 1,
    CantOpenFile = 2,
    CantReadFile = 3,
    FileDisk = 4,
    InvalidBufSize = 5,
    InvalidFileID = 6,
    InvalidFileSize = 7,
    InvalidString = 8,
    InvalidVmgs = 9,
    FileInfoNotAllocated = 10,
    DecryptionFailed = 11,
    EncryptionFailed = 12,
    WriteFailed = 13,
    FileExists = 14,
}

/// Read the contents of a `FileId` in a VMGS file
///
/// # Safety
///
/// `file_path` must point to a valid null-terminated utf-8 string.
/// `in_len` must be the size of `in_buf` in bytes and match the value returned from query_size_vmgs
// SAFETY: In this library this function name is unique.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn read_vmgs(
    file_path: *const c_char,
    file_id: FileId,
    encryption_key: *const c_char,
    use_encryption: bool,
    in_buf: *mut u8,
    in_len: u64,
) -> VmgsError {
    // SAFETY: all passed pointers are checked to be null-terminated and nonnull before access
    let file_path = unsafe {
        if file_path.is_null() {
            return VmgsError::NullParam;
        }
        match CStr::from_ptr(file_path).to_str() {
            Ok(res) => res,
            Err(_res) => return VmgsError::InvalidString,
        }
    };

    // SAFETY: `in_buf` is a pointer to a u8 array of size `in_len`
    let buf = unsafe {
        if in_buf.is_null() {
            return VmgsError::NullParam;
        }
        slice::from_raw_parts_mut(in_buf, in_len as usize)
    };

    let key = match use_encryption {
        // SAFETY: `encryption_key` must be null-terminated and nonnull if using encryption
        true => unsafe {
            if encryption_key.is_null() {
                return VmgsError::NullParam;
            }
            match CStr::from_ptr(encryption_key).to_str() {
                Ok(res) => Some(res.as_bytes()),
                Err(_res) => return VmgsError::InvalidString,
            }
        },
        false => None,
    };

    let data = match block_on(do_read(file_path, file_id, key)) {
        Ok(value) => value,
        Err(value) => return value,
    };

    if data.len() != in_len as usize {
        return VmgsError::InvalidBufSize;
    }

    buf.copy_from_slice(&data);

    VmgsError::Ok
}

fn open_disk(file_path: &str, read_only: bool) -> Result<Disk, VmgsError> {
    let file = File::options()
        .read(true)
        .write(!read_only)
        .open(file_path)
        .map_err(|_| VmgsError::FileDisk)?;

    let disk = Vhd1Disk::open_fixed(file, read_only).map_err(|_| VmgsError::FileDisk)?;
    Disk::new(disk).map_err(|_| VmgsError::FileDisk)
}

async fn do_read(
    file_path: &str,
    file_id: FileId,
    key: Option<&[u8]>,
) -> Result<Vec<u8>, VmgsError> {
    let mut vmgs = Vmgs::open(open_disk(file_path, true)?, None)
        .await
        .map_err(|_| VmgsError::InvalidVmgs)?;

    let info = vmgs
        .get_file_info(file_id)
        .map_err(|_| VmgsError::FileInfoNotAllocated)?;
    let data_size = info.valid_bytes;

    if let Some(encryption_key) = key {
        vmgs.unlock_with_encryption_key(encryption_key)
            .await
            .map_err(|_| VmgsError::DecryptionFailed)?;
    }
    let data = vmgs
        .read_file(file_id)
        .await
        .map_err(|_| VmgsError::CantReadFile)?;
    if data.len() != data_size as usize {
        return Err(VmgsError::CantReadFile);
    }

    Ok(data)
}

/// Write from a data file to a `FileId` in a VMGS file
///
/// # Safety
///
/// `file_path` and `data_path` must point to valid null-terminated utf-8 strings.
/// `encryption_key` must be null-terminated and nonnull if using encryption
// SAFETY: In this library this function name is unique.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn write_vmgs(
    file_path: *const c_char,
    data_path: *const c_char,
    file_id: FileId,
    encryption_key: *const c_char,
    use_encryption: bool,
) -> VmgsError {
    // SAFETY: all passed pointers are checked to be null-terminated and nonnull before access
    let (file_path, data_path) = unsafe {
        if file_path.is_null() || data_path.is_null() {
            return VmgsError::NullParam;
        }

        let file = CStr::from_ptr(file_path).to_str();
        let data = CStr::from_ptr(data_path).to_str();
        match (file, data) {
            (Ok(f), Ok(d)) => (f, d),
            _ => return VmgsError::InvalidString,
        }
    };

    let key = match use_encryption {
        true => {
            if encryption_key.is_null() {
                return VmgsError::NullParam;
            }
            // SAFETY: `encryption_key` guaranteed by caller to be null-terminated and nonnull if using encryption
            match unsafe { CStr::from_ptr(encryption_key) }.to_str() {
                Ok(res) => Some(res.as_bytes()),
                Err(_res) => return VmgsError::InvalidString,
            }
        }
        false => None,
    };
    match block_on(do_write(file_path, data_path, key, file_id)) {
        Ok(_) => VmgsError::Ok,
        Err(ret) => ret,
    }
}

async fn do_write(
    file_path: &str,
    data_path: &str,
    key: Option<&[u8]>,
    file_id: FileId,
) -> Result<(), VmgsError> {
    let mut buf = Vec::new();

    let mut file = File::open(data_path).map_err(|_| VmgsError::CantOpenFile)?;

    // manually allow, since we want to differentiate between the file not being
    // accessible, and a read operation failing
    #[expect(clippy::verbose_file_reads)]
    file.read_to_end(&mut buf)
        .map_err(|_| VmgsError::CantReadFile)?;

    let mut vmgs = Vmgs::open(open_disk(file_path, false)?, None)
        .await
        .map_err(|_| VmgsError::InvalidVmgs)?;

    if let Some(encryption_key) = key {
        vmgs.unlock_with_encryption_key(encryption_key)
            .await
            .map_err(|_| VmgsError::DecryptionFailed)?;

        vmgs.write_file_encrypted(file_id, &buf)
            .await
            .map_err(|_| VmgsError::WriteFailed)?;
    } else {
        vmgs.write_file(file_id, &buf)
            .await
            .map_err(|_| VmgsError::WriteFailed)?;
    }
    Ok(())
}

/// Create a VMGS file
///
/// If `file_size` is zero, default size of 4MB is used
/// Creation will fail if `path` already exists unless the `force_create` flag is set
/// The VMGS file can optionally be encrypted by setting the `use_encryption` flag and specifying
/// an encryption key
///
/// # Safety
///
/// `path` must point to a valid null-terminated utf-8 string.
// SAFETY: In this library this function name is unique.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn create_vmgs(
    path: *const c_char,
    file_size: u64,
    force_create: bool,
    encryption_key: *const c_char,
    use_encryption: bool,
) -> VmgsError {
    // SAFETY: all passed pointers are checked to be null-terminated and nonnull before access
    let path = unsafe {
        if path.is_null() {
            return VmgsError::NullParam;
        }
        match CStr::from_ptr(path).to_str() {
            Ok(res) => res,
            Err(_res) => return VmgsError::InvalidString,
        }
    };
    let key = match use_encryption {
        // SAFETY: `encryption_key` must be null-terminated and nonnull if using encryption
        true => unsafe {
            if encryption_key.is_null() {
                return VmgsError::NullParam;
            }
            match CStr::from_ptr(encryption_key).to_str() {
                Ok(res) => Some(res.as_bytes()),
                Err(_res) => return VmgsError::InvalidString,
            }
        },
        false => None,
    };
    let file_path = PathBuf::from(path);
    match block_on(do_create(file_path, file_size, force_create, key)) {
        Ok(_) => VmgsError::Ok,
        Err(res) => res,
    }
}

async fn do_create(
    file_path: impl AsRef<Path>,
    file_size: u64,
    force_create: bool,
    key: Option<&[u8]>,
) -> Result<(), VmgsError> {
    let mut overwrite_existing_file = false;

    // Make sure that a file does not already exist.
    if Path::new(file_path.as_ref()).exists() {
        if force_create {
            overwrite_existing_file = true;
        } else {
            return Err(VmgsError::FileExists);
        }
    }

    if file_size != 0 && file_size < (VMGS_BYTES_PER_BLOCK * 4) as u64
        || !file_size.is_multiple_of(512)
    {
        return Err(VmgsError::InvalidFileSize);
    }
    let file_size = if file_size == 0 {
        VMGS_DEFAULT_CAPACITY
    } else {
        file_size
    };
    let file = File::options()
        .create(true)
        .create_new(!overwrite_existing_file)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&file_path)
        .map_err(|_| VmgsError::FileDisk)?;

    file.set_len(file_size).map_err(|_| VmgsError::FileDisk)?;

    Vhd1Disk::make_fixed(&file).map_err(|_| VmgsError::FileDisk)?;

    let disk = Vhd1Disk::open_fixed(file, false).map_err(|_| VmgsError::FileDisk)?;

    let mut vmgs = Vmgs::format_new(Disk::new(disk).map_err(|_| VmgsError::FileDisk)?, None)
        .await
        .map_err(|_| VmgsError::InvalidVmgs)?;

    if let Some(encryption_key) = key {
        vmgs.update_encryption_key(encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .map_err(|_| VmgsError::EncryptionFailed)?;
    }
    Ok(())
}

/// Get the size of a `FileId` in a VMGS file
///
/// # Safety
///
/// `path` pointer must point to a valid, null-terminated utf-8 string.
/// `out_size` pointer must be nonnull
// SAFETY: In this library this function name is unique.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn query_size_vmgs(
    path: *const c_char,
    file_id: FileId,
    out_size: *mut u64,
) -> VmgsError {
    // SAFETY: all passed pointers are checked to be null-terminated and nonnull before access
    let path = unsafe {
        if path.is_null() || out_size.is_null() {
            return VmgsError::NullParam;
        }
        match CStr::from_ptr(path).to_str() {
            Ok(res) => res,
            Err(_res) => return VmgsError::InvalidString,
        }
    };

    let size = match block_on(do_query_size(path, file_id)) {
        Ok(value) => value,
        Err(value) => return value,
    };

    // SAFETY: `out_size` is not null
    unsafe { *out_size = size }
    VmgsError::Ok
}

async fn do_query_size(file_path: &str, file_id: FileId) -> Result<u64, VmgsError> {
    let vmgs = Vmgs::open(open_disk(file_path, true)?, None)
        .await
        .map_err(|_| VmgsError::InvalidVmgs)?;

    let info = vmgs
        .get_file_info(file_id)
        .map_err(|_| VmgsError::FileInfoNotAllocated)?;

    Ok(info.valid_bytes)
}
