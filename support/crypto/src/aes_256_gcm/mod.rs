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
    pub fn encrypt(&self) -> Result<Aes256GcmEncCtx<'_>, Aes256GcmError> {
        Ok(Aes256GcmEncCtx(self.0.enc_ctx()?))
    }

    /// Returns a context for decrypting data.
    pub fn decrypt(&self) -> Result<Aes256GcmDecCtx<'_>, Aes256GcmError> {
        Ok(Aes256GcmDecCtx(self.0.dec_ctx()?))
    }
}

/// Context for AES-256-GCM encryption.
pub struct Aes256GcmEncCtx<'a>(sys::Aes256GcmEncCtxInner<'a>);

impl Aes256GcmEncCtx<'_> {
    /// Encrypts `data` using the provided `iv` and produces the
    /// authentication tag in `tag`.
    pub fn cipher(
        &mut self,
        iv: &[u8],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        self.0.cipher(iv, data, tag)
    }
}

/// Context for AES-256-GCM decryption.
pub struct Aes256GcmDecCtx<'a>(sys::Aes256GcmDecCtxInner<'a>);

impl Aes256GcmDecCtx<'_> {
    /// Decrypts `data` using the provided `iv` and verifies the
    /// authentication `tag`.
    pub fn cipher(
        &mut self,
        iv: &[u8],
        data: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        self.0.cipher(iv, data, tag)
    }
}
