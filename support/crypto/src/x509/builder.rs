// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared helper for constructing self-signed test certificates.

#![cfg(any(test, feature = "test_helpers"))]

use core::str::FromStr;
use der::Decode;
use der::Encode;
use der::Sequence;
use x509_cert::name::Name;

#[derive(Sequence)]
struct TbsCertificate {
    #[asn1(context_specific = "0", default = "Default::default")]
    version: x509_cert::Version,
    serial_number: x509_cert::serial_number::SerialNumber,
    signature: x509_cert::spki::AlgorithmIdentifierOwned,
    issuer: Name,
    validity: x509_cert::time::Validity,
    subject: Name,
    subject_public_key_info: x509_cert::spki::SubjectPublicKeyInfoOwned,
    #[asn1(context_specific = "1", tag_mode = "IMPLICIT", optional = "true")]
    issuer_unique_id: Option<der::asn1::BitString>,
    #[asn1(context_specific = "2", tag_mode = "IMPLICIT", optional = "true")]
    subject_unique_id: Option<der::asn1::BitString>,
    #[asn1(context_specific = "3", tag_mode = "EXPLICIT", optional = "true")]
    extensions: Option<Vec<x509_cert::ext::Extension>>,
}

#[derive(Sequence)]
struct Certificate {
    tbs_certificate: TbsCertificate,
    signature_algorithm: x509_cert::spki::AlgorithmIdentifierOwned,
    signature: der::asn1::BitString,
}

/// Build a self-signed RSA certificate with serial number 1, no extensions,
/// and a validity range covering all representable time.
pub(super) fn build_self_signed(
    key: &crate::rsa::RsaKeyPair,
    country: &str,
    state: &str,
    locality: &str,
    organization: &str,
    common_name: &str,
) -> anyhow::Result<x509_cert::Certificate> {
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

    let signature_algorithm = x509_cert::spki::AlgorithmIdentifierOwned {
        oid: der::oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION,
        parameters: Some(der::Any::null()),
    };
    let tbs_certificate = TbsCertificate {
        version: x509_cert::Version::V3,
        serial_number: x509_cert::serial_number::SerialNumber::from(1u32),
        signature: signature_algorithm.clone(),
        issuer: name.clone(),
        validity: x509_cert::time::Validity::new(
            der::asn1::UtcTime::from_unix_duration(std::time::Duration::ZERO)?.into(),
            x509_cert::time::Time::INFINITY,
        ),
        subject: name,
        subject_public_key_info: spki,
        issuer_unique_id: None,
        subject_unique_id: None,
        extensions: None,
    };
    let signature = key.pkcs1_sign(&tbs_certificate.to_der()?, crate::HashAlgorithm::Sha256)?;
    let certificate = Certificate {
        tbs_certificate,
        signature_algorithm,
        signature: der::asn1::BitString::from_bytes(&signature)?,
    };
    Ok(x509_cert::Certificate::from_der(&certificate.to_der()?)?)
}
