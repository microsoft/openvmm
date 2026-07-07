// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA cryptographic operations.

#![cfg(any(
    openssl,
    rust,
    symcrypt,
    all(native, windows),
    all(native, target_os = "macos")
))]

#[cfg(openssl)]
pub(crate) mod ossl;
#[cfg(openssl)]
use ossl as sys;

#[cfg(rust)]
pub(crate) mod rust;
#[cfg(rust)]
use rust as sys;

#[cfg(symcrypt)]
pub(crate) mod symcrypt;
#[cfg(symcrypt)]
use symcrypt as sys;

#[cfg(all(native, windows))]
pub(crate) mod win;
#[cfg(all(native, windows))]
use win as sys;

#[cfg(all(native, target_os = "macos"))]
mod mac;
#[cfg(all(native, target_os = "macos"))]
use mac as sys;

use crate::HashAlgorithm;
use thiserror::Error;

/// An error for RSA operations.
#[cfg(any(
    openssl,
    symcrypt,
    all(native, windows),
    all(native, target_os = "macos")
))]
#[derive(Debug, Error)]
#[error("RSA error")]
pub struct RsaError(#[source] pub(crate) super::BackendError);

/// An error for RSA operations.
// TODO: Make this clone once RustCrypto rsa::errors::Error is cloneable
#[cfg(rust)]
#[derive(Debug, Error)]
#[error("RSA error during {1}")]
pub struct RsaError(
    #[source] pub(crate) rsa::errors::Error,
    pub(crate) &'static str,
);

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

    #[cfg(any(test, feature = "test_helpers"))]
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

    /// Sign `data` using RSA-PSS (RSASSA-PSS) with the specified hash
    /// algorithm. Uses MGF1 with the same hash and a salt length equal to
    /// the hash output size, matching the convention used by COSE/JWS
    /// (RFC 8230 section 2) and TLS 1.3.
    pub fn pss_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0.pss_sign(data, hash_algorithm)
    }
}

/// An RSA public key.
#[repr(transparent)] // Needed for the transmute in deref.
pub struct RsaPublicKey(pub(crate) sys::RsaPublicKeyInner);

/// The public components of an RSA key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsaPublicKeyComponents {
    /// The RSA modulus as a big-endian byte vector.
    pub modulus: Vec<u8>,
    /// The RSA public exponent as a big-endian byte vector.
    pub public_exponent: Vec<u8>,
}

impl RsaPublicKey {
    /// Construct an RSA public key from a big-endian modulus `n` and
    /// big-endian public exponent `e`.
    pub fn from_components(n: &[u8], e: &[u8]) -> Result<Self, RsaError> {
        sys::RsaPublicKeyInner::from_components(n, e).map(Self)
    }

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

    /// Verify an RSA-PSS (RSASSA-PSS) signature with the specified hash
    /// algorithm. Uses MGF1 with the same hash and a salt length equal to
    /// the hash output size, matching the convention used by COSE/JWS
    /// (RFC 8230 section 2) and TLS 1.3. Returns `Ok(true)` if the
    /// signature is valid, `Ok(false)` if the signature is invalid, or an
    /// error for other failures.
    pub fn pss_verify(
        &self,
        message: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        self.0.pss_verify(message, signature, hash_algorithm)
    }

    /// Returns the size of the RSA modulus in bytes.
    pub fn modulus_size(&self) -> usize {
        self.0.modulus_size()
    }

    /// Returns the RSA public components as big-endian byte vectors.
    pub fn to_components(&self) -> RsaPublicKeyComponents {
        self.0.to_components()
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
    /// silently treating verification failures as internal errors,
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
        assert_eq!(key.to_components(), imported.to_components());

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
    #[expect(deprecated)]
    fn oaep_roundtrip() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let payload = b"a secret message";
        for alg in [HashAlgorithm::Sha1, HashAlgorithm::Sha256] {
            let ct = key.oaep_encrypt(payload, alg).unwrap();
            let pt = key.oaep_decrypt(&ct, alg).unwrap();
            assert_eq!(pt, payload);
        }
    }

    /// PSS sign/verify round-trip with SHA-384. Each signature contains
    /// fresh random salt, so this also implicitly checks that the verifier
    /// recovers the salt from the signature rather than requiring a fixed
    /// value across backends.
    ///
    /// Uses a 2048-bit key for test speed; SHA-384's matched-strength
    /// key size is 3072 (NIST SP 800-57) but key generation in the
    /// pure-Rust backend is too slow at that size for unit tests.
    #[test]
    fn pss_sign_verify_roundtrip_sha384() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let message = b"the message that needs to be hashed before signing";
        let signature = key.pss_sign(message, HashAlgorithm::Sha384).unwrap();
        let valid = key
            .pss_verify(message, &signature, HashAlgorithm::Sha384)
            .unwrap();
        assert!(valid);
    }

    /// A tampered message must fail PSS verification with `Ok(false)`.
    #[test]
    fn pss_verify_rejects_tampered_message_sha384() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let message = b"original message";
        let signature = key.pss_sign(message, HashAlgorithm::Sha384).unwrap();
        let tampered = b"tampered message";
        let valid = key
            .pss_verify(tampered, &signature, HashAlgorithm::Sha384)
            .unwrap();
        assert!(!valid);
    }

    /// A truncated PSS signature must be rejected with `Ok(false)`.
    #[test]
    fn pss_verify_rejects_truncated_signature_sha384() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let message = b"original message";
        let mut signature = key.pss_sign(message, HashAlgorithm::Sha384).unwrap();
        signature.truncate(signature.len() - 1);
        let valid = key
            .pss_verify(message, &signature, HashAlgorithm::Sha384)
            .unwrap();
        assert!(!valid);
    }

    /// A PKCS#1 v1.5 signature must not verify as PSS (and vice versa);
    /// the two padding schemes are not interchangeable on the wire.
    #[test]
    fn pss_and_pkcs1_signatures_are_not_interchangeable() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let message = b"scheme-mismatch test";
        let pkcs1_sig = key.pkcs1_sign(message, HashAlgorithm::Sha256).unwrap();
        let pss_sig = key.pss_sign(message, HashAlgorithm::Sha256).unwrap();
        let pkcs1_as_pss = key
            .pss_verify(message, &pkcs1_sig, HashAlgorithm::Sha256)
            .unwrap();
        let pss_as_pkcs1 = key
            .pkcs1_verify(message, &pss_sig, HashAlgorithm::Sha256)
            .unwrap();
        assert!(!pkcs1_as_pss);
        assert!(!pss_as_pkcs1);
    }
}
