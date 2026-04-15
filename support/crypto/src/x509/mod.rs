// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate operations.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// An error for X.509 operations.
#[derive(Debug, Error)]
#[error("X.509 error")]
pub struct X509Error(#[source] super::BackendError);

/// An X.509 certificate.
pub struct X509Certificate(pub(crate) sys::X509CertificateInner);

impl X509Certificate {
    /// Parse an X.509 certificate from DER-encoded bytes.
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        sys::X509CertificateInner::from_der(data).map(Self)
    }

    /// Extract the public key from this certificate.
    pub fn public_key(&self) -> Result<super::rsa::RsaPublicKey, X509Error> {
        self.0.public_key()
    }

    /// Verify the signature of this certificate against the given issuer's
    /// public key.
    pub fn verify(&self, issuer_public_key: &super::rsa::RsaPublicKey) -> Result<bool, X509Error> {
        self.0.verify(issuer_public_key)
    }

    /// Check if this certificate (acting as issuer) issued `subject`.
    pub fn issued(&self, subject: &X509Certificate) -> bool {
        self.0.issued(&subject.0)
    }

    /// Encode this certificate as DER bytes.
    pub fn to_der(&self) -> Result<Vec<u8>, X509Error> {
        self.0.to_der()
    }
}

/// Builder for creating self-signed X.509 certificates (for testing).
pub struct X509Builder(sys::X509BuilderInner);

impl X509Builder {
    /// Create a new X.509 certificate builder.
    pub fn new() -> Result<Self, X509Error> {
        sys::X509BuilderInner::new().map(Self)
    }

    /// Set the public key from an RSA key pair.
    pub fn set_pubkey_from_rsa_key_pair(
        &mut self,
        key_pair: &super::rsa::RsaKeyPair,
    ) -> Result<(), X509Error> {
        self.0.set_pubkey_from_rsa_key_pair(key_pair)
    }

    /// Set the subject and issuer name with common certificate fields (for self-signed certificates).
    pub fn set_subject_and_issuer_name(
        &mut self,
        country: &str,
        state: &str,
        locality: &str,
        organization: &str,
        common_name: &str,
    ) -> Result<(), X509Error> {
        self.0
            .set_subject_and_issuer_name(country, state, locality, organization, common_name)
    }

    /// Set the validity period in days from now.
    pub fn set_validity_days(&mut self, days: u32) -> Result<(), X509Error> {
        self.0.set_validity_days(days)
    }

    /// Sign the certificate with the given RSA private key and build it.
    pub fn sign_and_build(
        self,
        key_pair: &super::rsa::RsaKeyPair,
    ) -> Result<X509Certificate, X509Error> {
        self.0.sign_and_build(key_pair).map(X509Certificate)
    }
}
