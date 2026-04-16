// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA cryptographic operations.

#[cfg(unix)]
pub(crate) mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// An error for RSA operations.
#[derive(Debug, Error)]
#[error("RSA error")]
pub struct RsaError(#[source] super::BackendError);

/// Hash algorithm for RSA-OAEP encryption/decryption.
#[derive(Debug, Clone, Copy)]
pub enum OaepHashAlgorithm {
    /// SHA-1
    Sha1,
    /// SHA-256
    Sha256,
}

/// An RSA private key (key pair).
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

    /// Encrypt `input` using RSA-OAEP with the specified hash algorithm.
    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0.oaep_encrypt(input, hash_algorithm)
    }

    /// Decrypt `input` using RSA-OAEP with the specified hash algorithm.
    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0.oaep_decrypt(input, hash_algorithm)
    }

    /// Export the private key in PKCS#8 DER format.
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        self.0.to_pkcs8_der()
    }

    /// Export the private key in traditional RSA DER format.
    pub fn to_private_key_der(&self) -> Result<Vec<u8>, RsaError> {
        self.0.to_private_key_der()
    }

    /// Sign `data` using RSA PKCS#1 v1.5 with SHA-256.
    pub fn sign_pkcs1_sha256(&self, data: &[u8]) -> Result<Vec<u8>, RsaError> {
        self.0.sign_pkcs1_sha256(data)
    }
}

/// An RSA public key.
pub struct RsaPublicKey(pub(crate) sys::RsaPublicKeyInner);

impl RsaPublicKey {
    /// Verify an RSA PKCS#1 v1.5 signature with SHA-256.
    pub fn verify_pkcs1_sha256(&self, message: &[u8], signature: &[u8]) -> Result<bool, RsaError> {
        self.0.verify_pkcs1_sha256(message, signature)
    }
}
