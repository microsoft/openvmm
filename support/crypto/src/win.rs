// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for Windows operations, used by multiple algorithms.

#![cfg(all(native, windows))]

use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

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
