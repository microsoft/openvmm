// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 signed data verification for UEFI Secure Boot authenticated variables.
//!
//! This module intentionally implements only the PKCS#7/CMS verification shape
//! needed by UEFI Secure Boot signature database checks. It is not a general
//! purpose PKCS#7 verifier and should not be used for other PKCS#7 use cases.

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

use crate::x509::X509Certificate;
use thiserror::Error;

/// A parsed PKCS#7 signedData object.
pub struct Pkcs7SignedData(sys::Pkcs7SignedDataInner);

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

/// An error encountered while verifying a PKCS#7 signed data object.
// TODO: Make this Clone when RsaError becomes Clone
#[derive(Debug, Error)]
pub enum Pkcs7VerifyError {
    /// A PKCS#7 parsing or structural error.
    #[error("PKCS#7 error")]
    Pkcs7(#[from] Pkcs7Error),
    /// An RSA signature verification error.
    #[error("RSA error")]
    Rsa(#[from] crate::rsa::RsaError),
    /// An X.509 certificate error.
    #[error("X509 error")]
    X509(#[from] crate::x509::X509Error),
}

impl Pkcs7SignedData {
    /// Parses a DER-encoded PKCS#7 signedData object.
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        sys::Pkcs7SignedDataInner::from_der(data).map(Self)
    }

    /// Encode this PKCS#7 object as DER bytes.
    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        self.0.to_der()
    }

    /// Creates a detached PKCS#7 signed-data object by signing `data` with the
    /// given certificate and key pair.
    #[cfg(any(test, feature = "test_helpers"))]
    pub fn sign(
        cert: &X509Certificate,
        key_pair: &crate::rsa::RsaKeyPair,
        data: &[u8],
    ) -> Result<Self, crate::rsa::RsaError> {
        sys::Pkcs7SignedDataInner::sign(cert, key_pair, data).map(Self)
    }

    /// Returns the first signer's embedded certificate and their signature.
    /// Errors if there are no or multiple signers.
    #[cfg(not(openssl))]
    pub fn signer_cert_sig(&self) -> Result<(X509Certificate, Vec<u8>), Pkcs7Error> {
        self.0.signer_cert_sig()
    }

    /// Returns every certificate embedded in the PKCS#7 SignedData's
    /// certificate bag.
    #[cfg(not(openssl))]
    pub fn embedded_certificates(&self) -> Result<Vec<X509Certificate>, Pkcs7Error> {
        self.0.embedded_certificates()
    }

    /// Verifies UEFI Secure Boot signed data against trusted certificates.
    ///
    /// This is a narrow verifier for UEFI authenticated variable / Secure Boot
    /// signature-list semantics. It does not provide full PKCS#7/CMS support
    /// and should not be used for other PKCS#7 verification scenarios.
    ///
    /// Returns `Ok(true)` when verification succeeds and `Ok(false)` when the
    /// signature, signer, or trust check fails. No certificate revocation
    /// checking is performed.
    pub fn verify_uefi(
        &self,
        trusted_certs: &[X509Certificate],
        signed_content: &[u8],
    ) -> Result<bool, Pkcs7VerifyError> {
        verify_inner(self, trusted_certs, signed_content)
    }
}

#[cfg(openssl)]
fn verify_inner(
    p7: &Pkcs7SignedData,
    trusted_certs: &[X509Certificate],
    signed_content: &[u8],
) -> Result<bool, Pkcs7VerifyError> {
    p7.0.verify(trusted_certs, signed_content)
}

/// Shared UEFI PKCS#7 verification flow.
///
/// 1. Require exactly one signer.
/// 2. Verify the signer's signature over the detached content.
/// 3. Walk the chain starting at the signer cert: at each step, succeed
///    if the current cert is itself trusted or is issued (and validly
///    signed) by a trusted cert.
#[cfg(not(openssl))]
fn verify_inner(
    p7: &Pkcs7SignedData,
    trusted_certs: &[X509Certificate],
    signed_content: &[u8],
) -> Result<bool, Pkcs7VerifyError> {
    let (signer, signature) = p7.signer_cert_sig()?;
    if !signer.public_key()?.pkcs1_verify(
        signed_content,
        &signature,
        crate::HashAlgorithm::Sha256,
    )? {
        return Ok(false);
    }

    #[derive(PartialEq, Eq, Hash, Clone)]
    struct CertId {
        issuer_dn: String,
        serial_number: Vec<u8>,
    }
    fn make_id(cert: &X509Certificate) -> Result<CertId, crate::x509::X509Error> {
        Ok(CertId {
            issuer_dn: cert.issuer_dn()?,
            serial_number: cert.serial_number()?,
        })
    }

    let embedded = p7.embedded_certificates()?;
    // Walk the chain. At each step:
    //   - succeed if `current` equals any trusted cert,
    //   - else succeed if any trusted cert issued `current` and that
    //     issuer's signature on `current` verifies,
    //   - else step up to an embedded cert that issued `current` (and
    //     whose signature verifies).
    let mut visited = std::collections::HashSet::new();
    let mut current = &signer;
    'outer: loop {
        let current_id = make_id(current)?;
        if !visited.insert(current_id.clone()) {
            return Ok(false);
        }

        for trusted in trusted_certs {
            let trusted_id = make_id(trusted)?;
            if trusted_id == current_id
                || (trusted.issued(current)? && current.verify(&trusted.public_key()?)?)
            {
                return Ok(true);
            }
        }

        for candidate in &embedded {
            if candidate.issued(current)? && current.verify(&candidate.public_key()?)? {
                current = candidate;
                continue 'outer;
            }
        }

        return Ok(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rsa::RsaKeyPair;

    /// The detached content used by the cross-backend verification tests.
    const SIGNED_CONTENT: &[u8] = b"openvmm pkcs7 cross-backend test data";

    /// Build a self-signed cert + detached PKCS#7 signature over
    /// `SIGNED_CONTENT`. Used to generate fresh fixtures inside each test
    /// rather than reading them from disk.
    fn make_fixture() -> (X509Certificate, Pkcs7SignedData) {
        let key = RsaKeyPair::generate(2048).unwrap();
        let cert = X509Certificate::build_self_signed(
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

    /// A PKCS#7 detached signature must verify against the issuing cert using
    /// UEFI Secure Boot trust semantics across all supported backends. This is
    /// the main cross-backend coverage for the UEFI verification path.
    #[test]
    fn verify_with_trusted_cert_succeeds() {
        let (cert, p7) = make_fixture();
        assert!(p7.verify_uefi(&[cert], SIGNED_CONTENT).unwrap());
    }

    /// Helper: verification failures must surface as `Ok(false)`. Backend
    /// errors are reserved for exceptional conditions, not rejected signatures
    /// or trust failures.
    fn assert_verify_rejected(result: Result<bool, Pkcs7VerifyError>) {
        match result {
            Ok(false) => {}
            Err(err) => panic!("verify returned unexpected error: {err}"),
            Ok(true) => panic!("verify unexpectedly succeeded"),
        }
    }

    /// Verifying with content that does not match the signed digest
    /// must not succeed. This is the contract that
    /// `authenticate_variable` in the UEFI nvram service relies on.
    #[test]
    fn verify_rejects_tampered_content() {
        let (cert, p7) = make_fixture();
        let mut tampered = SIGNED_CONTENT.to_vec();
        tampered[0] ^= 0xff;
        assert_verify_rejected(p7.verify_uefi(&[cert], &tampered));
    }

    /// A truncated detached content must not verify.
    #[test]
    fn verify_rejects_truncated_content() {
        let (cert, p7) = make_fixture();
        let truncated = &SIGNED_CONTENT[..SIGNED_CONTENT.len() - 1];
        assert_verify_rejected(p7.verify_uefi(&[cert], truncated));
    }

    /// An empty trust store must cause verification to fail (no trust
    /// anchor available).
    #[test]
    fn verify_with_empty_store_fails() {
        let (_cert, p7) = make_fixture();
        assert_verify_rejected(p7.verify_uefi(&[], SIGNED_CONTENT));
    }

    /// Verification should try every trusted certificate supplied by the
    /// caller. An unrelated cert must not prevent a later matching cert from
    /// succeeding.
    #[test]
    fn verify_with_later_trusted_cert_succeeds() {
        let (cert, p7) = make_fixture();
        let key = RsaKeyPair::generate(2048).unwrap();
        let other = X509Certificate::build_self_signed(
            &key,
            "US",
            "WA",
            "Redmond",
            "Other",
            "other.test.openvmm",
        )
        .unwrap();
        let certs = vec![other, cert];

        assert!(p7.verify_uefi(&certs, SIGNED_CONTENT).unwrap());
    }

    /// A store populated only with an unrelated cert must cause
    /// verification to fail.
    #[test]
    fn verify_with_unrelated_cert_fails() {
        let (_cert, p7) = make_fixture();
        let key = RsaKeyPair::generate(2048).unwrap();
        let other = X509Certificate::build_self_signed(
            &key,
            "US",
            "WA",
            "Redmond",
            "Other",
            "other.test.openvmm",
        )
        .unwrap();
        assert_verify_rejected(p7.verify_uefi(&[other], SIGNED_CONTENT));
    }

    /// Full sign + UEFI verification roundtrip using freshly generated keys.
    #[test]
    fn sign_verify_roundtrip() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let cert = X509Certificate::build_self_signed(
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
        assert!(p7.verify_uefi(&[cert], content).unwrap());
    }

    /// Cross-check: a freshly produced signature must fail verification
    /// against a different cert's store.
    #[test]
    fn sign_verify_rejects_wrong_store() {
        let signer_key = RsaKeyPair::generate(2048).unwrap();
        let signer_cert = X509Certificate::build_self_signed(
            &signer_key,
            "US",
            "WA",
            "Redmond",
            "Signer",
            "signer.test.openvmm",
        )
        .unwrap();
        let other_key = RsaKeyPair::generate(2048).unwrap();
        let other_cert = X509Certificate::build_self_signed(
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
        assert_verify_rejected(p7.verify_uefi(&[other_cert], content));
    }
}
