// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 signed data verification.

mod ossl;
use ossl as sys;

use thiserror::Error;

/// A parsed X509 certificate.
pub struct X509Certificate(sys::X509CertificateInner);

/// A parsed PKCS#7 signedData object.
pub struct Pkcs7SignedData(sys::Pkcs7SignedDataInner);

/// An error for PKCS#7 parsing operations.
#[derive(Clone, Debug, Error)]
#[error("PKCS#7 parse error")]
pub struct Pkcs7Error(#[source] super::BackendError);

/// An error for X509 certificate parsing operations.
#[derive(Clone, Debug, Error)]
#[error("X509 certificate parse error")]
pub struct X509CertificateError(#[source] super::BackendError);

/// An error for PKCS#7 verification setup operations.
#[derive(Clone, Debug, Error)]
#[error("PKCS#7 verify error")]
pub struct Pkcs7VerifyError(#[source] super::BackendError);

impl X509Certificate {
    /// Parses a DER-encoded X509 certificate.
    pub fn from_der(data: &[u8]) -> Result<Self, X509CertificateError> {
        sys::X509CertificateInner::from_der(data).map(Self)
    }
}

impl Pkcs7SignedData {
    /// Parses a DER-encoded PKCS#7 signedData object.
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        sys::Pkcs7SignedDataInner::from_der(data).map(Self)
    }

    /// Verifies signed data against trusted certificates.
    ///
    /// Returns `Ok(true)` when verification succeeds and `Ok(false)` when the
    /// signature check fails.
    pub fn verify(
        &self,
        trusted_certs: &[X509Certificate],
        signed_content: &[u8],
    ) -> Result<bool, Pkcs7VerifyError> {
        self.0.verify(
            &trusted_certs.iter().map(|c| &c.0).collect::<Vec<_>>(),
            signed_content,
        )
    }
}
