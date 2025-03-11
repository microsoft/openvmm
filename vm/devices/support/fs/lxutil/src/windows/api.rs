// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(
    non_snake_case,
    non_camel_case_types,
    dead_code,
    clippy::upper_case_acronyms
)]

use ntapi::ntioapi;
use std::ffi;
use std::os::windows::prelude::*;
use winapi::shared::basetsd;
use winapi::shared::ntdef;
use windows::Win32::Storage::FileSystem;

// Maximum size needed for an EA buffer containing all metadata fields.
pub const LX_UTIL_FS_METADATA_EA_BUFFER_SIZE: usize = 84;

// NT constants missing from ntapi.
pub const LX_FILE_METADATA_HAS_UID: ntdef::ULONG = 0x1;
pub const LX_FILE_METADATA_HAS_GID: ntdef::ULONG = 0x2;
pub const LX_FILE_METADATA_HAS_MODE: ntdef::ULONG = 0x4;
pub const IO_REPARSE_TAG_LX_SYMLINK: ntdef::ULONG = 0xA000001D;
pub const FILE_CS_FLAG_CASE_SENSITIVE_DIR: ntdef::ULONG = 0x1;

// Fallback modes.
pub const LX_DRVFS_DISABLE_NONE: ntdef::ULONG = 0;

// Flags for converting attributes.
pub const LX_UTIL_FS_CALLER_HAS_TRAVERSE_PRIVILEGE: ntdef::ULONG = 0x1;

// Flags for listing extended attributes.
pub const LX_UTIL_XATTR_LIST_CASE_SENSITIVE_DIR: ntdef::ULONG = 0x1;

// Size of PE header signature.
pub const LX_UTIL_PE_HEADER_SIZE: ntdef::ULONG = 2;

#[allow(non_camel_case_types, non_snake_case, unused)]
#[repr(C)]
pub struct FILE_ID_64_EXTD_DIR_INFORMATION {
    pub NextEntryOffset: u32,
    pub FileIndex: u32,
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub EndOfFile: i64,
    pub AllocationSize: i64,
    pub FileAttributes: u32,
    pub FileNameLength: u32,
    pub EaSize: u32,
    pub ReparsePointTag: u32,
    pub FileId: i64,
    pub FileName: [u16; 1],
}

#[allow(non_camel_case_types, non_snake_case, unused)]
#[repr(C)]
pub struct FILE_ID_ALL_EXTD_DIR_INFORMATION {
    pub NextEntryOffset: u32,
    pub FileIndex: u32,
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub EndOfFile: i64,
    pub AllocationSize: i64,
    pub FileAttributes: u32,
    pub FileNameLength: u32,
    pub EaSize: u32,
    pub ReparsePointTag: u32,
    pub FileId: i64,
    pub FileId128: FileSystem::FILE_ID_128,
    pub FileName: [u16; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SymlinkData {
    version: u32,
    // 12 bytes to make this struct the same size as REPARSE_DATA_BUFFER
    target: [u8; 12],
}

#[repr(C)]
pub struct LX_UTIL_BUFFER {
    pub Buffer: ntdef::PVOID,
    pub Size: usize,
    pub Flags: ntdef::ULONG,
}

impl Default for LX_UTIL_BUFFER {
    fn default() -> Self {
        Self {
            Buffer: std::ptr::null_mut(),
            Size: 0,
            Flags: 0,
        }
    }
}

/// Ensures lxutil.dll has been loaded successfully. If this is not called,
/// then the LxUtil* functions may panic if the DLL cannot be loaded.
pub fn delay_load_lxutil() -> std::io::Result<()> {
    get_module().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    Ok(())
}

pal::delayload!("lxutil.dll" {
    pub fn LxUtilFsCreateLinkReparseBuffer(
        link_target: &ntdef::ANSI_STRING,
        size: &mut ntdef::USHORT,
    ) -> ntioapi::PREPARSE_DATA_BUFFER;

    pub fn LxUtilFsCreateMetadataEaBuffer(
        uid: lx::uid_t,
        gid: lx::gid_t,
        mode: lx::mode_t,
        device_id: lx::dev_t,
        ea_buffer: *mut ffi::c_void,
    ) -> ntdef::ULONG;

    pub fn LxUtilFsCreateNtLinkReparseBuffer(
        link_target: *const ntdef::UNICODE_STRING,
        flags: ntdef::ULONG,
        size: &mut ntdef::USHORT,
    ) -> ntioapi::PREPARSE_DATA_BUFFER;

    pub fn LxUtilFsGetFileSystemBlockSize(handle: RawHandle) -> ntdef::ULONG;

    pub fn LxUtilFsGetLxFileSystemAttributes(
        handle: RawHandle,
        fs_type: usize,
        stat_fs: &mut lx::StatFs,
    ) -> i32;

    pub fn LxUtilFsIsAppExecLink(attributes: ntdef::ULONG, reparse_tag: ntdef::ULONG) -> ntdef::BOOLEAN;

    pub fn LxUtilFsReadAppExecLink(offset:u64, buffer: ntdef::PVOID, buffer_size: basetsd::SIZE_T) -> basetsd::SIZE_T;

    pub fn LxUtilFsSetTimes(
        handle: RawHandle,
        accessed_time: &lx::Timespec,
        modified_time: &lx::Timespec,
        change_time: &lx::Timespec,
    ) -> i32;

    pub fn LxUtilFsTruncate(handle: RawHandle, size: ntdef::ULONGLONG) -> i32;

    pub fn LxUtilFsUpdateLxAttributes(
        handle: RawHandle,
        uid: lx::uid_t,
        gid: lx::gid_t,
        mode: lx::mode_t,
    ) -> i32;

    pub fn LxUtilSymlinkRead(link_file: RawHandle, link_target: *mut ntdef::UNICODE_STRING) -> i32;

    pub fn LxUtilXattrGet(
        handle: RawHandle,
        name: &ntdef::ANSI_STRING,
        value: &mut LX_UTIL_BUFFER,
    ) -> isize;

    pub fn LxUtilXattrGetSystem(
        handle: RawHandle,
        name: &ntdef::ANSI_STRING,
        value: &mut LX_UTIL_BUFFER,
    ) -> isize;

    pub fn LxUtilXattrList(handle: RawHandle, flags: ntdef::ULONG, list: *mut *const u8) -> isize;

    pub fn LxUtilXattrRemove(handle: RawHandle, name: &ntdef::ANSI_STRING) -> i32;

    pub fn LxUtilXattrSet(
        handle: RawHandle,
        name: &ntdef::ANSI_STRING,
        value: &LX_UTIL_BUFFER,
        flags: i32,
    ) -> i32;

    pub fn LxUtilXattrSetSystem(
        handle: RawHandle,
        name: &ntdef::ANSI_STRING,
        value: &LX_UTIL_BUFFER,
        flags: i32,
    ) -> i32;
});
