// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! XTS-AES-256 encryption and decryption for disk-level crypto.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as sys;

use thiserror::Error;

/// XTS-AES-256 encryption/decryption.
pub struct XtsAes256(sys::XtsAes256Inner);

/// An error for XTS-AES-256 cryptographic operations.
#[derive(Clone, Debug, Error)]
#[error("XTS-AES-256 error")]
pub struct XtsAes256Error(#[source] super::BackendError);

impl XtsAes256 {
    /// The required key length for the algorithm.
    ///
    /// An XTS-AES-256 key contains two AES keys, each of which is 256 bits.
    pub const KEY_LEN: usize = 64;

    /// Creates a new XTS-AES-256 encryption/decryption context.
    pub fn new(key: &[u8; Self::KEY_LEN], data_unit_size: u32) -> Result<Self, XtsAes256Error> {
        sys::XtsAes256Inner::new(key, data_unit_size).map(Self)
    }

    /// Returns a context for encrypting data.
    pub fn encrypt(&self) -> Result<XtsAes256Ctx<'_>, XtsAes256Error> {
        Ok(XtsAes256Ctx(self.0.ctx(true)?))
    }

    /// Returns a context for decrypting data.
    pub fn decrypt(&self) -> Result<XtsAes256Ctx<'_>, XtsAes256Error> {
        Ok(XtsAes256Ctx(self.0.ctx(false)?))
    }
}

/// Context for XTS-AES-256 encryption/decryption.
pub struct XtsAes256Ctx<'a>(sys::XtsAes256CtxInner<'a>);

impl XtsAes256Ctx<'_> {
    /// Encrypts or decrypts `data` using the provided `tweak`.
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        self.0.cipher(tweak, data)
    }
}
