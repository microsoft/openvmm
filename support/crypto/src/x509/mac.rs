// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate parsing and verification on macOS.
//!
//! The certificate's ASN.1 structure is decoded and re-encoded with the
//! `x509-cert` and `der` crates. Public-key extraction, distinguished-name
//! normalization, and common-name lookup are delegated to
//! Security.framework via the `SecCertificate` API.

// UNSAFETY: calling Security.framework and CoreFoundation C APIs via FFI.
#![expect(unsafe_code)]

use super::X509Error;
use crate::mac::*;
use der::Decode;
use der::Encode;
use std::ptr;
use x509_cert::Certificate;

type SecCertificateRef = CFTypeRef;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecCertificateCreateWithData(
        allocator: CFAllocatorRef,
        data: CFDataRef,
    ) -> SecCertificateRef;
    fn SecCertificateCopyKey(cert: SecCertificateRef) -> SecKeyRef;
    fn SecCertificateCopyCommonName(
        cert: SecCertificateRef,
        common_name: *mut CFStringRef,
    ) -> OsStatusCode;
    fn SecCertificateCopyNormalizedSubjectSequence(cert: SecCertificateRef) -> CFDataRef;
    fn SecCertificateCopyNormalizedIssuerSequence(cert: SecCertificateRef) -> CFDataRef;

    fn SecKeyCopyExternalRepresentation(key: SecKeyRef, error: *mut CFErrorRef) -> CFDataRef;
}

fn err(e: crate::BackendError) -> X509Error {
    X509Error(e)
}

fn rsa_err(e: crate::BackendError) -> crate::rsa::RsaError {
    crate::rsa::RsaError(e)
}

fn der_err(e: der::Error, op: &'static str) -> X509Error {
    X509Error(crate::BackendError::Der(e, op))
}

fn rsa_der_err(e: der::Error, op: &'static str) -> crate::rsa::RsaError {
    crate::rsa::RsaError(crate::BackendError::Der(e, op))
}

fn null_err(op: &'static str) -> X509Error {
    X509Error(crate::BackendError::Null(op))
}

fn rsa_null_err(op: &'static str) -> crate::rsa::RsaError {
    crate::rsa::RsaError(crate::BackendError::Null(op))
}

/// A SecCertificate paired with the parsed ASN.1 view of the same DER
/// bytes. The Security.framework handle is used for public-key extraction
/// and DN normalization; the parsed view is used for everything else.
pub struct X509CertificateInner {
    cert: CfHandle,
    parsed: Certificate,
}

