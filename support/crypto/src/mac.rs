// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for macOS operations, used by multiple algorithms.

#![cfg(all(native, target_os = "macos"))]

use std::ffi::c_void;
use std::fmt;

pub(crate) type CFTypeRef = *const c_void;
pub(crate) type CFAllocatorRef = *const c_void;
pub(crate) type CFDataRef = *const c_void;
pub(crate) type CFStringRef = *const c_void;
pub(crate) type CFErrorRef = *const c_void;
pub(crate) type CFArrayRef = *const c_void;
pub(crate) type CFNumberRef = *const c_void;
pub(crate) type CFDictionaryRef = *const c_void;
pub(crate) type CFIndex = isize;

pub(crate) type SecKeyRef = CFTypeRef;

/// kCFStringEncodingUTF8
const K_CF_STRING_ENCODING_UTF8: u32 = 0x08000100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) static kCFAllocatorDefault: CFAllocatorRef;
    pub(crate) fn CFRelease(cf: CFTypeRef);
    pub(crate) fn CFDataCreate(
        allocator: CFAllocatorRef,
        bytes: *const u8,
        length: CFIndex,
    ) -> CFDataRef;
    pub(crate) fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    pub(crate) fn CFDataGetLength(data: CFDataRef) -> CFIndex;
    pub(crate) fn CFArrayGetCount(arr: CFArrayRef) -> CFIndex;
    pub(crate) fn CFArrayGetValueAtIndex(arr: CFArrayRef, idx: CFIndex) -> CFTypeRef;
    fn CFStringGetLength(the_string: CFStringRef) -> CFIndex;
    fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut u8,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> u8;
    fn CFErrorGetCode(err: CFErrorRef) -> CFIndex;
    fn CFErrorCopyDescription(err: CFErrorRef) -> CFStringRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: i32,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecCopyErrorMessageString(status: OsStatusCode, reserved: *const c_void) -> CFStringRef;
}

/// RAII wrapper for any CoreFoundation type. Released with `CFRelease` on
/// drop. Null pointers are tolerated (drop is a no-op) so this can be
/// constructed directly from the return value of CF/Security APIs.
pub(crate) struct CfHandle(pub(crate) CFTypeRef);

// SAFETY: CoreFoundation immutable objects and Security.framework `SecKey`
// objects are documented as thread-safe; CFRetain/CFRelease are also
// thread-safe. The CF types wrapped by `CfHandle` in this crate are all in
// that category.
unsafe impl Send for CfHandle {}
// SAFETY: see above.
unsafe impl Sync for CfHandle {}

impl Drop for CfHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: pointer is a valid CF object that we own.
            unsafe { CFRelease(self.0) };
        }
    }
}

/// Create an owned `CFData` from a byte slice, wrapped in a `CfHandle`.
/// Returns `BackendError::Null` if CFDataCreate returns null.
pub(crate) fn cf_data(bytes: &[u8], op: &'static str) -> Result<CfHandle, super::BackendError> {
    // SAFETY: bytes pointer/length is valid; kCFAllocatorDefault is a
    // valid CF allocator.
    let data = unsafe { CFDataCreate(kCFAllocatorDefault, bytes.as_ptr(), bytes.len() as CFIndex) };
    if data.is_null() {
        return Err(super::BackendError::Null(op));
    }
    Ok(CfHandle(data))
}

/// Copy the bytes out of a `CFData` into a `Vec<u8>`.
///
/// # Safety
///
/// `data` must be a valid `CFDataRef`.
pub(crate) unsafe fn cf_data_to_vec(data: CFDataRef) -> Vec<u8> {
    // SAFETY: per caller contract.
    let len = unsafe { CFDataGetLength(data) } as usize;
    // SAFETY: per caller contract.
    let ptr = unsafe { CFDataGetBytePtr(data) };
    if ptr.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: ptr is valid for `len` bytes per CFDataGetBytePtr contract.
    unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
}

