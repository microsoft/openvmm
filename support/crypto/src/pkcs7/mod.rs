// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 signed data verification.

mod ossl;
use ossl as sys;

use thiserror::Error;

/// A parsed PKCS#7 signedData object.
pub struct Pkcs7SignedData(sys::Pkcs7SignedDataInner);

/// A store of trusted X509 certificates used for PKCS#7 verification.
pub struct Pkcs7CertStore(sys::Pkcs7CertStoreInner);

/// An error for PKCS#7 operations.
#[derive(Clone, Debug, Error)]
#[error("PKCS#7 error")]
pub struct Pkcs7Error(#[source] super::BackendError);

impl Pkcs7CertStore {
    /// Creates a new empty certificate store.
    pub fn new() -> Result<Self, Pkcs7Error> {
        sys::Pkcs7CertStoreInner::new().map(Self)
    }

    /// Adds a DER-encoded X509 certificate to the store.
    pub fn add_cert_der(&mut self, data: &[u8]) -> Result<(), Pkcs7Error> {
        self.0.add_cert_der(data)
    }
}

impl Pkcs7SignedData {
    /// Parses a DER-encoded PKCS#7 signedData object.
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        sys::Pkcs7SignedDataInner::from_der(data).map(Self)
    }

    /// Encode this PKCS#7 object as DER bytes.
    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        self.0.to_der()
    }

    /// Creates a PKCS#7 signed-data object by signing `data` with the given
    /// certificate and key pair.
    pub fn sign(
        cert: &super::x509::X509Certificate,
        key_pair: &super::rsa::RsaKeyPair,
        data: &[u8],
    ) -> Result<Self, Pkcs7Error> {
        sys::Pkcs7SignedDataInner::sign(cert, key_pair, data).map(Self)
    }

    /// Verifies signed data against a trusted certificate store.
    ///
    /// Consumes the store, since the backend may need to finalize it.
    ///
    /// The `uefi_mode` flag weakens verification behavior to match UEFI's requirements.
    ///
    /// Returns `Ok(true)` when verification succeeds and `Ok(false)` when the
    /// signature check fails.
    pub fn verify(
        &self,
        store: Pkcs7CertStore,
        signed_content: &[u8],
        uefi_mode: bool,
    ) -> Result<bool, Pkcs7Error> {
        self.0.verify(store.0, signed_content, uefi_mode)
    }
}
