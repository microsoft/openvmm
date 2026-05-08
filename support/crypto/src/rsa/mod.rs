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
pub struct RsaError(#[source] pub(crate) super::BackendError);

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

    /// Verify an RSA PKCS#1 v1.5 signature with the specified hash algorithm.
    /// Returns `Ok(true)` if the signature is valid, `Ok(false)` if the
    /// signature is invalid, or an error for other failures.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Sign and verify a message of arbitrary (i.e. not pre-hashed) length.
    /// Both backends must hash the message internally before applying the
    /// PKCS#1 v1.5 RSA signature, so this test guards against the
    /// hash-vs-raw-message confusion that would otherwise produce
    /// different signatures and/or false rejections across backends.
    #[test]
    fn pkcs1_sign_verify_roundtrip_sha256() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let message = b"the message that needs to be hashed before signing";
        let signature = key.pkcs1_sign(message, HashAlgorithm::Sha256).unwrap();
        let valid = key
            .pkcs1_verify(message, &signature, HashAlgorithm::Sha256)
            .unwrap();
        assert!(valid);
    }

    /// A tampered message must fail verification with `Ok(false)` (and
    /// must not panic or surface as `Err`). This guards against a backend
    /// silently treating verification failures as internal errors —
    /// which would prevent callers from distinguishing "bad signature"
    /// from "infrastructure failure".
    #[test]
    fn pkcs1_verify_rejects_tampered_message() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let message = b"original message";
        let signature = key.pkcs1_sign(message, HashAlgorithm::Sha256).unwrap();
        let tampered = b"tampered message";
        let valid = key
            .pkcs1_verify(tampered, &signature, HashAlgorithm::Sha256)
            .unwrap();
        assert!(!valid);
    }

    /// A signature truncated to less than the modulus size must be
    /// rejected with `Ok(false)` (and must not panic or surface as
    /// `Err`). This keeps malformed signatures in the "bad signature"
    /// bucket rather than conflating them with infrastructure failures.
    #[test]
    fn pkcs1_verify_rejects_truncated_signature() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let message = b"original message";
        let mut signature = key.pkcs1_sign(message, HashAlgorithm::Sha256).unwrap();
        signature.truncate(signature.len() - 1);
        let valid = key
            .pkcs1_verify(message, &signature, HashAlgorithm::Sha256)
            .unwrap();
        assert!(!valid);
    }

    #[test]
    fn pkcs8_der_roundtrip() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let der = key.to_pkcs8_der().unwrap();
        let imported = RsaKeyPair::from_pkcs8_der(&der).unwrap();
        assert_eq!(key.modulus(), imported.modulus());
        assert_eq!(key.public_exponent(), imported.public_exponent());

        // Sign with the original and verify with the imported, to confirm
        // the private-key components survived the round-trip.
        let message = b"roundtrip";
        let signature = key.pkcs1_sign(message, HashAlgorithm::Sha256).unwrap();
        let valid = imported
            .pkcs1_verify(message, &signature, HashAlgorithm::Sha256)
            .unwrap();
        assert!(valid);
    }

    /// OAEP encrypt/decrypt round-trip with both supported hash algorithms.
    #[test]
    fn oaep_roundtrip() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let payload = b"a secret message";
        for alg in [HashAlgorithm::Sha1, HashAlgorithm::Sha256] {
            let ct = key.oaep_encrypt(payload, alg).unwrap();
            let pt = key.oaep_decrypt(&ct, alg).unwrap();
            assert_eq!(pt, payload);
        }
    }
}
