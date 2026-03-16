// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate operations.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// An X.509 certificate.
pub struct X509Certificate {
    pub(crate) inner: sys::X509CertificateInner,
}

/// A public key extracted from an X.509 certificate.
pub struct PublicKey(sys::PublicKeyInner);

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublicKey")
            .field("key_type", &self.key_type())
            .finish()
    }
}

/// The type of a public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    /// RSA
    Rsa,
    /// Other key type
    Other,
}

/// Error from X.509 operations.
#[derive(Debug, Error)]
#[error("X.509 error")]
pub struct X509Error(#[source] super::BackendError);

impl X509Certificate {
    /// Parse an X.509 certificate from DER-encoded bytes.
    pub fn from_der(der: &[u8]) -> Result<Self, X509Error> {
        sys::x509_from_der(der).map(|inner| Self { inner })
    }

    /// Extract the public key from the certificate.
    pub fn public_key(&self) -> Result<PublicKey, X509Error> {
        self.inner.public_key().map(PublicKey)
    }

    /// Verify that this certificate's signature was made by the given key.
    pub fn verify(&self, key: &PublicKey) -> Result<bool, X509Error> {
        self.inner.verify(&key.0)
    }

    /// Check whether this certificate issued `child` (subject/issuer match).
    pub fn issued(&self, child: &X509Certificate) -> bool {
        self.inner.issued(&child.inner)
    }
}

impl PublicKey {
    /// Get the key type.
    pub fn key_type(&self) -> KeyType {
        self.0.key_type()
    }

    /// Verify an RSA-SHA256 signature over `data`.
    pub fn verify_rsa_sha256(&self, data: &[u8], signature: &[u8]) -> Result<bool, X509Error> {
        self.0.verify_rsa_sha256(data, signature)
    }
}
