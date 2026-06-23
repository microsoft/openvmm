// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cross-backend UEFI Secure Boot-focused PKCS#7 (CMS SignedData) parsing
//! for the pure-Rust (`rust`) and `symcrypt` backends.

use super::*;
use cms::cert::CertificateChoices;
use cms::content_info::ContentInfo;
use cms::signed_data::SignedData;
use cms::signed_data::SignerIdentifier;
use der::Decode;
use der::oid::db::rfc5911::ID_SIGNED_DATA;

#[cfg(rust)]
fn err(err: der::Error, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(err, op)
}

#[cfg(symcrypt)]
fn err(err: der::Error, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(crate::BackendError::Der(err, op))
}

pub struct Pkcs7SignedDataInner(SignedData);

impl Pkcs7SignedDataInner {
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        let ci = ContentInfo::from_der(data).map_err(|e| err(e, "parsing PKCS#7 ContentInfo"))?;
        if ci.content_type != ID_SIGNED_DATA {
            return Err(err(
                der::ErrorKind::OidUnknown {
                    oid: ci.content_type,
                }
                .to_error(),
                "unrecognized content type OID",
            ));
        }
        let signed_data = ci
            .content
            .decode_as::<SignedData>()
            .map_err(|e| err(e, "decoding PKCS#7 SignedData"))?;
        Ok(Self(signed_data))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        use der::Encode;
        let sd_der = self
            .0
            .to_der()
            .map_err(|e| err(e, "encoding PKCS#7 SignedData"))?;
        let content = der::AnyRef::try_from(sd_der.as_slice())
            .map_err(|e| err(e, "wrapping SignedData in Any"))?;
        let ci = ContentInfo {
            content_type: ID_SIGNED_DATA,
            content: content.into(),
        };
        ci.to_der()
            .map_err(|e| err(e, "encoding PKCS#7 ContentInfo"))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn sign(
        cert: &X509Certificate,
        key_pair: &crate::rsa::RsaKeyPair,
        data: &[u8],
    ) -> Result<Self, crate::rsa::RsaError> {
        use der::asn1::SetOfVec;
        use x509_cert::spki::AlgorithmIdentifierOwned;

        // Detached PKCS#1 v1.5 SHA-256 signature; no signed attributes so
        // the signature covers `data` directly per RFC 5652 §5.4.
        let signature = key_pair.pkcs1_sign(data, crate::HashAlgorithm::Sha256)?;

        let digest_alg = AlgorithmIdentifierOwned {
            oid: der::oid::db::rfc5912::ID_SHA_256,
            parameters: None,
        };
        let signature_algorithm = AlgorithmIdentifierOwned {
            oid: der::oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION,
            parameters: None,
        };

        let sid = SignerIdentifier::IssuerAndSerialNumber(cms::cert::IssuerAndSerialNumber {
            issuer: cert.0.0.tbs_certificate().issuer().clone(),
            serial_number: cert.0.0.tbs_certificate().serial_number().clone(),
        });

        let signer = cms::signed_data::SignerInfo {
            version: cms::content_info::CmsVersion::V1,
            sid,
            digest_alg: digest_alg.clone(),
            signed_attrs: None,
            signature_algorithm,
            signature: der::asn1::OctetString::new(signature).unwrap(),
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
            version: cms::content_info::CmsVersion::V1,
            digest_algorithms,
            encap_content_info: cms::signed_data::EncapsulatedContentInfo {
                econtent_type: der::oid::db::rfc5911::ID_DATA,
                econtent: None,
            },
            certificates: Some(cms::signed_data::CertificateSet(certs)),
            crls: None,
            signer_infos: cms::signed_data::SignerInfos(signer_infos),
        };

        Ok(Self(signed_data))
    }

    pub fn embedded_certificates(&self) -> Result<Vec<X509Certificate>, Pkcs7Error> {
        Ok(self
            .0
            .certificates
            .as_ref()
            .iter()
            .flat_map(|c| c.as_ref().iter())
            .filter_map(|c| {
                let CertificateChoices::Certificate(c) = c else {
                    return None;
                };
                Some(X509Certificate(
                    crate::x509::symcrypt_rust::X509CertificateInner(c.clone()),
                ))
            })
            .collect())
    }

    pub fn signer_cert_sig(&self) -> Result<(X509Certificate, Vec<u8>), Pkcs7Error> {
        let signers = self.0.signer_infos.as_ref().as_slice();
        if signers.len() != 1 {
            return Err(err(
                der::ErrorKind::Failed.into(),
                "expected exactly one signer in PKCS#7 SignedData",
            ));
        }
        let signer = &signers[0];
        let sid = match &signer.sid {
            SignerIdentifier::IssuerAndSerialNumber(s) => s,
            SignerIdentifier::SubjectKeyIdentifier(_) => {
                return Err(err(
                    der::ErrorKind::Failed.into(),
                    "SubjectKeyIdentifier extension signer id not supported",
                ));
            }
        };
        let cert = self
            .embedded_certificates()?
            .into_iter()
            .find(|x| {
                x.0.0.tbs_certificate().issuer() == &sid.issuer
                    && x.0.0.tbs_certificate().serial_number() == &sid.serial_number
            })
            .ok_or_else(|| {
                err(
                    der::ErrorKind::Failed.into(),
                    "no certificate matches the signer identifier",
                )
            })?;
        Ok((cert, signer.signature.as_bytes().to_vec()))
    }
}
