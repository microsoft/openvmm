// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cryptography primitives for disk encryption.

use thiserror::Error;

/// XTS-AES-256 encryption/decryption.
pub struct XtsAes256(crypto::xts_aes_256::XtsAes256);

/// An error for cryptographic operations.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(crypto::xts_aes_256::XtsAes256Error);

impl XtsAes256 {
    /// The required key length for the algorithm.
    ///
    /// Note that an XTS-AES-256 key contains two AES keys, each of which is 256 bits.
    pub const KEY_LEN: usize = 64;

    /// Creates a new XTS-AES-256 encryption/decryption context.
    pub fn new(key: &[u8; Self::KEY_LEN], data_unit_size: u32) -> Result<Self, Error> {
        crypto::xts_aes_256::XtsAes256::new(key, data_unit_size)
            .map(Self)
            .map_err(Error)
    }

    /// Returns a context for encrypting data.
    pub fn encrypt(&self) -> Result<XtsAes256Ctx<'_>, Error> {
        Ok(XtsAes256Ctx(self.0.encrypt().map_err(Error)?))
    }

    /// Returns a context for decrypting data.
    pub fn decrypt(&self) -> Result<XtsAes256Ctx<'_>, Error> {
        Ok(XtsAes256Ctx(self.0.decrypt().map_err(Error)?))
    }
}

/// Context for XTS-AES-256 encryption/decryption.
pub struct XtsAes256Ctx<'a>(crypto::xts_aes_256::XtsAes256Ctx<'a>);

impl XtsAes256Ctx<'_> {
    /// Encrypts or decrypts `data` using the provided `tweak`.
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), Error> {
        self.0.cipher(tweak, data).map_err(Error)?;
        Ok(())
    }
}
