// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! XTS-AES-256 encryption and decryption.

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
/// An XTS-AES-256 key contains two AES keys, each of which is 256 bits.
pub const KEY_LEN: usize = 64;

/// XTS-AES-256 encryption/decryption.
pub struct XtsAes256(sys::XtsAes256Inner);

/// An error for XTS-AES-256 cryptographic operations.
#[derive(Clone, Debug, Error)]
#[error("XTS-AES-256 error")]
pub struct XtsAes256Error(#[source] super::BackendError);

impl XtsAes256 {
    /// Creates a new XTS-AES-256 encryption/decryption context.
    pub fn new(key: &[u8; KEY_LEN], data_unit_size: u32) -> Result<Self, XtsAes256Error> {
        sys::XtsAes256Inner::new(key, data_unit_size).map(Self)
    }

    /// Returns a context for encrypting data.
    pub fn encrypt(&self) -> Result<XtsAes256EncCtx<'_>, XtsAes256Error> {
        Ok(XtsAes256EncCtx(self.0.enc_ctx()?))
    }

    /// Returns a context for decrypting data.
    pub fn decrypt(&self) -> Result<XtsAes256DecCtx<'_>, XtsAes256Error> {
        Ok(XtsAes256DecCtx(self.0.dec_ctx()?))
    }
}

/// Context for XTS-AES-256 encryption.
pub struct XtsAes256EncCtx<'a>(sys::XtsAes256EncCtxInner<'a>);

impl XtsAes256EncCtx<'_> {
    /// Encrypts `data` using the provided `tweak`.
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        self.0.cipher(tweak, data)
    }
}

/// Context for XTS-AES-256 decryption.
pub struct XtsAes256DecCtx<'a>(sys::XtsAes256DecCtxInner<'a>);

impl XtsAes256DecCtx<'_> {
    /// Decrypts `data` using the provided `tweak`.
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        self.0.cipher(tweak, data)
    }
}
