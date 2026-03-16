// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES-256-GCM authenticated encryption and decryption.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as sys;

use thiserror::Error;

/// Error returned by AES-256-GCM operations.
#[derive(Debug, Error)]
#[error("AES-256-GCM error")]
pub struct Aes256GcmError(#[source] super::BackendError);

/// Encrypt `data` with AES-256-GCM.
///
/// Writes the authentication tag into `tag`. Returns the ciphertext.
pub fn encrypt(
    key: &[u8],
    iv: &[u8],
    data: &[u8],
    tag: &mut [u8],
) -> Result<Vec<u8>, Aes256GcmError> {
    sys::encrypt(key, iv, data, tag)
}

/// Decrypt `data` with AES-256-GCM.
///
/// Verifies the authentication `tag`. Returns the plaintext.
pub fn decrypt(key: &[u8], iv: &[u8], data: &[u8], tag: &[u8]) -> Result<Vec<u8>, Aes256GcmError> {
    sys::decrypt(key, iv, data, tag)
}
