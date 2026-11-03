// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared helpers for constructing self-signed test certificates with the
//! `x509-cert` builder.

#![cfg(any(test, feature = "test_helpers"))]

use der::Decode;
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
/// given name, validity and optional AKI, with serial number 1.
/// The caller invokes `.build()` with a backend-specific signer to produce
/// the final `Certificate`.
///
/// Returns the builder and the SubjectPublicKeyInfo DER encoding for the
/// signer adapter.
pub(super) fn self_signed_builder(
    key: super::X509KeyPair<'_>,
    options: &super::X509SelfSignedOptions<'_>,
) -> anyhow::Result<(CertificateBuilder<SelfSignedProfile>, Vec<u8>)> {
    use const_oid::db::rfc2256::STATE_OR_PROVINCE_NAME;
    use const_oid::db::rfc4519::{COMMON_NAME, COUNTRY_NAME, LOCALITY_NAME, ORGANIZATION_NAME};
    let mut rdns = x509_cert::name::RdnSequence::default();
    for (oid, value, tag) in [
        (COUNTRY_NAME, options.country, der::Tag::PrintableString),
        (STATE_OR_PROVINCE_NAME, options.state, der::Tag::Utf8String),
        (LOCALITY_NAME, options.locality, der::Tag::Utf8String),
        (
            ORGANIZATION_NAME,
            options.organization,
            der::Tag::Utf8String,
        ),
        (COMMON_NAME, options.common_name, der::Tag::Utf8String),
    ] {
        if !value.is_empty() {
            rdns.push(
                vec![x509_cert::attr::AttributeTypeAndValue {
                    oid,
                    value: der::Any::new(tag, value.as_bytes())?,
                }]
                .try_into()?,
            );
        }
    }
    let name = Name::from_der(&rdns.to_der()?)?;

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
        super::X509KeyPair::Ecdsa(key) => key.0.as_pub().to_public_key_info()?,
    };
    let public_key_der = spki.to_der()?;

    let serial_number = x509_cert::serial_number::SerialNumber::from(1u32);
    let validity = options.validity()?;

    let profile = SelfSignedProfile { subject: name };
    let mut builder = CertificateBuilder::new(profile, serial_number, validity, spki)?;
    if let Some(id) = options.authority_key_identifier {
        builder.add_extension(&x509_cert::ext::pkix::AuthorityKeyIdentifier {
            key_identifier: Some(der::asn1::OctetString::new(id)?),
            ..Default::default()
        })?;
    }
    Ok((builder, public_key_der))
}

/// Adapter that implements the `signature` and `spki` traits required by
/// [`x509_cert::builder::CertificateBuilder::build`] on top of a
/// [`super::X509KeyPair`], using SHA-256 for RSA and SHA-384 for ECDSA.
#[cfg(any(all(native, target_os = "macos"), symcrypt))]
pub(super) mod keypair_signer {
    use der::Decode;

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
                    oid: const_oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION,
                    parameters: Some(der::Any::null()),
                },
                #[cfg(symcrypt)]
                super::super::X509KeyPair::Ecdsa(_) => x509_cert::spki::AlgorithmIdentifierOwned {
                    oid: const_oid::db::rfc5912::ECDSA_WITH_SHA_384,
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
                    let signature = key
                        .sign(msg, crate::HashAlgorithm::Sha384)
                        .map_err(signature::Error::from_source)?;
                    signature.to_der().map_err(signature::Error::from_source)?
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
