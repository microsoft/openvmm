// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for macOS operations, used by multiple algorithms.

#![cfg(all(native, target_os = "macos"))]

use std::ffi::c_void;
use std::fmt;

type CFStringRef = *const c_void;
type CFIndex = isize;

/// kCFStringEncodingUTF8
const K_CF_STRING_ENCODING_UTF8: u32 = 0x08000100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFStringGetLength(the_string: CFStringRef) -> CFIndex;
    fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut u8,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> u8;
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecCopyErrorMessageString(status: OsStatusCode, reserved: *const c_void) -> CFStringRef;
}

/// An OSStatus code from a Security.framework or CoreFoundation API.
///
/// Displays a human-readable message via `SecCopyErrorMessageString` when
/// available, falling back to just the numeric code.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct OsStatusCode(pub i32);

impl OsStatusCode {
    pub const SUCCESS: Self = Self(0);

    pub fn success(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for OsStatusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: SecCopyErrorMessageString is safe with any i32 value.
        let cf_str = unsafe { SecCopyErrorMessageString(*self, std::ptr::null()) };
        if cf_str.is_null() {
            return write!(f, "OSStatus {}", self.0);
        }

        // SAFETY: cf_str is a valid CFStringRef.
        let len = unsafe { CFStringGetLength(cf_str) };
        // UTF-8 can use up to 4 bytes per character; +1 for null terminator.
        let buf_size = len * 4 + 1;
        let mut buf = vec![0u8; buf_size as usize];
        // SAFETY: cf_str is valid, buf is large enough.
        let ok = unsafe {
            CFStringGetCString(
                cf_str,
                buf.as_mut_ptr(),
                buf_size,
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        // SAFETY: cf_str is a non-null CFStringRef we must release.
        unsafe { CFRelease(cf_str) };

        if ok == 0 {
            return write!(f, "OSStatus {}", self.0);
        }

        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let msg = std::str::from_utf8(&buf[..nul]).unwrap_or("");
        write!(f, "OSStatus {}: {}", self.0, msg)
    }
}

impl std::error::Error for OsStatusCode {}

/// An error that occurred in the crypto backend.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct BackendError(BackendErrorKind);

#[derive(Clone, Debug, thiserror::Error)]
enum BackendErrorKind {
    #[error("{1}: {0}")]
    OsStatus(#[source] OsStatusCode, &'static str),
    #[error("{0}: returned null")]
    Null(&'static str),
}

impl BackendError {
    pub(crate) fn os_status(code: OsStatusCode, op: &'static str) -> Self {
        Self(BackendErrorKind::OsStatus(code, op))
    }

    pub(crate) fn null(op: &'static str) -> Self {
        Self(BackendErrorKind::Null(op))
    }
}
