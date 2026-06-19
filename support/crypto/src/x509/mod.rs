// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate operations.

#![cfg(any(
    openssl,
    rust,
    symcrypt,
    all(native, windows),
    all(native, target_os = "macos")
))]

#[cfg(any(rust, symcrypt, all(native, target_os = "macos"),))]
mod builder;

#[cfg(openssl)]
pub(crate) mod ossl;
#[cfg(openssl)]
use ossl as sys;

#[cfg(any(rust, symcrypt))]
pub(crate) mod symcrypt_rust;
#[cfg(any(rust, symcrypt))]
use symcrypt_rust as sys;

#[cfg(all(native, windows))]
pub(crate) mod win;
#[cfg(all(native, windows))]
use win as sys;

#[cfg(all(native, target_os = "macos"))]
mod mac;
#[cfg(all(native, target_os = "macos"))]
use mac as sys;

use thiserror::Error;

/// An error for X.509 operations.
#[cfg(not(rust))]
#[derive(Clone, Debug, Error)]
#[error("X.509 error")]
pub struct X509Error(#[source] pub(crate) super::BackendError);

/// An error for X.509 operations.
#[cfg(rust)]
#[derive(Clone, Debug, Error)]
#[error("X.509 error during {1}")]
pub struct X509Error(#[source] pub(crate) der::Error, pub(crate) &'static str);

/// An X.509 certificate.
pub struct X509Certificate(pub(crate) sys::X509CertificateInner);

impl X509Certificate {
    /// Parse an X.509 certificate from DER-encoded bytes.
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        sys::X509CertificateInner::from_der(data).map(Self)
    }

    /// Extract the public key from this certificate.
    pub fn public_key(&self) -> Result<crate::rsa::RsaPublicKey, crate::rsa::RsaError> {
        self.0.public_key()
    }

    /// Verify the signature of this certificate against the given issuer's
    /// public key.
    /// Returns `Ok(true)` if the signature is valid, `Ok(false)` if the
    /// signature is invalid, or an error for other failures.
    pub fn verify(
        &self,
        issuer_public_key: &crate::rsa::RsaPublicKey,
    ) -> Result<bool, crate::rsa::RsaError> {
        self.0.verify(issuer_public_key)
    }

    /// Check if this certificate (acting as issuer) issued `subject`.
    ///
    /// This performs only deterministic structural comparisons - it does not
    /// cryptographically verify the issuer's signature on `subject`. Callers
    /// that need to establish a trust relationship must additionally call
    /// [`X509Certificate::verify`] with the issuer's public key.
    pub fn issued(&self, subject: &X509Certificate) -> Result<bool, X509Error> {
        self.0.issued(&subject.0)
    }

    /// Encode this certificate as DER bytes.
    pub fn to_der(&self) -> Result<Vec<u8>, X509Error> {
        self.0.to_der()
    }

    #[cfg(any(test, feature = "test_helpers"))]
    /// Build a self-signed never-expiring X.509 certificate (for testing).
    pub fn build_self_signed(
        key: &crate::rsa::RsaKeyPair,
        country: &str,
        state: &str,
        locality: &str,
        organization: &str,
        common_name: &str,
    ) -> anyhow::Result<Self> {
        sys::X509CertificateInner::build_self_signed(
            key,
            country,
            state,
            locality,
            organization,
            common_name,
        )
        .map(Self)
    }

    /// Get the Common Name (CN) from the X.509 certificate's subject name. If
    /// there are multiple CNs present, return the first.
    pub fn subject_common_name(&self) -> Result<Option<String>, X509Error> {
        self.0.subject_common_name()
    }

    /// String representation of the certificate's issuer Distinguished Name.
    pub fn issuer_dn(&self) -> Result<String, X509Error> {
        self.0.issuer_dn()
    }

    /// Raw bytes of the certificate's serial number, as encoded in the
    /// underlying certificate.
    pub fn serial_number(&self) -> Result<Vec<u8>, X509Error> {
        self.0.serial_number()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_cert(key: &crate::rsa::RsaKeyPair) -> X509Certificate {
        X509Certificate::build_self_signed(
            key,
            "US",
            "Washington",
            "Redmond",
            "Test",
            "test.example.com",
        )
        .unwrap()
    }

    /// Build a self-signed cert and verify it against its own public key.
    /// This catches the previously-latent symcrypt bug where the backend
    /// passed the raw TBS bytes to `pkcs1_verify` instead of the SHA-256
    /// digest of the TBS bytes — a self-signed cert would have failed to
    /// verify against its own key.
    #[test]
    fn self_signed_cert_verifies() {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = build_test_cert(&key);
        assert!(cert.verify(&key).unwrap());
        assert!(cert.issued(&cert).unwrap());
    }

    /// A cert signed by one key must NOT verify against an unrelated key.
    #[test]
    fn cert_rejects_wrong_issuer_key() {
        let signing_key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let other_key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = build_test_cert(&signing_key);
        let other_pubkey = X509Certificate::build_self_signed(
            &other_key,
            "US",
            "Washington",
            "Redmond",
            "Other",
            "other.example.com",
        )
        .unwrap()
        .public_key()
        .unwrap();
        // Invalid signatures must be reported as `Ok(false)`, not `Err`.
        assert!(matches!(cert.verify(&other_pubkey), Ok(false)));
    }

    /// DER round-trip preserves the cert and its signature.
    #[test]
    fn der_roundtrip() {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = build_test_cert(&key);
        let der = cert.to_der().unwrap();
        let reparsed = X509Certificate::from_der(&der).unwrap();
        let pubkey = reparsed.public_key().unwrap();
        assert!(reparsed.verify(&pubkey).unwrap());
    }

    #[test]
    fn subject_common_name() {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = build_test_cert(&key);
        let sn = cert.subject_common_name().unwrap().unwrap();
        assert_eq!(sn, "test.example.com");
    }
}
