// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES-256-CBC encryption and decryption.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// Error returned by AES-256-CBC operations.
#[derive(Debug, Error)]
#[error("AES-256-CBC error")]
pub struct Aes256CbcError(#[source] super::BackendError);

/// AES-256-CBC encrypt with no padding.
pub fn encrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    sys::encrypt(key, data, iv)
}

/// AES-256-CBC decrypt with no padding.
pub fn decrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    sys::decrypt(key, data, iv)
}
