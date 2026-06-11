// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 implementation using the Windows BCrypt API.

use windows::Win32::Security::Cryptography::BCRYPT_HASH_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_SHA256_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCryptCreateHash;
use windows::Win32::Security::Cryptography::BCryptDestroyHash;
use windows::Win32::Security::Cryptography::BCryptFinishHash;
use windows::Win32::Security::Cryptography::BCryptHashData;

pub struct Sha256(BCRYPT_HASH_HANDLE);

impl Sha256 {
    pub fn new() -> Self {
        let mut handle = BCRYPT_HASH_HANDLE::default();
        // SAFETY: the SHA-256 pseudo-handle is a valid process-wide algorithm
        // handle; no secret and no caller-allocated object buffer are needed.
        // All inputs are known-good, so the call must succeed.
        unsafe {
            BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut handle, None, None, 0).unwrap();
        }
        Self(handle)
    }

    pub fn update(&mut self, data: &[u8]) {
        // SAFETY: handle is valid and owned; data slice is valid for the call.
        // The call cannot fail for a valid SHA-256 hash handle.
        unsafe {
            BCryptHashData(self.0, data, 0).unwrap();
        }
    }

    pub fn finish(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: handle is valid and owned; output buffer matches the SHA-256
        // digest size. The call cannot fail for valid inputs.
        unsafe {
            BCryptFinishHash(self.0, &mut out, 0).unwrap();
        }
        out
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        // SAFETY: handle is valid and not aliased.
        let _ = unsafe { BCryptDestroyHash(self.0) };
    }
}
