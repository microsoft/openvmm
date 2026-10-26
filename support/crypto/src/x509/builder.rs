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

/// Set up a `CertificateBuilder` for a self-signed certificate with the
/// given distinguished name components, serial number 1, and a validity
/// range covering all of representable time. The caller invokes `.build()`
/// with a backend-specific signer to produce the final `Certificate`.
///
/// Returns the builder and the SubjectPublicKeyInfo DER encoding for the
/// signer adapter.
pub(super) fn self_signed_builder(
    key: super::X509KeyPair<'_>,
    country: &str,
    state: &str,
    locality: &str,
    organization: &str,
    common_name: &str,
) -> anyhow::Result<(CertificateBuilder<SelfSignedProfile>, Vec<u8>)> {
    let name = Name::from_str(&format!(
        "CN={common_name},O={organization},L={locality},ST={state},C={country}"
    ))?;

    let spki = match key {
        super::X509KeyPair::Rsa(key) => {
            let components = key.to_components()?;
            let pkcs1_pub = pkcs1::RsaPublicKey {
                modulus: der::asn1::UintRef::new(&components.modulus)?,
                public_exponent: der::asn1::UintRef::new(&components.public_exponent)?,
            };
            x509_cert::spki::SubjectPublicKeyInfoOwned {
                algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                    oid: pkcs1::ALGORITHM_OID,
                    parameters: Some(der::Any::null()),
                },
                subject_public_key: der::asn1::BitString::from_bytes(&pkcs1_pub.to_der()?)?,
            }
        }
        #[cfg(symcrypt)]
        super::X509KeyPair::Ecdsa(key) => {
            let mut point = vec![crate::ecdsa::SEC1_UNCOMPRESSED_POINT_TAG];
            point.extend_from_slice(&key.public_key_bytes()?);
            x509_cert::spki::SubjectPublicKeyInfoOwned {
                algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                    oid: der::oid::db::rfc5912::ID_EC_PUBLIC_KEY,
                    parameters: Some(der::oid::db::rfc5912::SECP_384_R_1.into()),
                },
                subject_public_key: der::asn1::BitString::from_bytes(&point)?,
            }
        }
    };
    let public_key_der = spki.to_der()?;

    let serial_number = x509_cert::serial_number::SerialNumber::from(1u32);
    let validity = x509_cert::time::Validity::new(
        der::asn1::GeneralizedTime::from_unix_duration(std::time::Duration::from_secs(0))?.into(),
        x509_cert::time::Time::INFINITY,
    );

    let profile = SelfSignedProfile { subject: name };
    let builder = CertificateBuilder::new(profile, serial_number, validity, spki)?;
    Ok((builder, public_key_der))
}

/// Adapter that implements the `signature` and `spki` traits required by
/// [`x509_cert::builder::CertificateBuilder::build`] on top of a
/// [`super::X509KeyPair`], using SHA-256 for RSA and SHA-384 for ECDSA.
#[cfg(any(all(native, target_os = "macos"), symcrypt))]
pub(super) mod keypair_signer {
    use der::Decode;
    #[cfg(symcrypt)]
    use der::Encode;

    pub struct KeyPairSigner<'a> {
        pub key: super::super::X509KeyPair<'a>,
        pub public_key_der: Vec<u8>,
    }

    #[derive(Clone)]
    pub struct KeyPairVerifyingKey(Vec<u8>);

    pub struct KeyPairSignature(Vec<u8>);

    impl signature::Keypair for KeyPairSigner<'_> {
        type VerifyingKey = KeyPairVerifyingKey;

        fn verifying_key(&self) -> Self::VerifyingKey {
            KeyPairVerifyingKey(self.public_key_der.clone())
        }
    }

    impl x509_cert::spki::DynSignatureAlgorithmIdentifier for KeyPairSigner<'_> {
        fn signature_algorithm_identifier(
            &self,
        ) -> x509_cert::spki::Result<x509_cert::spki::AlgorithmIdentifierOwned> {
            Ok(match self.key {
                super::super::X509KeyPair::Rsa(_) => x509_cert::spki::AlgorithmIdentifierOwned {
                    oid: der::oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION,
                    parameters: Some(der::Any::null()),
                },
                #[cfg(symcrypt)]
                super::super::X509KeyPair::Ecdsa(_) => x509_cert::spki::AlgorithmIdentifierOwned {
                    oid: der::oid::db::rfc5912::ECDSA_WITH_SHA_384,
                    parameters: None,
                },
            })
        }
    }

    impl signature::Signer<KeyPairSignature> for KeyPairSigner<'_> {
        fn try_sign(&self, msg: &[u8]) -> Result<KeyPairSignature, signature::Error> {
            let bytes = match self.key {
                super::super::X509KeyPair::Rsa(key) => key
                    .pkcs1_sign(msg, crate::HashAlgorithm::Sha256)
                    .map_err(signature::Error::from_source)?,
                #[cfg(symcrypt)]
                super::super::X509KeyPair::Ecdsa(key) => {
                    use der::asn1::UintRef;

                    let signature = key
                        .sign(msg, crate::HashAlgorithm::Sha384)
                        .map_err(signature::Error::from_source)?;
                    let (r, s) = signature.split_at(crate::ecdsa::EcdsaCurve::P384.key_size());
                    [
                        UintRef::new(r).map_err(signature::Error::from_source)?,
                        UintRef::new(s).map_err(signature::Error::from_source)?,
                    ]
                    .to_der()
                    .map_err(signature::Error::from_source)?
                }
            };
            Ok(KeyPairSignature(bytes))
        }
    }

    impl x509_cert::spki::EncodePublicKey for KeyPairVerifyingKey {
        fn to_public_key_der(&self) -> x509_cert::spki::Result<der::Document> {
            Ok(der::Document::from_der(&self.0)?)
        }
    }

    impl x509_cert::spki::SignatureBitStringEncoding for KeyPairSignature {
        fn to_bitstring(&self) -> der::Result<der::asn1::BitString> {
            der::asn1::BitString::from_bytes(&self.0)
        }
    }
}
