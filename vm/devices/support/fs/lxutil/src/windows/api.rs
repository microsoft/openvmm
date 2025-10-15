// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(non_snake_case)]

use std::os::windows::prelude::*;
use winapi::shared::ntdef;

#[repr(C)]
pub struct LX_UTIL_BUFFER {
    pub Buffer: ntdef::PVOID,
    pub Size: usize,
    pub Flags: ntdef::ULONG,
}

/// Ensures lxutil.dll has been loaded successfully. If this is not called,
/// then the LxUtil* functions may panic if the DLL cannot be loaded.
pub fn delay_load_lxutil() -> std::io::Result<()> {
    get_module().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    Ok(())
}

pal::delayload!("lxutil.dll" {
    pub fn LxUtilXattrRemove(handle: RawHandle, name: &ntdef::ANSI_STRING) -> i32;

    pub fn LxUtilXattrSetSystem(
        handle: RawHandle,
        name: &ntdef::ANSI_STRING,
        value: &LX_UTIL_BUFFER,
        flags: i32,
    ) -> i32;
});
