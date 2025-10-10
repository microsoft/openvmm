// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(non_snake_case)]

use std::os::windows::prelude::*;
use winapi::shared::ntdef;

// Flags for listing extended attributes.
pub const LX_UTIL_XATTR_LIST_CASE_SENSITIVE_DIR: ntdef::ULONG = 0x1;

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
