// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Helper to create a CString from a utf-8 vector and return an lx::Result if it fails.
#[cfg(target_os = "linux")]
pub fn create_cstr(value: impl Into<Vec<u8>>) -> lx::Result<std::ffi::CString> {
    std::ffi::CString::new(value).map_err(|_| lx::Error::EINVAL)
}
