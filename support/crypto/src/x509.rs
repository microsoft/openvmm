// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate operations.

use thiserror::Error;

/// An X.509 certificate.
pub struct X509Certificate {
    pub(crate) inner: openssl::x509::X509,
}

/// A public key extracted from an X.509 certificate.
pub struct PublicKey {
    pub(crate) inner: openssl::pkey::PKey<openssl::pkey::Public>,
}

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
pub enum X509Error {
    #[error("failed to parse X.509 certificate from DER")]
    ParseDer(#[source] openssl::error::ErrorStack),
    #[error("failed to get public key from certificate")]
    GetPublicKey(#[source] openssl::error::ErrorStack),
    #[error("failed to verify certificate signature")]
    Verify(#[source] openssl::error::ErrorStack),
}

impl X509Certificate {
    /// Parse an X.509 certificate from DER-encoded bytes.
    pub fn from_der(der: &[u8]) -> Result<Self, X509Error> {
        let inner = openssl::x509::X509::from_der(der).map_err(X509Error::ParseDer)?;
        Ok(Self { inner })
    }

    /// Extract the public key from the certificate.
    pub fn public_key(&self) -> Result<PublicKey, X509Error> {
        let inner = self.inner.public_key().map_err(X509Error::GetPublicKey)?;
        Ok(PublicKey { inner })
    }

    /// Verify that this certificate's signature was made by the given key.
    pub fn verify(&self, key: &PublicKey) -> Result<bool, X509Error> {
        self.inner.verify(&key.inner).map_err(X509Error::Verify)
    }

    /// Check whether this certificate issued `child` (subject/issuer match).
    pub fn issued(&self, child: &X509Certificate) -> bool {
        self.inner.issued(&child.inner) == openssl::x509::X509VerifyResult::OK
    }
}

impl PublicKey {
    /// Get the key type.
    pub fn key_type(&self) -> KeyType {
        if self.inner.id() == openssl::pkey::Id::RSA {
            KeyType::Rsa
        } else {
            KeyType::Other
        }
    }

    /// Verify an RSA-SHA256 signature over `data`.
    pub fn verify_rsa_sha256(&self, data: &[u8], signature: &[u8]) -> Result<bool, X509Error> {
        let mut verifier =
            openssl::sign::Verifier::new(openssl::hash::MessageDigest::sha256(), &self.inner)
                .map_err(X509Error::Verify)?;
        verifier
            .set_rsa_padding(openssl::rsa::Padding::PKCS1)
            .map_err(X509Error::Verify)?;
        verifier.update(data).map_err(X509Error::Verify)?;
        verifier.verify(signature).map_err(X509Error::Verify)
    }
}
