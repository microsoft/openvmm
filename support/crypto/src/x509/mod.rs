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
pub(crate) mod mac;
#[cfg(all(native, target_os = "macos"))]
use mac as sys;

use thiserror::Error;

/// An error reading or converting certificate validity bounds.
#[derive(Debug, Error)]
pub enum X509ValidityError {
    /// A certificate backend error.
    #[error("failed to read certificate validity")]
    X509(#[from] X509Error),
    /// A validity bound outside the supported timestamp range.
    #[error("invalid certificate validity timestamp")]
    Timestamp(#[from] jiff::Error),
}

/// An error for X.509 operations.
#[cfg(not(rust))]
#[derive(Clone, Debug, Error)]
#[error("X.509 operation failed")]
pub struct X509Error(#[source] pub(crate) super::BackendError);

/// An error for X.509 operations.
#[cfg(rust)]
#[derive(Clone, Debug, Error)]
#[error("X.509 operation failed while {1}")]
pub struct X509Error(#[source] pub(crate) der::Error, pub(crate) &'static str);

/// An error encountered while verifying an X.509 certificate signature.
#[derive(Debug, Error)]
pub enum X509VerifyError {
    /// A certificate parsing or encoding error.
    #[error("failed to process the certificate")]
    X509(#[from] X509Error),
    /// An RSA signature verification error.
    #[error("failed to verify the RSA certificate signature")]
    Rsa(#[from] crate::rsa::RsaError),
    /// An ECDSA signature verification error.
    #[cfg(any(openssl, symcrypt, all(native, windows)))]
    #[error("failed to verify the ECDSA certificate signature")]
    Ecdsa(#[from] crate::ecdsa::EcdsaError),
}

/// A public key extracted from a certificate or used to verify its signature.
pub enum X509PublicKey {
    /// An RSA public key.
    Rsa(crate::rsa::RsaPublicKey),
    /// An ECDSA public key.
    #[cfg(any(openssl, symcrypt, all(native, windows)))]
    Ecdsa(crate::ecdsa::EcdsaPublicKey),
}

impl X509PublicKey {
    /// Extract the RSA public key, or `None` if the certificate's key is not an
    /// RSA key.
    pub fn rsa(self) -> Option<crate::rsa::RsaPublicKey> {
        match self {
            X509PublicKey::Rsa(key) => Some(key),
            #[cfg(any(openssl, symcrypt, all(native, windows)))]
            _ => None,
        }
    }

    /// Extract the ECDSA public key, or `None` if the certificate's key is not
    /// an ECDSA key.
    #[cfg(any(openssl, symcrypt, all(native, windows)))]
    pub fn ecdsa(self) -> Option<crate::ecdsa::EcdsaPublicKey> {
        match self {
            X509PublicKey::Ecdsa(key) => Some(key),
            _ => None,
        }
    }
}

/// A borrowed key pair used for X.509 operations.
#[derive(Clone, Copy)]
pub enum X509KeyPair<'a> {
    /// An RSA key pair.
    Rsa(&'a crate::rsa::RsaKeyPair),
    /// An ECDSA key pair.
    #[cfg(any(openssl, symcrypt, all(native, windows)))]
    Ecdsa(&'a crate::ecdsa::EcdsaKeyPair),
}

impl<'a> From<&'a crate::rsa::RsaKeyPair> for X509KeyPair<'a> {
    fn from(key: &'a crate::rsa::RsaKeyPair) -> Self {
        Self::Rsa(key)
    }
}

#[cfg(any(openssl, symcrypt, all(native, windows)))]
impl<'a> From<&'a crate::ecdsa::EcdsaKeyPair> for X509KeyPair<'a> {
    fn from(key: &'a crate::ecdsa::EcdsaKeyPair) -> Self {
        Self::Ecdsa(key)
    }
}

/// An X.509 certificate.
pub struct X509Certificate(pub(crate) sys::X509CertificateInner);

/// Parameters for constructing a self-signed test certificate.
///
/// Empty name components are omitted. The same name is used for issuer and
/// subject. Times are ASN.1 UTCTime or GeneralizedTime content strings.
/// Defaults provide a generic test identity valid from 1970 through 9999.
#[cfg(any(test, feature = "test_helpers"))]
pub struct X509SelfSignedOptions<'a> {
    /// Country name.
    pub country: &'a str,
    /// State or province.
    pub state: &'a str,
    /// Locality.
    pub locality: &'a str,
    /// Organization.
    pub organization: &'a str,
    /// Common name.
    pub common_name: &'a str,
    /// Inclusive validity start.
    pub not_before: &'a str,
    /// Inclusive validity end.
    pub not_after: &'a str,
    /// Optional authority key identifier, independent of the signing key.
    pub authority_key_identifier: Option<&'a [u8]>,
}

#[cfg(any(test, feature = "test_helpers"))]
impl Default for X509SelfSignedOptions<'_> {
    fn default() -> Self {
        Self {
            country: "US",
            state: "Washington",
            locality: "Redmond",
            organization: "OpenVMM Test",
            common_name: "test.example.com",
            not_before: "700101000000Z",
            not_after: "99991230000000Z",
            authority_key_identifier: None,
        }
    }
}

#[cfg(any(test, feature = "test_helpers"))]
#[cfg(any(rust, symcrypt, all(native, target_os = "macos")))]
impl X509SelfSignedOptions<'_> {
    pub(super) fn validity(&self) -> anyhow::Result<x509_cert::time::Validity> {
        fn time(value: &str) -> der::Result<x509_cert::time::Time> {
            let tag = if value.len() == 13 {
                der::Tag::UtcTime
            } else {
                der::Tag::GeneralizedTime
            };
            der::asn1::AnyRef::new(tag, value.as_bytes())?.decode_as()
        }
        let validity =
            x509_cert::time::Validity::new(time(self.not_before)?, time(self.not_after)?);
        anyhow::ensure!(
            validity.not_before.to_unix_duration() <= validity.not_after.to_unix_duration(),
            "certificate validity ends before it starts"
        );
        Ok(validity)
    }
}

impl X509Certificate {
    /// Parse an X.509 certificate from DER-encoded bytes.
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        sys::X509CertificateInner::from_der(data).map(Self)
    }

    /// Parse the first DER-encoded certificate, ignoring extraneous trailing
    /// bytes.
    pub fn from_der_prefix(data: &[u8]) -> Result<Self, X509Error> {
        sys::X509CertificateInner::from_der_prefix(data).map(Self)
    }

    /// Extract the public key from this certificate.
    pub fn public_key(&self) -> Result<X509PublicKey, X509Error> {
        self.0.public_key()
    }

    /// Verify the signature of this certificate against the given issuer's
    /// public key. Returns `Ok(true)` if the signature is valid, `Ok(false)`
    /// if the signature is invalid, or an error for other failures.
    pub fn verify(&self, issuer_public_key: &X509PublicKey) -> Result<bool, X509VerifyError> {
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
    /// Build a self-signed test certificate with explicit name, validity and AKI.
    ///
    /// Signs with SHA-256 for RSA and SHA-384 for ECDSA. The supplied AKI is a
    /// test value: it need not identify the signing key.
    pub fn build_self_signed(
        key: X509KeyPair<'_>,
        options: &X509SelfSignedOptions<'_>,
    ) -> anyhow::Result<Self> {
        sys::X509CertificateInner::build_self_signed(key, options).map(Self)
    }

    /// Get the Common Name (CN) from the X.509 certificate's subject name. If
    /// there are multiple CNs present, return the first.
    pub fn subject_common_name(&self) -> Result<Option<String>, X509Error> {
        self.0.subject_common_name()
    }

    /// Get the first common name in the certificate's issuer name.
    pub fn issuer_common_name(&self) -> Result<Option<String>, X509Error> {
        self.0.issuer_common_name()
    }

    /// Get the inclusive validity bounds as UTC timestamps, without checking
    /// the current time. The original ASN.1 time encoding is not preserved.
    pub fn validity(&self) -> Result<(jiff::Timestamp, jiff::Timestamp), X509ValidityError> {
        self.0.validity()
    }

    /// Get the authority key identifier, or `None` if the extension or its
    /// key ID is unavailable.
    pub fn authority_key_identifier(&self) -> Result<Option<Vec<u8>>, X509Error> {
        self.0.authority_key_identifier()
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

    #[test]
    fn certificate_prefix_ignores_storage_padding() {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = build_test_cert(&key);
        let encoded = cert.to_der().unwrap();
        for padding in [
            vec![],
            vec![0; 4096],
            vec![0xa5; 4096],
            vec![0x30, 0x80, 0xff],
        ] {
            let mut storage = encoded.clone();
            storage.extend_from_slice(&padding);
            let parsed = X509Certificate::from_der_prefix(&storage).unwrap();
            assert_eq!(parsed.to_der().unwrap(), encoded);
            assert!(parsed.verify(&parsed.public_key().unwrap()).unwrap());
        }
        assert!(X509Certificate::from_der_prefix(&[]).is_err());
        assert!(X509Certificate::from_der_prefix(&[0; 4096]).is_err());
        assert!(X509Certificate::from_der_prefix(&encoded[..encoded.len() - 1]).is_err());
    }

    #[test]
    fn configurable_self_signed_metadata() {
        use der::Decode;
        use x509_cert::ext::pkix::AuthorityKeyIdentifier;

        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let name = "Test \"AK\", + issuer = é";
        let options = X509SelfSignedOptions {
            common_name: name,
            not_before: "261009180232Z",
            not_after: "20561120180232Z",
            authority_key_identifier: Some(&[0x01, 0xab, 0x00, 0xff]),
            ..Default::default()
        };
        let cert = X509Certificate::build_self_signed((&key).into(), &options).unwrap();
        assert_eq!(cert.subject_common_name().unwrap().as_deref(), Some(name));
        assert_eq!(cert.issuer_common_name().unwrap().as_deref(), Some(name));
        assert_eq!(
            cert.validity().unwrap(),
            (
                "2026-10-09T18:02:32Z".parse::<jiff::Timestamp>().unwrap(),
                "2056-11-20T18:02:32Z".parse::<jiff::Timestamp>().unwrap()
            )
        );
        assert_eq!(
            cert.authority_key_identifier().unwrap(),
            Some(vec![1, 0xab, 0, 0xff])
        );
        let parsed = x509_cert::Certificate::from_der(&cert.to_der().unwrap()).unwrap();
        let tbs = parsed.tbs_certificate();
        assert_eq!(tbs.issuer(), tbs.subject());
        assert_eq!(
            tbs.validity().not_before.to_date_time(),
            der::DateTime::new(2026, 10, 9, 18, 2, 32).unwrap()
        );
        assert_eq!(
            tbs.validity().not_after.to_date_time(),
            der::DateTime::new(2056, 11, 20, 18, 2, 32).unwrap()
        );
        assert_eq!(
            tbs.get_extension::<AuthorityKeyIdentifier>()
                .unwrap()
                .unwrap()
                .1
                .key_identifier
                .unwrap()
                .as_bytes(),
            &[1, 0xab, 0, 0xff]
        );
        assert!(cert.verify(&cert.public_key().unwrap()).unwrap());
    }

    #[test]
    fn invalid_self_signed_validity_is_rejected() {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        for (not_before, not_after) in [
            ("not a date", "20561120180232Z"),
            ("261009180232Z", "20561320180232Z"),
            ("261009180232Z", "251009180232Z"),
        ] {
            assert!(
                X509Certificate::build_self_signed(
                    (&key).into(),
                    &X509SelfSignedOptions {
                        not_before,
                        not_after,
                        ..Default::default()
                    },
                )
                .is_err()
            );
        }
    }

    #[test]
    fn validity_timestamp_bounds() {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        for (not_after, expected) in [
            ("000229000000Z", "2000-02-29T00:00:00Z"),
            ("491231235959Z", "2049-12-31T23:59:59Z"),
            ("20500101000000Z", "2050-01-01T00:00:00Z"),
        ] {
            let cert = X509Certificate::build_self_signed(
                (&key).into(),
                &X509SelfSignedOptions {
                    not_after,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                cert.validity().unwrap(),
                (jiff::Timestamp::UNIX_EPOCH, expected.parse().unwrap())
            );
        }
        let cert = X509Certificate::build_self_signed(
            (&key).into(),
            &X509SelfSignedOptions {
                not_after: "99991231235959Z",
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            cert.validity(),
            Err(X509ValidityError::Timestamp(_))
        ));
    }

    #[cfg(any(openssl, all(native, windows)))]
    #[test]
    fn validity_before_unix_epoch() {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = X509Certificate::build_self_signed(
            (&key).into(),
            &X509SelfSignedOptions {
                not_before: "691231235959Z",
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            cert.validity().unwrap().0,
            jiff::Timestamp::from_second(-1).unwrap()
        );
    }

    fn build_test_cert<'a>(key: impl Into<X509KeyPair<'a>>) -> X509Certificate {
        X509Certificate::build_self_signed(key.into(), &Default::default()).unwrap()
    }

    #[test]
    fn rsa_self_signed_cert_verifies() {
        let key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = build_test_cert(&key);
        assert_eq!(
            cert.validity().unwrap(),
            (
                jiff::Timestamp::UNIX_EPOCH,
                "9999-12-30T00:00:00Z".parse().unwrap()
            )
        );
        assert_eq!(cert.authority_key_identifier().unwrap(), None);
        let public_key = cert.public_key().unwrap();
        assert!(matches!(&public_key, X509PublicKey::Rsa(_)));
        assert!(cert.verify(&public_key).unwrap());
        assert!(cert.issued(&cert).unwrap());
        let reparsed = X509Certificate::from_der(&cert.to_der().unwrap()).unwrap();
        assert!(reparsed.verify(&public_key).unwrap());
    }

    #[cfg(any(openssl, symcrypt, all(native, windows)))]
    #[test]
    fn ecdsa_self_signed_cert_verifies() {
        let key = crate::ecdsa::EcdsaKeyPair::generate(crate::ecdsa::EcdsaCurve::P384).unwrap();
        let cert = build_test_cert(&key);
        let public_key = cert.public_key().unwrap();
        assert!(matches!(&public_key, X509PublicKey::Ecdsa(_)));
        assert!(cert.verify(&public_key).unwrap());
        assert!(cert.issued(&cert).unwrap());
        let reparsed = X509Certificate::from_der(&cert.to_der().unwrap()).unwrap();
        assert!(reparsed.verify(&public_key).unwrap());
        let public_key = reparsed.public_key().unwrap().ecdsa().unwrap();
        assert_eq!(
            public_key.to_public_key_der().unwrap(),
            key.to_public_key_der().unwrap()
        );
    }

    #[cfg(any(symcrypt, all(native, windows)))]
    #[test]
    fn ecdsa_cert_propagates_signature_decoding_errors() {
        use der::Decode;
        use der::Encode;

        #[derive(der::Sequence)]
        struct EncodedCertificate<'a> {
            tbs_certificate: der::AnyRef<'a>,
            signature_algorithm: der::AnyRef<'a>,
            signature: der::asn1::BitStringRef<'a>,
        }

        let key = crate::ecdsa::EcdsaKeyPair::generate(crate::ecdsa::EcdsaCurve::P384).unwrap();
        let cert = build_test_cert(&key);
        let public_key = cert.public_key().unwrap();
        let encoded = cert.to_der().unwrap();
        let mut cert = EncodedCertificate::from_der(&encoded).unwrap();

        let mut oversized = vec![0x30, 54, 2, 49];
        oversized.extend_from_slice(&[1; 49]);
        oversized.extend_from_slice(&[2, 1, 1]);
        for signature in [&[0x30, 0][..], oversized.as_slice()] {
            cert.signature = der::asn1::BitStringRef::from_bytes(signature).unwrap();
            let cert = X509Certificate::from_der(&cert.to_der().unwrap()).unwrap();
            assert!(matches!(
                cert.verify(&public_key),
                Err(X509VerifyError::Ecdsa(_))
            ));
        }
    }

    #[cfg(any(openssl, symcrypt, all(native, windows)))]
    #[test]
    fn cert_rejects_wrong_key_type() {
        let rsa_key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let ecdsa_key =
            crate::ecdsa::EcdsaKeyPair::generate(crate::ecdsa::EcdsaCurve::P384).unwrap();
        let rsa_cert = build_test_cert(&rsa_key);
        let ecdsa_cert = build_test_cert(&ecdsa_key);
        let rsa_public_key = rsa_cert.public_key().unwrap();
        let ecdsa_public_key = ecdsa_cert.public_key().unwrap();

        assert!(rsa_cert.verify(&rsa_public_key).unwrap());
        assert!(ecdsa_cert.verify(&ecdsa_public_key).unwrap());

        // Backends may report an algorithm mismatch as an error or an invalid signature.
        assert!(matches!(
            rsa_cert.verify(&ecdsa_public_key),
            Ok(false) | Err(_)
        ));
        assert!(matches!(
            ecdsa_cert.verify(&rsa_public_key),
            Ok(false) | Err(_)
        ));
    }

    /// A cert signed by one key must NOT verify against an unrelated key.
    #[test]
    fn cert_rejects_wrong_issuer_key() {
        let signing_key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let other_key = crate::rsa::RsaKeyPair::generate(2048).unwrap();
        let cert = build_test_cert(&signing_key);
        let other_pubkey = build_test_cert(&other_key).public_key().unwrap();
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
