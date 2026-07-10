// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared helpers for constructing self-signed test certificates with the
//! `x509-cert` builder.

#![cfg(any(test, feature = "test_helpers"))]

use core::str::FromStr;
use der::Encode;
use x509_cert::builder::CertificateBuilder;
use x509_cert::name::Name;

/// `BuilderProfile` that produces a self-signed certificate with no
/// extensions and the same distinguished name for the issuer and subject.
pub(super) struct SelfSignedProfile {
    subject: Name,
}

impl x509_cert::builder::profile::BuilderProfile for SelfSignedProfile {
    fn get_issuer(&self, _subject: &Name) -> Name {
        self.subject.clone()
    }

    fn get_subject(&self) -> Name {
        self.subject.clone()
    }

    fn build_extensions(
        &self,
        _spk: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
        _issuer_spk: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
        _tbs: &x509_cert::TbsCertificate,
    ) -> x509_cert::builder::Result<Vec<x509_cert::ext::Extension>> {
        Ok(Vec::new())
    }
}

/// Set up a `CertificateBuilder` for a self-signed RSA certificate with the
/// given distinguished name components, serial number 1, and a validity
/// range covering all of representable time. The caller invokes `.build()`
/// with a backend-specific signer to produce the final `Certificate`.
///
/// Returns the builder along with the PKCS#1 `RSAPublicKey` DER encoding
/// of the supplied key, which several backends need to hand to their
/// signer adapter.
pub(super) fn self_signed_builder(
    key: &crate::rsa::RsaKeyPair,
    country: &str,
    state: &str,
    locality: &str,
    organization: &str,
    common_name: &str,
) -> anyhow::Result<(CertificateBuilder<SelfSignedProfile>, Vec<u8>)> {
    let name = Name::from_str(&format!(
        "CN={common_name},O={organization},L={locality},ST={state},C={country}"
    ))?;

    let components = key.to_components();
    let pkcs1_pub = pkcs1::RsaPublicKey {
        modulus: der::asn1::UintRef::new(&components.modulus)?,
        public_exponent: der::asn1::UintRef::new(&components.public_exponent)?,
    };
    let pkcs1_der = pkcs1_pub.to_der()?;
    let spki = x509_cert::spki::SubjectPublicKeyInfoOwned {
        algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
            oid: pkcs1::ALGORITHM_OID,
            parameters: Some(der::Any::null()),
        },
        subject_public_key: der::asn1::BitString::from_bytes(&pkcs1_der)?,
    };

    let serial_number = x509_cert::serial_number::SerialNumber::from(1u32);
    let validity = x509_cert::time::Validity::new(
        der::asn1::GeneralizedTime::from_unix_duration(std::time::Duration::from_secs(0))?.into(),
        x509_cert::time::Time::INFINITY,
    );

    let profile = SelfSignedProfile { subject: name };
    let builder = CertificateBuilder::new(profile, serial_number, validity, spki)?;
    Ok((builder, pkcs1_der))
}

/// Adapter that implements the `signature` and `spki` traits required by
/// [`x509_cert::builder::CertificateBuilder::build`] on top of a
/// [`crate::rsa::RsaKeyPair`] using its `pkcs1_sign` primitive with SHA-256.
#[cfg(any(all(native, target_os = "macos"), symcrypt))]
pub(super) mod rsa_keypair_signer {
    use der::Encode;

    pub struct RsaKeyPairSigner<'a> {
        pub key: &'a crate::rsa::RsaKeyPair,
        pub pkcs1_der: Vec<u8>,
    }

    #[derive(Clone)]
    pub struct RsaKeyPairVerifyingKey(Vec<u8>);

    pub struct RsaKeyPairSignature(Vec<u8>);

    impl signature::Keypair for RsaKeyPairSigner<'_> {
        type VerifyingKey = RsaKeyPairVerifyingKey;

        fn verifying_key(&self) -> Self::VerifyingKey {
            RsaKeyPairVerifyingKey(self.pkcs1_der.clone())
        }
    }

    impl x509_cert::spki::DynSignatureAlgorithmIdentifier for RsaKeyPairSigner<'_> {
        fn signature_algorithm_identifier(
            &self,
        ) -> x509_cert::spki::Result<x509_cert::spki::AlgorithmIdentifierOwned> {
            Ok(x509_cert::spki::AlgorithmIdentifierOwned {
                oid: der::oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION,
                parameters: Some(der::Any::null()),
            })
        }
    }

    impl signature::Signer<RsaKeyPairSignature> for RsaKeyPairSigner<'_> {
        fn try_sign(&self, msg: &[u8]) -> Result<RsaKeyPairSignature, signature::Error> {
            self.key
                .pkcs1_sign(msg, crate::HashAlgorithm::Sha256)
                .map(RsaKeyPairSignature)
                .map_err(signature::Error::from_source)
        }
    }

    impl x509_cert::spki::EncodePublicKey for RsaKeyPairVerifyingKey {
        fn to_public_key_der(&self) -> x509_cert::spki::Result<der::Document> {
            // Wrap the PKCS#1 RSAPublicKey DER in a SubjectPublicKeyInfo and
            // encode the result as a DER document.
            let spki = x509_cert::spki::SubjectPublicKeyInfoOwned {
                algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                    oid: pkcs1::ALGORITHM_OID,
                    parameters: Some(der::Any::null()),
                },
                subject_public_key: der::asn1::BitString::from_bytes(&self.0)?,
            };
            Ok(der::Document::try_from(spki.to_der()?)?)
        }
    }

    impl x509_cert::spki::SignatureBitStringEncoding for RsaKeyPairSignature {
        fn to_bitstring(&self) -> der::Result<der::asn1::BitString> {
            der::asn1::BitString::from_bytes(&self.0)
        }
    }
}
