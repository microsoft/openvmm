// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 signed data verification.

#![cfg(any(
    openssl,
    all(native, windows),
    all(native, target_os = "macos"),
    rust,
    symcrypt
))]

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

#[cfg(any(rust, symcrypt))]
mod symcrypt_rust;
#[cfg(any(rust, symcrypt))]
use symcrypt_rust as sys;

use thiserror::Error;

/// A parsed PKCS#7 signedData object.
pub struct Pkcs7SignedData(sys::Pkcs7SignedDataInner);

/// A store of trusted X509 certificates used for PKCS#7 verification.
pub struct Pkcs7CertStore(sys::Pkcs7CertStoreInner);

/// An error for PKCS#7 operations.
#[cfg(not(rust))]
#[derive(Clone, Debug, Error)]
#[error("PKCS#7 error")]
pub struct Pkcs7Error(#[source] super::BackendError);

/// An error for PKCS#7 operations.
#[cfg(rust)]
#[derive(Clone, Debug, Error)]
#[error("PKCS#7 error during {1}")]
pub struct Pkcs7Error(#[source] der::Error, &'static str);

impl Pkcs7CertStore {
    /// Creates a new empty certificate store.
    pub fn new() -> Result<Self, Pkcs7Error> {
        sys::Pkcs7CertStoreInner::new().map(Self)
    }

    /// Adds an X509 certificate to the store.
    #[cfg(any(openssl, rust, symcrypt))]
    pub fn add_cert(&mut self, cert: &super::x509::X509Certificate) -> Result<(), Pkcs7Error> {
        self.0.add_cert(cert)
    }

    /// Adds a DER-encoded X509 certificate to the store.
    // TODO: Remove this method and make every backend support add_cert.
    pub fn add_cert_der(&mut self, data: &[u8]) -> Result<(), Pkcs7Error> {
        #[cfg(any(openssl, symcrypt))]
        {
            self.0.add_cert(
                &crate::x509::X509Certificate::from_der(data).map_err(|e| Pkcs7Error(e.0))?,
            )
        }
        #[cfg(rust)]
        {
            self.0.add_cert(
                &crate::x509::X509Certificate::from_der(data).map_err(|e| Pkcs7Error(e.0, e.1))?,
            )
        }
        #[cfg(not(any(openssl, rust, symcrypt)))]
        {
            self.0.add_cert_der(data)
        }
    }
}

impl Pkcs7SignedData {
    /// Parses a DER-encoded PKCS#7 signedData object.
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        sys::Pkcs7SignedDataInner::from_der(data).map(Self)
    }

    /// Encode this PKCS#7 object as DER bytes.
    #[cfg(any(openssl, rust, symcrypt))]
    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        self.0.to_der()
    }

    /// Creates a detached PKCS#7 signed-data object by signing `data` with the
    /// given certificate and key pair.
    #[cfg(any(openssl, rust, symcrypt))]
    pub fn sign(
        cert: &super::x509::X509Certificate,
        key_pair: &super::rsa::RsaKeyPair,
        data: &[u8],
    ) -> Result<Self, crate::rsa::RsaError> {
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
    #[cfg(not(any(rust, symcrypt)))]
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

#[cfg(all(test, openssl))]
mod tests {
    use super::*;

    /// The detached content used by the cross-backend verification tests.
    const SIGNED_CONTENT: &[u8] = b"openvmm pkcs7 cross-backend test data";

    /// Build a self-signed cert + detached PKCS#7 signature over
    /// `SIGNED_CONTENT`. Used to generate fresh fixtures inside each test
    /// rather than reading them from disk.
    fn make_fixture() -> (crate::x509::X509Certificate, Pkcs7SignedData) {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = crate::x509::X509Certificate::build_self_signed(
            &key,
            "US",
            "WA",
            "Redmond",
            "OpenVMM Test",
            "pkcs7.test.openvmm",
        )
        .unwrap();
        let p7 = Pkcs7SignedData::sign(&cert, &key, SIGNED_CONTENT).unwrap();
        (cert, p7)
    }

    /// Garbage input must not panic and must surface as an `Err`. This
    /// guards against unchecked DER parsing across backends.
    #[test]
    fn from_der_rejects_garbage() {
        assert!(Pkcs7SignedData::from_der(&[]).is_err());
        assert!(Pkcs7SignedData::from_der(&[0xff, 0x00, 0x01, 0x02]).is_err());
    }

    /// A PKCS#7 detached signature must verify against the issuing cert
    /// in a UEFI-mode store across all supported backends. This is the
    /// main cross-backend coverage for the verify path.
    #[test]
    fn verify_with_trusted_cert_succeeds() {
        let (cert, p7) = make_fixture();
        let mut store = Pkcs7CertStore::new().unwrap();
        store.add_cert(&cert).unwrap();
        assert!(p7.verify(store, SIGNED_CONTENT, true).unwrap());
    }

    /// Helper: verification failures may surface as either `Ok(false)`
    /// or `Err(_)` depending on the backend, per the documented
    /// contract on `Pkcs7SignedData::verify`. Both are acceptable; the
    /// only forbidden outcome is `Ok(true)`.
    fn assert_verify_rejected(result: Result<bool, Pkcs7Error>) {
        match result {
            Ok(false) | Err(_) => {}
            Ok(true) => panic!("verify unexpectedly succeeded"),
        }
    }

    /// Verifying with content that does not match the signed digest
    /// must not succeed. This is the contract that
    /// `authenticate_variable` in the UEFI nvram service relies on.
    #[test]
    fn verify_rejects_tampered_content() {
        let (cert, p7) = make_fixture();
        let mut store = Pkcs7CertStore::new().unwrap();
        store.add_cert(&cert).unwrap();
        let mut tampered = SIGNED_CONTENT.to_vec();
        tampered[0] ^= 0xff;
        assert_verify_rejected(p7.verify(store, &tampered, true));
    }

    /// A truncated detached content must not verify.
    #[test]
    fn verify_rejects_truncated_content() {
        let (cert, p7) = make_fixture();
        let mut store = Pkcs7CertStore::new().unwrap();
        store.add_cert(&cert).unwrap();
        let truncated = &SIGNED_CONTENT[..SIGNED_CONTENT.len() - 1];
        assert_verify_rejected(p7.verify(store, truncated, true));
    }

    /// An empty trust store must cause verification to fail (no trust
    /// anchor available).
    #[test]
    fn verify_with_empty_store_fails() {
        let (_cert, p7) = make_fixture();
        let store = Pkcs7CertStore::new().unwrap();
        assert_verify_rejected(p7.verify(store, SIGNED_CONTENT, true));
    }

    /// A store populated only with an unrelated cert must cause
    /// verification to fail.
    #[test]
    fn verify_with_unrelated_cert_fails() {
        let (_cert, p7) = make_fixture();
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let other = crate::x509::X509Certificate::build_self_signed(
            &key,
            "US",
            "WA",
            "Redmond",
            "Other",
            "other.test.openvmm",
        )
        .unwrap();
        let mut store = Pkcs7CertStore::new().unwrap();
        store.add_cert(&other).unwrap();
        assert_verify_rejected(p7.verify(store, SIGNED_CONTENT, true));
    }

    /// Full sign + verify roundtrip using freshly generated keys.
    #[test]
    fn sign_verify_roundtrip() {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = crate::x509::X509Certificate::build_self_signed(
            &key,
            "US",
            "WA",
            "Redmond",
            "Roundtrip",
            "roundtrip.test.openvmm",
        )
        .unwrap();
        let content = b"hello pkcs7 roundtrip";
        let p7 = Pkcs7SignedData::sign(&cert, &key, content).unwrap();
        let mut store = Pkcs7CertStore::new().unwrap();
        store.add_cert(&cert).unwrap();
        assert!(p7.verify(store, content, true).unwrap());
    }

    /// Cross-check: a freshly produced signature must fail verification
    /// against a different cert's store.
    #[test]
    fn sign_verify_rejects_wrong_store() {
        let signer_key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let signer_cert = crate::x509::X509Certificate::build_self_signed(
            &signer_key,
            "US",
            "WA",
            "Redmond",
            "Signer",
            "signer.test.openvmm",
        )
        .unwrap();
        let other_key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let other_cert = crate::x509::X509Certificate::build_self_signed(
            &other_key,
            "US",
            "WA",
            "Redmond",
            "Other",
            "other.test.openvmm",
        )
        .unwrap();
        let content = b"wrong-store content";
        let p7 = Pkcs7SignedData::sign(&signer_cert, &signer_key, content).unwrap();
        let mut store = Pkcs7CertStore::new().unwrap();
        store.add_cert(&other_cert).unwrap();
        assert_verify_rejected(p7.verify(store, content, true));
    }
}
