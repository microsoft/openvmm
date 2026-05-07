// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate operations.

#![cfg(any(openssl, symcrypt))]

#[cfg(openssl)]
mod ossl;
#[cfg(openssl)]
use ossl as sys;

#[cfg(symcrypt)]
mod symcrypt;
#[cfg(symcrypt)]
use symcrypt as sys;

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
    pub fn public_key(&self) -> Result<crate::rsa::RsaPublicKey, X509Error> {
        self.0.public_key()
    }

    /// Verify the signature of this certificate against the given issuer's
    /// public key.
    /// Different backends may return Ok(false) or an error if the signature is invalid, but all return an error for other failures.
    pub fn verify(&self, issuer_public_key: &crate::rsa::RsaPublicKey) -> Result<bool, X509Error> {
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
}
