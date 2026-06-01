// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for Windows operations, used by multiple algorithms.

#![cfg(all(native, windows))]

use std::ffi::c_void;
use windows::Win32::Foundation::HLOCAL;
use windows::Win32::Foundation::LocalFree;
use windows::Win32::Foundation::NTE_BAD_TYPE;
use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

/// Owns a buffer allocated by Crypt32 via `CryptDecodeObjectEx` /
/// `CryptEncodeObjectEx` with the ALLOC flag. Frees with `LocalFree`.
pub struct CryptAlloc {
    pub ptr: *mut c_void,
    pub len: u32,
}

impl Drop for CryptAlloc {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: ptr was allocated by Crypt32 with LocalAlloc semantics.
            let _ = unsafe { LocalFree(Some(HLOCAL(self.ptr))) };
        }
    }
}

impl CryptAlloc {
    /// Returns the allocation as a byte slice, validating that the
    /// pointer is non-null. Required before constructing a slice from a
    /// Crypt32-allocated buffer because `from_raw_parts` requires a
    /// non-null pointer even when `len == 0`.
    pub fn as_bytes(&self) -> Result<&[u8], windows_result::Error> {
        if self.ptr.is_null() {
            return Err(windows_result::Error::from_hresult(NTE_BAD_TYPE));
        }
        // SAFETY: ptr is non-null and points to `len` bytes owned by self.
        Ok(unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len as usize) })
    }

    /// Reborrows the allocation as a `&T`, validating that the pointer is
    /// non-null and the buffer is large enough to hold a `T`.
    ///
    /// # Safety
    ///
    /// Caller must ensure that Crypt32 actually populates the buffer with
    /// a valid `T` (e.g. by passing the matching struct type to
    /// `CryptDecodeObjectEx`).
    pub unsafe fn as_struct<T>(&self) -> Result<&T, windows_result::Error> {
        if self.ptr.is_null() || (self.len as usize) < size_of::<T>() {
            return Err(windows_result::Error::from_hresult(NTE_BAD_TYPE));
        }
        // SAFETY: ptr is non-null, aligned (LocalAlloc returns suitably
        // aligned memory), large enough, and the caller asserts that
        // Crypt32 populated a valid T.
        Ok(unsafe { &*self.ptr.cast::<T>() })
    }
}

pub struct AlgHandle(pub BCRYPT_ALG_HANDLE);
// SAFETY: the handle can be sent across threads.
unsafe impl Send for AlgHandle {}
// SAFETY: the handle can be shared across threads.
unsafe impl Sync for AlgHandle {}

impl Drop for AlgHandle {
    fn drop(&mut self) {
        // SAFETY: handle is valid and not aliased
        let _ = unsafe {
            windows::Win32::Security::Cryptography::BCryptCloseAlgorithmProvider(self.0, 0)
        };
    }
}

pub struct KeyHandle(pub BCRYPT_KEY_HANDLE);
// SAFETY: the handle can be sent across threads.
unsafe impl Send for KeyHandle {}
// SAFETY: the handle can be shared across threads.
unsafe impl Sync for KeyHandle {}

impl Drop for KeyHandle {
    fn drop(&mut self) {
        // SAFETY: handle is valid and not aliased
        let _ = unsafe { windows::Win32::Security::Cryptography::BCryptDestroyKey(self.0) };
    }
}

// TODO: Consider making KeyBlob generic over the key size once zerocopy has better
// const generic support.
#[repr(C)]
#[derive(IntoBytes, Immutable)]
pub struct KeyBlob32 {
    header_magic: u32,
    header_version: u32,
    key_len: u32,
    key: [u8; 32],
}

impl KeyBlob32 {
    pub fn new(key: &[u8; 32]) -> KeyBlob32 {
        KeyBlob32 {
            header_magic: windows::Win32::Security::Cryptography::BCRYPT_KEY_DATA_BLOB_MAGIC,
            header_version: windows::Win32::Security::Cryptography::BCRYPT_KEY_DATA_BLOB_VERSION1,
            key_len: 32,
            key: *key,
        }
    }
}
