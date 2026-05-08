// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA cryptographic operations.

#![cfg(any(openssl, symcrypt))]

#[cfg(openssl)]
pub(crate) mod ossl;
#[cfg(openssl)]
use ossl as sys;

#[cfg(symcrypt)]
pub(crate) mod symcrypt;
#[cfg(symcrypt)]
use symcrypt as sys;

use thiserror::Error;

/// An error for RSA operations.
#[derive(Debug, Error)]
#[error("RSA error")]
pub struct RsaError(#[source] super::BackendError);

/// Hash algorithm for RSA operations.
#[derive(Debug, Clone, Copy)]
pub enum HashAlgorithm {
    /// SHA-1
    Sha1,
    /// SHA-256
    Sha256,
}

/// An RSA private key (key pair).
#[repr(transparent)] // Needed for the transmute in deref.
pub struct RsaKeyPair(pub(crate) sys::RsaKeyPairInner);

impl RsaKeyPair {
    /// Generate a new RSA key pair with the given bit size.
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        sys::RsaKeyPairInner::generate(bits).map(Self)
    }

    /// Parse an RSA private key from PKCS#8 DER-encoded bytes.
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, RsaError> {
        sys::RsaKeyPairInner::from_pkcs8_der(der).map(Self)
    }

    /// Convert the RSA private key to PKCS#8 DER-encoded bytes.
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        self.0.to_pkcs8_der()
    }

    /// Decrypt `input` using RSA-OAEP with the specified hash algorithm.
    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0.oaep_decrypt(input, hash_algorithm)
    }

    /// Sign `data` using RSA PKCS#1 v1.5 with the specified hash algorithm.
    pub fn pkcs1_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0.pkcs1_sign(data, hash_algorithm)
    }
}

/// An RSA public key.
#[repr(transparent)] // Needed for the transmute in deref.
pub struct RsaPublicKey(pub(crate) sys::RsaPublicKeyInner);

impl RsaPublicKey {
    /// Encrypt `input` using RSA-OAEP with the specified hash algorithm.
    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0.oaep_encrypt(input, hash_algorithm)
    }

    /// Verify an RSA PKCS#1 v1.5 signature with the specified hash algorithm. Returns `Ok(true)` if the signature is valid.
    /// Different backends may return Ok(false) or an error if the signature is invalid, but all return an error for other failures.
    pub fn pkcs1_verify(
        &self,
        message: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        self.0.pkcs1_verify(message, signature, hash_algorithm)
    }

    /// Returns the size of the RSA modulus in bytes.
    pub fn modulus_size(&self) -> usize {
        self.0.modulus_size()
    }

    /// Returns the RSA modulus as a big-endian byte vector.
    pub fn modulus(&self) -> Vec<u8> {
        self.0.modulus()
    }

    /// Returns the RSA public exponent as a big-endian byte vector.
    pub fn public_exponent(&self) -> Vec<u8> {
        self.0.public_exponent()
    }
}

impl std::ops::Deref for RsaKeyPair {
    type Target = RsaPublicKey;

    fn deref(&self) -> &Self::Target {
        // SAFETY: RsaPublicKey is just a wrapper around RsaPublicKeyInner.
        unsafe { std::mem::transmute::<&sys::RsaPublicKeyInner, &RsaPublicKey>(self.0.as_pub()) }
    }
}