/// Copy a `CFString` into a Rust `String`, releasing the `CFString` once
/// done. Null is treated as the empty string.
///
/// # Safety
///
/// `s` must be a valid owned `CFStringRef` (or null) returned by a
/// CF/Security API.
pub(crate) unsafe fn cf_string_to_string(s: CFStringRef) -> String {
    if s.is_null() {
        return String::new();
    }
    let _release = CfHandle(s);
    // SAFETY: s is a valid non-null CFStringRef.
    let len = unsafe { CFStringGetLength(s) };
    let buf_size = len * 4 + 1;
    let mut buf = vec![0u8; buf_size as usize];
    // SAFETY: buf is sized as required.
    let ok =
        unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf_size, K_CF_STRING_ENCODING_UTF8) };
    if ok == 0 {
        return String::new();
    }
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..nul]).into_owned()
}

/// Build a `BackendError::Sec` from an owned `CFErrorRef`, releasing it.
/// If `error` is null, returns a `BackendError::Null` for `op` instead.
///
/// # Safety
///
/// `error` must be a valid owned `CFErrorRef` (or null) returned by a
/// CF/Security API.
pub(crate) unsafe fn sec_err(error: CFErrorRef, op: &'static str) -> super::BackendError {
    if error.is_null() {
        return super::BackendError::Null(op);
    }
    let release = CfHandle(error);
    // SAFETY: error is a valid CFErrorRef.
    let code = unsafe { CFErrorGetCode(error) };
    // SAFETY: error is valid; CFErrorCopyDescription returns an owned
    // CFStringRef (or null) which cf_string_to_string handles.
    let msg = unsafe { cf_string_to_string(CFErrorCopyDescription(error)) };
    drop(release);
    super::BackendError::Sec(format!("code {}: {}", code, msg), op)
}

/// Returns the CFError's numeric code, or `None` if `error` is null. Does
/// not release `error`.
///
/// # Safety
///
/// `error` must be null or a valid `CFErrorRef`.
pub(crate) unsafe fn cf_error_code(error: CFErrorRef) -> Option<CFIndex> {
    if error.is_null() {
        return None;
    }
    // SAFETY: per caller contract.
    Some(unsafe { CFErrorGetCode(error) })
}

/// `kCFNumberIntType` — type tag for `CFNumberCreate` with a 32-bit value.
const K_CF_NUMBER_INT_TYPE: i32 = 9;

/// Create an owned `CFNumber` wrapping an `i32` value.
pub(crate) fn cf_number(value: i32, op: &'static str) -> Result<CfHandle, super::BackendError> {
    // SAFETY: value pointer is valid; kCFAllocatorDefault is valid.
    let num = unsafe {
        CFNumberCreate(
            kCFAllocatorDefault,
            K_CF_NUMBER_INT_TYPE,
            std::ptr::from_ref(&value).cast(),
        )
    };
    if num.is_null() {
        return Err(super::BackendError::Null(op));
    }
    Ok(CfHandle(num))
}

/// Create an owned `CFDictionary` from `(key, value)` slices. Both keys
/// and values must outlive the call; the dictionary takes its own
/// references via `kCFTypeDictionary*CallBacks`.
pub(crate) fn cf_dict(
    pairs: &[(CFTypeRef, CFTypeRef)],
    op: &'static str,
) -> Result<CfHandle, super::BackendError> {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|(k, _)| *k).collect();
    let values: Vec<CFTypeRef> = pairs.iter().map(|(_, v)| *v).collect();
    // SAFETY: keys/values point to valid CF objects; callback tables are
    // standard CF symbols.
    let dict = unsafe {
        CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as CFIndex,
            std::ptr::from_ref(&kCFTypeDictionaryKeyCallBacks).cast(),
            std::ptr::from_ref(&kCFTypeDictionaryValueCallBacks).cast(),
        )
    };
    if dict.is_null() {
        return Err(super::BackendError::Null(op));
    }
    Ok(CfHandle(dict))
}

/// An OSStatus code from a Security.framework or CoreFoundation API.
///
/// Displays a human-readable message via `SecCopyErrorMessageString` when
/// available, falling back to just the numeric code.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct OsStatusCode(pub i32);

impl OsStatusCode {
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
        // SAFETY: cf_str is an owned non-null CFStringRef.
        let msg = unsafe { cf_string_to_string(cf_str) };
        if msg.is_empty() {
            write!(f, "OSStatus {}", self.0)
        } else {
            write!(f, "OSStatus {}: {}", self.0, msg)
        }
    }
}

impl std::error::Error for OsStatusCode {}
