// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA key operations including OAEP encryption/decryption and PKCS#11
//! key unwrapping.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// An RSA key pair (private + public).
pub struct RsaKeyPair(sys::RsaKeyPairInner);

impl std::fmt::Debug for RsaKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsaKeyPair")
            .field("bits", &(self.modulus_size() * 8))
            .finish()
    }
}

/// RSA-OAEP hash algorithm selection.
#[derive(Debug, Clone, Copy)]
pub enum OaepHashAlgorithm {
    /// SHA-1
    Sha1,
    /// SHA-256
    Sha256,
}

/// Error from RSA operations.
#[derive(Debug, Error)]
#[error("RSA error")]
pub struct RsaError(#[source] super::BackendError);

/// Error from RSA-OAEP operations.
#[derive(Debug, Error)]
#[error("RSA-OAEP error")]
pub struct RsaOaepError(#[source] super::BackendError);

/// Error from PKCS#11 RSA-AES key unwrap.
#[derive(Debug, Error)]
#[error("PKCS#11 RSA-AES key unwrap error")]
pub struct Pkcs11RsaAesKeyUnwrapError(#[source] super::BackendError);

impl RsaKeyPair {
    /// Generate a new RSA key pair with the given number of bits.
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        sys::generate(bits).map(Self)
    }

    /// Import an RSA private key from DER-encoded bytes.
    pub fn from_der(der: &[u8]) -> Result<Self, RsaError> {
        sys::from_der(der).map(Self)
    }

    /// Export the RSA private key to DER-encoded bytes.
    pub fn private_key_to_der(&self) -> Result<Vec<u8>, RsaError> {
        self.0.private_key_to_der()
    }

    /// The modulus size in bytes.
    pub fn modulus_size(&self) -> usize {
        self.0.modulus_size()
    }

    /// The public exponent as big-endian bytes.
    pub fn exponent(&self) -> Vec<u8> {
        self.0.exponent()
    }

    /// The modulus as big-endian bytes.
    pub fn modulus(&self) -> Vec<u8> {
        self.0.modulus()
    }

    /// RSA-OAEP encrypt.
    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaOaepError> {
        self.0.oaep_encrypt(input, hash_algorithm)
    }

    /// RSA-OAEP decrypt.
    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaOaepError> {
        self.0.oaep_decrypt(input, hash_algorithm)
    }

    /// PKCS#11 RSA AES key unwrap.
    ///
    /// Unwraps a key blob that was wrapped with the PKCS#11 CKM_RSA_AES_KEY_WRAP
    /// mechanism. Returns the unwrapped RSA key.
    pub fn pkcs11_rsa_aes_key_unwrap(
        &self,
        wrapped_key_blob: &[u8],
    ) -> Result<RsaKeyPair, Pkcs11RsaAesKeyUnwrapError> {
        self.0.pkcs11_rsa_aes_key_unwrap(wrapped_key_blob).map(Self)
    }
}
