// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES-256-CBC encryption and decryption (no padding).

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// The required key length for the algorithm.
///
/// An AES-256-CBC key is 256 bits.
pub const KEY_LEN: usize = 32;

/// AES-256-CBC encryption/decryption.
pub struct Aes256Cbc(sys::Aes256CbcInner);

/// An error for AES-256-CBC cryptographic operations.
#[derive(Clone, Debug, Error)]
#[error("AES-256-CBC error")]
pub struct Aes256CbcError(#[source] super::BackendError);

impl Aes256Cbc {
    /// Creates a new AES-256-CBC encryption/decryption context.
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256CbcError> {
        sys::Aes256CbcInner::new(key).map(Self)
    }

    /// Returns a context for encrypting data.
    pub fn encrypt(&self) -> Result<Aes256CbcEncCtx<'_>, Aes256CbcError> {
        Ok(Aes256CbcEncCtx(self.0.enc_ctx()?))
    }

    /// Returns a context for decrypting data.
    pub fn decrypt(&self) -> Result<Aes256CbcDecCtx<'_>, Aes256CbcError> {
        Ok(Aes256CbcDecCtx(self.0.dec_ctx()?))
    }
}

/// Context for AES-256-CBC encryption.
pub struct Aes256CbcEncCtx<'a>(sys::Aes256CbcEncCtxInner<'a>);

impl Aes256CbcEncCtx<'_> {
    /// Encrypts `data` using the provided `iv`.
    ///
    /// Padding is disabled — `data` must be a multiple of 16 bytes.
    pub fn cipher(&mut self, iv: &[u8], data: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
        self.0.cipher(iv, data)
    }
}

/// Context for AES-256-CBC decryption.
pub struct Aes256CbcDecCtx<'a>(sys::Aes256CbcDecCtxInner<'a>);

impl Aes256CbcDecCtx<'_> {
    /// Decrypts `data` using the provided `iv`.
    ///
    /// Padding is disabled — `data` must be a multiple of 16 bytes.
    pub fn cipher(&mut self, iv: &[u8], data: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
        self.0.cipher(iv, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aes_256_cbc_roundtrip() {
        let key = [0x42u8; KEY_LEN];
        let iv = [0x01u8; 16];
        let data = [0xABu8; 32]; // must be block-aligned for no-padding CBC

        let aes = Aes256Cbc::new(&key).unwrap();

        let mut enc_ctx = aes.encrypt().unwrap();
        let encrypted = enc_ctx.cipher(&iv, &data).unwrap();
        assert_ne!(encrypted, data);

        let mut dec_ctx = aes.decrypt().unwrap();
        let decrypted = dec_ctx.cipher(&iv, &encrypted).unwrap();
        assert_eq!(decrypted, data);
    }
}
