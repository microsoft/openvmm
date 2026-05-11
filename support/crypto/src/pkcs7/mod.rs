// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 signed data verification.

#![cfg(any(openssl, all(native, windows), all(native, target_os = "macos")))]

#[cfg(openssl)]
mod ossl;
#[cfg(openssl)]
use ossl as sys;

#[cfg(all(native, windows))]
mod win;
#[cfg(all(native, windows))]
use win as sys;

#[cfg(all(native, target_os = "macos"))]
mod mac;
#[cfg(all(native, target_os = "macos"))]
use mac as sys;

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
    #[cfg(openssl)]
    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        self.0.to_der()
    }

    /// Creates a PKCS#7 signed-data object by signing `data` with the given
    /// certificate and key pair.
    #[cfg(openssl)]
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
    /// Returns `Ok(true)` when verification succeeds. Different backends may
    /// return `Ok(false)` or an `Err` when the signature check fails.
    ///
    /// No certificate revocation checking is performed.
    ///
    /// # `uefi_mode`
    ///
    /// When `false`, verification uses the backend's default PKI rules: the
    /// signer must chain up to a root certificate in `store`, all certs in
    /// the chain must be currently time-valid, and the chain must be valid
    /// for the default purpose.
    ///
    /// When `true`, the following relaxations are applied so that PKCS#7
    /// signatures can be verified against the certificates found in a UEFI
    /// `EFI_SIGNATURE_LIST` (`db`/`dbx`/`KEK`/`PK`):
    ///
    /// 1. **Partial chains are accepted.** Any certificate in `store` is
    ///    treated as a trust anchor, not just self-signed roots. UEFI
    ///    signature lists typically contain leaf or intermediate certs with
    ///    no full chain available to the verifier.
    /// 2. **Certificate time validity is ignored.** Expired certificates are
    ///    accepted. UEFI signing certs in the wild are often long expired
    ///    and existing firmware implementations accept them.
    /// 3. **Any key-usage / extended-key-usage is accepted.** UEFI signature
    ///    list certs are not marked with the usages that a general-purpose
    ///    PKI verifier expects for the default purpose.
    pub fn verify(
        self,
        store: Pkcs7CertStore,
        signed_content: &[u8],
        uefi_mode: bool,
    ) -> Result<bool, Pkcs7Error> {
        // Our only caller of this method today, uefi, always wants 'uefi_mode'.
        // set to true. Behavior of our current backends is known to be subtly
        // different when uefi_mode is false. If a caller ever needs support for
        // uefi_mode = false, the backend implementation will need to be updated
        // to handle the stricter PKI rules.
        //
        // Specifically known is that the handling of the x509 purpose (EKU)
        // constraints has different defaults on different backends, but there
        // may be other subtle differences as well.
        assert!(uefi_mode, "only uefi_mode is currently supported");
        self.0.verify(store.0, signed_content, uefi_mode)
    }
}
