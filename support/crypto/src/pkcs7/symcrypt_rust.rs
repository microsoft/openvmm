// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure-Rust PKCS#7 (CMS SignedData) verification backend.
//!
//! Shared by the `rust` and `symcrypt` features. Uses the RustCrypto
//! `cms`/`x509-cert`/`der` stack for parsing and structural traversal,
//! delegates RSA signature verification to this crate's `rsa` module
//! (which uses `symcrypt` or `rsa` depending on the active feature).

use super::*;
use cms::cert::CertificateChoices;
use cms::cert::IssuerAndSerialNumber;
use cms::content_info::CmsVersion;
use cms::content_info::ContentInfo;
use cms::signed_data::CertificateSet;
use cms::signed_data::EncapsulatedContentInfo;
use cms::signed_data::SignedData;
use cms::signed_data::SignerIdentifier;
use cms::signed_data::SignerInfo;
use cms::signed_data::SignerInfos;
use der::AnyRef;
use der::Decode;
use der::Encode;
use der::asn1::OctetString;
use der::asn1::SetOfVec;
use der::oid::db::rfc5911::ID_DATA;
use der::oid::db::rfc5911::ID_SIGNED_DATA;
use der::oid::db::rfc5912::ID_SHA_256;
use der::oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION;
use x509_cert::Certificate;
use x509_cert::spki::AlgorithmIdentifierOwned;

#[cfg(symcrypt)]
fn err(err: der::Error, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(crate::BackendError::Der(err, op))
}

#[cfg(rust)]
fn err(err: der::Error, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(err, op)
}

pub struct Pkcs7SignedDataInner(SignedData);

pub struct Pkcs7CertStoreInner(Vec<Certificate>);

impl Pkcs7CertStoreInner {
    pub fn new() -> Result<Self, Pkcs7Error> {
        Ok(Self(Vec::new()))
    }

    pub fn add_cert(&mut self, cert: &crate::x509::X509Certificate) -> Result<(), Pkcs7Error> {
        self.0.push(cert.0.0.clone());
        Ok(())
    }
}

impl Pkcs7SignedDataInner {
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        let ci = ContentInfo::from_der(data).map_err(|e| err(e, "parsing PKCS#7 ContentInfo"))?;
        let signed_data = ci
            .content
            .decode_as::<SignedData>()
            .map_err(|e| err(e, "decoding PKCS#7 SignedData"))?;
        Ok(Self(signed_data))
    }

    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        let sd_der = self
            .0
            .to_der()
            .map_err(|e| err(e, "encoding PKCS#7 SignedData"))?;
        let content = AnyRef::try_from(sd_der.as_slice())
            .map_err(|e| err(e, "wrapping SignedData in Any"))?;
        let ci = ContentInfo {
            content_type: ID_SIGNED_DATA,
            content: content.into(),
        };
        ci.to_der()
            .map_err(|e| err(e, "encoding PKCS#7 ContentInfo"))
    }

    pub fn sign(
        cert: &crate::x509::X509Certificate,
        key_pair: &crate::rsa::RsaKeyPair,
        data: &[u8],
    ) -> Result<Self, crate::rsa::RsaError> {
        // Produce a detached PKCS#1 v1.5 SHA-256 signature over `data`. We
        // omit signed attributes, so the signature covers the content
        // directly per RFC 5652 §5.4.
        let signature = key_pair.pkcs1_sign(data, crate::rsa::HashAlgorithm::Sha256)?;

        let digest_alg = AlgorithmIdentifierOwned {
            oid: ID_SHA_256,
            parameters: None,
        };
        let signature_algorithm = AlgorithmIdentifierOwned {
            oid: SHA_256_WITH_RSA_ENCRYPTION,
            parameters: None,
        };

        let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
            issuer: cert.0.0.tbs_certificate().issuer().clone(),
            serial_number: cert.0.0.tbs_certificate().serial_number().clone(),
        });

        let signer = SignerInfo {
            version: CmsVersion::V1,
            sid,
            digest_alg: digest_alg.clone(),
            signed_attrs: None,
            signature_algorithm,
            signature: OctetString::new(signature).unwrap(),
            unsigned_attrs: None,
        };

        let mut digest_algorithms = SetOfVec::new();
        digest_algorithms.insert(digest_alg).unwrap();

        let mut certs = SetOfVec::new();
        certs
            .insert(CertificateChoices::Certificate(cert.0.0.clone()))
            .unwrap();

        let mut signer_infos = SetOfVec::new();
        signer_infos.insert(signer).unwrap();

        let signed_data = SignedData {
            version: CmsVersion::V1,
            digest_algorithms,
            encap_content_info: EncapsulatedContentInfo {
                econtent_type: ID_DATA,
                econtent: None,
            },
            certificates: Some(CertificateSet(certs)),
            crls: None,
            signer_infos: SignerInfos(signer_infos),
        };

        Ok(Self(signed_data))
    }
}
