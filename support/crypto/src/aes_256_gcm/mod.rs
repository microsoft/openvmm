// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES-256-GCM encryption and decryption.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as sys;

use thiserror::Error;

/// The required key length for the algorithm.
///
/// An AES-256-GCM key is 256 bits.
pub const KEY_LEN: usize = 32;

/// AES-256-GCM encryption/decryption.
pub struct Aes256Gcm(sys::Aes256GcmInner);

/// An error for AES-256-GCM cryptographic operations.
#[derive(Clone, Debug, Error)]
#[error("AES-256-GCM error")]
pub struct Aes256GcmError(#[source] super::BackendError);

impl Aes256Gcm {
    /// Creates a new AES-256-GCM encryption/decryption context.
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256GcmError> {
        sys::Aes256GcmInner::new(key).map(Self)
    }

    /// Returns a context for encrypting data.
    pub fn encrypt(&self) -> Result<Aes256GcmCtx<'_>, Aes256GcmError> {
        Ok(Aes256GcmCtx(self.0.ctx(true)?))
    }

    /// Returns a context for decrypting data.
    pub fn decrypt(&self) -> Result<Aes256GcmCtx<'_>, Aes256GcmError> {
        Ok(Aes256GcmCtx(self.0.ctx(false)?))
    }
}

/// Context for AES-256-GCM encryption/decryption.
pub struct Aes256GcmCtx<'a>(sys::Aes256GcmCtxInner<'a>);

impl Aes256GcmCtx<'_> {
    /// Encrypts or decrypts `data` using the provided `iv` and produces or
    /// verifies the authentication tag in `tag`.
    pub fn cipher(
        &mut self,
        iv: &[u8],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        self.0.cipher(iv, data, tag)
    }
}
