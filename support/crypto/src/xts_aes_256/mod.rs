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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xts_aes_256() {
        // Key and plaintext from IEEE Std 1619-2007 XTS-AES-256 test vectors
        // (vectors 10–14 use this key derived from the digits of e and pi).
        let key = hex::decode(
            "2718281828459045235360287471352662497757247093699959574966967627\
             3141592653589793238462643383279502884197169399375105820974944592",
        )
        .unwrap();
        let tweak: u128 = 0;
        let plain = hex::decode("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .unwrap();
        let expected_ciphertext =
            hex::decode("3a060a8cad115a6f44572e3759e43c8fcad8bfcb233ff6ad71b7c1e7ca651508")
                .unwrap();

        let data_unit_size = plain.len() as u32;
        let xts = XtsAes256::new(&key.try_into().unwrap(), data_unit_size).unwrap();

        // Encrypt and verify against known test vector.
        let mut enc_data = plain.clone();
        let mut enc_ctx = xts.encrypt().unwrap();
        enc_ctx.cipher(tweak, &mut enc_data).unwrap();
        assert_eq!(enc_data, expected_ciphertext);

        // Decrypt and verify we recover the original plaintext.
        let mut dec_data = enc_data.clone();
        let mut dec_ctx = xts.decrypt().unwrap();
        dec_ctx.cipher(tweak, &mut dec_data).unwrap();
        assert_eq!(dec_data, plain);

        // Verify a different tweak produces different ciphertext.
        let mut enc_data2 = plain.clone();
        let mut enc_ctx2 = xts.encrypt().unwrap();
        enc_ctx2.cipher(1, &mut enc_data2).unwrap();
        assert_ne!(enc_data2, enc_data);
    }
}