impl X509CertificateInner {
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        let parsed =
            Certificate::from_der(data).map_err(|e| der_err(e, "parsing DER certificate"))?;
        let cf = cf_data(data, "CFDataCreate for certificate").map_err(err)?;
        // SAFETY: cf.0 is a valid CFDataRef.
        let cert = unsafe { SecCertificateCreateWithData(kCFAllocatorDefault, cf.0) };
        if cert.is_null() {
            return Err(null_err("SecCertificateCreateWithData"));
        }
        Ok(Self {
            cert: CfHandle(cert),
            parsed,
        })
    }

    pub fn public_key(&self) -> Result<crate::rsa::RsaPublicKey, crate::rsa::RsaError> {
        // SAFETY: self.cert.0 is a valid SecCertificateRef.
        let key = unsafe { SecCertificateCopyKey(self.cert.0) };
        if key.is_null() {
            return Err(rsa_null_err("SecCertificateCopyKey"));
        }
        let key = CfHandle(key);
        let mut error: CFErrorRef = ptr::null();
        // SAFETY: key.0 is a valid SecKeyRef.
        let data = unsafe { SecKeyCopyExternalRepresentation(key.0, &mut error) };
        if data.is_null() {
            // SAFETY: error is null or a valid CFErrorRef.
            return Err(rsa_err(unsafe {
                sec_err(error, "SecKeyCopyExternalRepresentation")
            }));
        }
        let data = CfHandle(data);
        // SAFETY: data.0 is a valid CFDataRef.
        let pkcs1_der = unsafe { cf_data_to_vec(data.0) };
        let pk = pkcs1::RsaPublicKey::from_der(&pkcs1_der)
            .map_err(|e| rsa_der_err(e, "parsing PKCS#1 RSA public key"))?;
        crate::rsa::RsaPublicKey::from_components(
            pk.modulus.as_bytes(),
            pk.public_exponent.as_bytes(),
        )
    }

    pub fn verify(
        &self,
        issuer_public_key: &crate::rsa::RsaPublicKey,
    ) -> Result<bool, crate::rsa::RsaError> {
        let oid = self.parsed.signature_algorithm().oid;
        let hash = crate::HashAlgorithm::try_from(oid)
            .map_err(|e| rsa_der_err(e, "unrecognized signature algorithm OID"))?;

        let tbs_der = self
            .parsed
            .tbs_certificate()
            .to_der()
            .map_err(|e| rsa_der_err(e, "encoding TBS certificate"))?;
        let signature = self.parsed.signature().raw_bytes();
        issuer_public_key.pkcs1_verify(&tbs_der, signature, hash)
    }

    pub fn issued(&self, subject: &X509CertificateInner) -> Result<bool, X509Error> {
        use x509_cert::ext::pkix::AuthorityKeyIdentifier;
        use x509_cert::ext::pkix::KeyUsage;
        use x509_cert::ext::pkix::SubjectKeyIdentifier;
        use x509_cert::ext::pkix::name::GeneralName;

        // Compare DNs via Security.framework's normalized form so equivalent
        // encodings still match.
        let issuer_subject = copy_normalized(self.cert.0, /*subject=*/ true)?;
        let subject_issuer = copy_normalized(subject.cert.0, /*subject=*/ false)?;
        if issuer_subject != subject_issuer {
            return Ok(false);
        }

        let issuer_tbs = self.parsed.tbs_certificate();
        let subject_tbs = subject.parsed.tbs_certificate();

        if let Some((_crit, ku)) = issuer_tbs
            .get_extension::<KeyUsage>()
            .map_err(|e| der_err(e, "parsing KeyUsage extension"))?
            && !ku.key_cert_sign()
        {
            return Ok(false);
        }

        if let Some((_crit, akid)) = subject_tbs
            .get_extension::<AuthorityKeyIdentifier>()
            .map_err(|e| der_err(e, "parsing AuthorityKeyIdentifier extension"))?
        {
            if let Some(akid_key_id) = &akid.key_identifier {
                let skid = issuer_tbs
                    .get_extension::<SubjectKeyIdentifier>()
                    .map_err(|e| der_err(e, "parsing SubjectKeyIdentifier extension"))?;
                match skid {
                    Some((_crit, ski)) => {
                        if akid_key_id != &ski.0 {
                            return Ok(false);
                        }
                    }
                    None => return Ok(false),
                }
            }
            if let Some(akid_serial) = &akid.authority_cert_serial_number {
                if akid_serial != issuer_tbs.serial_number() {
                    return Ok(false);
                }
            }
            if let Some(gens) = &akid.authority_cert_issuer {
                let mut has_dn = false;
                let has_matching_dn = gens.iter().any(|g| match g {
                    GeneralName::DirectoryName(dn) => {
                        has_dn = true;
                        dn == issuer_tbs.subject()
                    }
                    _ => false,
                });
                if has_dn && !has_matching_dn {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    pub fn to_der(&self) -> Result<Vec<u8>, X509Error> {
        self.parsed
            .to_der()
            .map_err(|e| der_err(e, "encoding certificate as DER"))
    }

    pub fn issuer_dn(&self) -> Result<String, X509Error> {
        Ok(self.parsed.tbs_certificate().issuer().to_string())
    }

    pub fn serial_number(&self) -> Result<Vec<u8>, X509Error> {
        Ok(self
            .parsed
            .tbs_certificate()
            .serial_number()
            .as_bytes()
            .to_vec())
    }

    pub fn subject_common_name(&self) -> Result<Option<String>, X509Error> {
        let mut cn: CFStringRef = ptr::null();
        // SAFETY: self.cert.0 is a valid SecCertificateRef.
        let status = unsafe { SecCertificateCopyCommonName(self.cert.0, &mut cn) };
        if status.0 != 0 {
            return Err(err(crate::BackendError::OsStatus(
                status,
                "SecCertificateCopyCommonName",
            )));
        }
        if cn.is_null() {
            return Ok(None);
        }
        // SAFETY: cn is a valid owned CFStringRef.
        Ok(Some(unsafe { cf_string_to_string(cn) }))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn build_self_signed(
        key: &crate::rsa::RsaKeyPair,
        country: &str,
        state: &str,
        locality: &str,
        organization: &str,
        common_name: &str,
    ) -> anyhow::Result<Self> {
        use super::builder::rsa_keypair_signer::RsaKeyPairSigner;
        use x509_cert::builder::Builder;

        let (builder, pkcs1_der) = super::builder::self_signed_builder(
            key,
            country,
            state,
            locality,
            organization,
            common_name,
        )?;
        let cert = builder.build(&RsaKeyPairSigner { key, pkcs1_der })?;
        Ok(Self::from_der(&cert.to_der()?)?)
    }
}

/// Copy the normalized subject or issuer DN bytes from a certificate.
fn copy_normalized(cert: SecCertificateRef, subject: bool) -> Result<Vec<u8>, X509Error> {
    // SAFETY: cert is a valid SecCertificateRef.
    let data = unsafe {
        if subject {
            SecCertificateCopyNormalizedSubjectSequence(cert)
        } else {
            SecCertificateCopyNormalizedIssuerSequence(cert)
        }
    };
    if data.is_null() {
        return Err(null_err(
            "SecCertificateCopyNormalized{Subject,Issuer}Sequence",
        ));
    }
    let data = CfHandle(data);
    // SAFETY: data.0 is a valid CFDataRef.
    Ok(unsafe { cf_data_to_vec(data.0) })
}
