// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 SignedData parsing on macOS using Security.framework's
//! `CMSDecoder` API.
//!
//! `CMSDecoder` handles the CMS structural decoding, signer enumeration,
//! and embedded-certificate extraction natively. The signer's raw
//! signature bytes are not exposed by any public Security.framework API,
//! so we pull them out of the DER ourselves with a tiny SignerInfo
//! walker. Signature verification, chain walking, and trust evaluation
//! all happen in the shared verifier in [`super`].
//!
//! Signing (test-only) is the one path that does not use Security
//! Framework. `CMSEncoderAddSigners` requires a `SecIdentity` (a
//! certificate paired with its private key in a keychain), which would
//! require routing test keys through a `PKCS#12` import. The test helper
//! instead hand-encodes the SignedData using the `cms` crate (already
//! pulled in for the macOS X.509 backend).

// UNSAFETY: calling Security.framework and CoreFoundation C APIs via FFI.
#![expect(unsafe_code)]

use super::*;
use crate::mac::*;
use std::ptr;

type CMSDecoderRef = CFTypeRef;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn CMSDecoderCreate(decoder_out: *mut CMSDecoderRef) -> OsStatusCode;
    fn CMSDecoderUpdateMessage(
        decoder: CMSDecoderRef,
        bytes: *const u8,
        len: usize,
    ) -> OsStatusCode;
    fn CMSDecoderFinalizeMessage(decoder: CMSDecoderRef) -> OsStatusCode;
    fn CMSDecoderGetNumSigners(decoder: CMSDecoderRef, num_out: *mut usize) -> OsStatusCode;
    fn CMSDecoderCopyAllCerts(decoder: CMSDecoderRef, certs_out: *mut CFArrayRef) -> OsStatusCode;
    fn CMSDecoderCopySignerCert(
        decoder: CMSDecoderRef,
        index: usize,
        cert_out: *mut CFTypeRef,
    ) -> OsStatusCode;
    fn SecCertificateCopyData(cert: CFTypeRef) -> CFDataRef;
}

fn err(e: crate::BackendError) -> Pkcs7Error {
    Pkcs7Error(e)
}

fn x509_to_pkcs7(e: crate::x509::X509Error) -> Pkcs7Error {
    Pkcs7Error(e.0)
}

pub struct Pkcs7SignedDataInner {
    /// Finalized `CMSDecoder`. Used for signer-count and certificate
    /// enumeration.
    decoder: CfHandle,
    /// CFData wrapping the original DER bytes, kept so `signer_cert_sig`
    /// can pull the raw SignerInfo signature octets out and so `to_der`
    /// can return them.
    raw_der: CfHandle,
}

impl Pkcs7SignedDataInner {
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        let raw_der = cf_data(data, "CFDataCreate for SignedData").map_err(err)?;
        let decoder = decoder_from_cfdata(&raw_der).map_err(err)?;
        // Sanity-check that this is a well-formed CMS SignedData with
        // at least one signer; matches the symmetric check done by the
        // other backends in `from_der`.
        let mut signers: usize = 0;
        // SAFETY: decoder is a finalized CMSDecoderRef.
        let s = unsafe { CMSDecoderGetNumSigners(decoder.0, &mut signers) };
        if !s.success() {
            return Err(err(crate::BackendError::OsStatus(
                s,
                "CMSDecoderGetNumSigners",
            )));
        }
        if signers == 0 {
            return Err(err(crate::BackendError::Null(
                "PKCS#7 SignedData has no signers",
            )));
        }
        Ok(Self { decoder, raw_der })
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        // SAFETY: raw_der is an owned CFDataRef.
        Ok(unsafe { cf_data_to_vec(self.raw_der.0) })
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn sign(
        cert: &X509Certificate,
        key_pair: &crate::rsa::RsaKeyPair,
        data: &[u8],
    ) -> Result<Self, crate::rsa::RsaError> {
        // `CMSEncoderAddSigners` needs a `SecIdentity` (a cert paired
        // with its private key in a keychain). Plumbing a test key
        // through PKCS#12 import for that one path is excessive, so the
        // test helper builds the SignedData with the `cms` crate instead.
        let der = cms_sign(cert, key_pair, data)?;
        Self::from_der(&der).map_err(|e| crate::rsa::RsaError(e.0))
    }

    /// Returns the first (and only) signer's embedded certificate and
    /// its raw signature bytes.
    pub fn signer_cert_sig(&self) -> Result<(X509Certificate, Vec<u8>), Pkcs7Error> {
        let mut signers: usize = 0;
        // SAFETY: self.decoder.0 is a finalized CMSDecoderRef.
        let s = unsafe { CMSDecoderGetNumSigners(self.decoder.0, &mut signers) };
        if !s.success() {
            return Err(err(crate::BackendError::OsStatus(
                s,
                "CMSDecoderGetNumSigners",
            )));
        }
        if signers != 1 {
            return Err(err(crate::BackendError::Null(
                "expected exactly one signer in PKCS#7 SignedData",
            )));
        }
        let signer = copy_signer_cert(&self.decoder, 0)?;
        // SAFETY: raw_der is an owned CFDataRef.
        let raw = unsafe { cf_data_to_vec(self.raw_der.0) };
        let signature = extract_first_signer_signature(&raw)?;
        Ok((signer, signature))
    }

    /// Returns every certificate embedded in the SignedData's
    /// certificate bag.
    pub fn embedded_certificates(&self) -> Result<Vec<X509Certificate>, Pkcs7Error> {
        copy_all_certs(&self.decoder).map_err(err)
    }
}

/// Create a `CMSDecoder`, push the bytes of `data` through, and
/// finalize. Returns the owned decoder handle.
fn decoder_from_cfdata(data: &CfHandle) -> Result<CfHandle, crate::BackendError> {
    let mut decoder: CMSDecoderRef = ptr::null();
    // SAFETY: decoder is initialized by CMSDecoderCreate.
    let s = unsafe { CMSDecoderCreate(&mut decoder) };
    if !s.success() {
        return Err(crate::BackendError::OsStatus(s, "CMSDecoderCreate"));
    }
    let decoder = CfHandle(decoder);

    // SAFETY: data is a valid owned CFDataRef.
    let ptr = unsafe { CFDataGetBytePtr(data.0) };
    // SAFETY: data is a valid owned CFDataRef.
    let len = unsafe { CFDataGetLength(data.0) } as usize;
    // SAFETY: ptr is valid for len bytes per CFDataGetBytePtr's
    // contract; decoder is valid.
    let s = unsafe { CMSDecoderUpdateMessage(decoder.0, ptr, len) };
    if !s.success() {
        return Err(crate::BackendError::OsStatus(s, "CMSDecoderUpdateMessage"));
    }
    // SAFETY: decoder valid.
    let s = unsafe { CMSDecoderFinalizeMessage(decoder.0) };
    if !s.success() {
        return Err(crate::BackendError::OsStatus(
            s,
            "CMSDecoderFinalizeMessage",
        ));
    }
    Ok(decoder)
}

fn copy_signer_cert(decoder: &CfHandle, index: usize) -> Result<X509Certificate, Pkcs7Error> {
    let mut cert: CFTypeRef = ptr::null();
    // SAFETY: decoder valid; cert receives an owned SecCertificateRef.
    let s = unsafe { CMSDecoderCopySignerCert(decoder.0, index, &mut cert) };
    if !s.success() {
        return Err(err(crate::BackendError::OsStatus(
            s,
            "CMSDecoderCopySignerCert",
        )));
    }
    if cert.is_null() {
        return Err(err(crate::BackendError::Null("CMSDecoderCopySignerCert")));
    }
    let cert = CfHandle(cert);
    sec_cert_to_x509(cert.0)
}

fn copy_all_certs(decoder: &CfHandle) -> Result<Vec<X509Certificate>, crate::BackendError> {
    let mut arr: CFArrayRef = ptr::null();
    // SAFETY: decoder valid; arr receives an owned CFArray.
    let s = unsafe { CMSDecoderCopyAllCerts(decoder.0, &mut arr) };
    if !s.success() {
        return Err(crate::BackendError::OsStatus(s, "CMSDecoderCopyAllCerts"));
    }
    if arr.is_null() {
        return Ok(Vec::new());
    }
    let arr = CfHandle(arr);
    // SAFETY: arr is a valid CFArrayRef.
    let count = unsafe { CFArrayGetCount(arr.0) };
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        // SAFETY: i is in [0, count); the returned SecCertificateRef is
        // a non-owning borrow from the array, sufficient for the brief
        // SecCertificateCopyData call inside sec_cert_to_x509.
        let c = unsafe { CFArrayGetValueAtIndex(arr.0, i) };
        if c.is_null() {
            continue;
        }
        out.push(sec_cert_to_x509(c).map_err(|e| e.0)?);
    }
    Ok(out)
}

/// Re-wrap a `SecCertificate` as our cross-backend `X509Certificate` by
/// copying its DER bytes and feeding them back through `from_der`.
fn sec_cert_to_x509(cert: CFTypeRef) -> Result<X509Certificate, Pkcs7Error> {
    // SAFETY: cert is a valid SecCertificateRef.
    let der = unsafe { SecCertificateCopyData(cert) };
    if der.is_null() {
        return Err(err(crate::BackendError::Null("SecCertificateCopyData")));
    }
    let der = CfHandle(der);
    // SAFETY: der is a valid owned CFDataRef.
    let bytes = unsafe { cf_data_to_vec(der.0) };
    X509Certificate::from_der(&bytes).map_err(x509_to_pkcs7)
}

/// Walk the DER for `ContentInfo { SignedData { ... signerInfos } }`,
/// drop into the first `SignerInfo`, skip past `version`, `sid`,
/// `digestAlgorithm`, optional `signedAttrs [0]`, and
/// `signatureAlgorithm`, and return the raw octets of the `signature`
/// `OCTET STRING`. `CMSDecoder` does not expose this through any public
/// API.
fn extract_first_signer_signature(p7_der: &[u8]) -> Result<Vec<u8>, Pkcs7Error> {
    fn bogus() -> Pkcs7Error {
        err(crate::BackendError::Null("malformed PKCS#7 SignerInfo"))
    }

    /// Parse one DER TLV. Returns `(tag, contents, rest)`. Supports
    /// short-form lengths and long-form lengths up to 4 length octets,
    /// which is more than enough for any realistic SignedData.
    fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let tag = *input.first()?;
        let mut idx = 1;
        let first = *input.get(idx)?;
        idx += 1;
        let len = if first & 0x80 == 0 {
            first as usize
        } else {
            let n = (first & 0x7f) as usize;
            if n == 0 || n > 4 {
                return None;
            }
            let mut v = 0usize;
            for _ in 0..n {
                v = (v << 8) | (*input.get(idx)? as usize);
                idx += 1;
            }
            v
        };
        let end = idx.checked_add(len)?;
        Some((tag, input.get(idx..end)?, &input[end..]))
    }

    fn into_tag(input: &[u8], expected: u8) -> Option<&[u8]> {
        let (tag, content, _) = tlv(input)?;
        (tag == expected).then_some(content)
    }

    // ContentInfo SEQUENCE
    let ci = into_tag(p7_der, 0x30).ok_or_else(bogus)?;
    // contentType OID — discard
    let (_, _, after_oid) = tlv(ci).ok_or_else(bogus)?;
    // [0] EXPLICIT content
    let explicit = into_tag(after_oid, 0xa0).ok_or_else(bogus)?;
    // SignedData SEQUENCE
    let sd = into_tag(explicit, 0x30).ok_or_else(bogus)?;

    // Skip version INTEGER, digestAlgorithms SET, encapContentInfo SEQ.
    let (_, _, rest) = tlv(sd).ok_or_else(bogus)?;
    let (_, _, rest) = tlv(rest).ok_or_else(bogus)?;
    let (_, _, mut rest) = tlv(rest).ok_or_else(bogus)?;
    // Optional [0] IMPLICIT certificates and [1] IMPLICIT crls.
    while let Some((tag, _, after)) = tlv(rest) {
        if tag == 0xa0 || tag == 0xa1 {
            rest = after;
        } else {
            break;
        }
    }

    // signerInfos SET OF
    let signer_infos = into_tag(rest, 0x31).ok_or_else(bogus)?;
    // First SignerInfo SEQUENCE
    let si = into_tag(signer_infos, 0x30).ok_or_else(bogus)?;

    // Skip version INTEGER, sid, digestAlgorithm SEQ.
    let (_, _, rest) = tlv(si).ok_or_else(bogus)?;
    let (_, _, rest) = tlv(rest).ok_or_else(bogus)?;
    let (_, _, mut rest) = tlv(rest).ok_or_else(bogus)?;
    // Optional [0] IMPLICIT signedAttrs.
    if let Some((0xa0, _, after)) = tlv(rest) {
        rest = after;
    }
    // signatureAlgorithm SEQ
    let (_, _, rest) = tlv(rest).ok_or_else(bogus)?;
    // signature OCTET STRING
    let (tag, sig, _) = tlv(rest).ok_or_else(bogus)?;
    if tag != 0x04 {
        return Err(bogus());
    }
    Ok(sig.to_vec())
}

#[cfg(any(test, feature = "test_helpers"))]
fn cms_sign(
    cert: &X509Certificate,
    key_pair: &crate::rsa::RsaKeyPair,
    data: &[u8],
) -> Result<Vec<u8>, crate::rsa::RsaError> {
    use cms::cert::CertificateChoices;
    use cms::content_info::ContentInfo;
    use cms::signed_data::SignedData;
    use cms::signed_data::SignerIdentifier;
    use der::Decode;
    use der::Encode;
    use der::asn1::SetOfVec;
    use der::oid::db::rfc5911::ID_SIGNED_DATA;
    use x509_cert::Certificate;
    use x509_cert::spki::AlgorithmIdentifierOwned;

    let signature = key_pair.pkcs1_sign(data, crate::HashAlgorithm::Sha256)?;
    let cert_der = cert.to_der().map_err(|e| crate::rsa::RsaError(e.0))?;
    let parsed_cert = Certificate::from_der(&cert_der)
        .map_err(|e| crate::rsa::RsaError(crate::BackendError::Der(e, "parsing signer cert")))?;

    let digest_alg = AlgorithmIdentifierOwned {
        oid: der::oid::db::rfc5912::ID_SHA_256,
        parameters: None,
    };
    let signature_algorithm = AlgorithmIdentifierOwned {
        oid: der::oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION,
        parameters: None,
    };

    let sid = SignerIdentifier::IssuerAndSerialNumber(cms::cert::IssuerAndSerialNumber {
        issuer: parsed_cert.tbs_certificate().issuer().clone(),
        serial_number: parsed_cert.tbs_certificate().serial_number().clone(),
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
        .insert(CertificateChoices::Certificate(parsed_cert))
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

    let sd_der = signed_data
        .to_der()
        .map_err(|e| crate::rsa::RsaError(crate::BackendError::Der(e, "encoding SignedData")))?;
    let content = der::AnyRef::try_from(sd_der.as_slice()).map_err(|e| {
        crate::rsa::RsaError(crate::BackendError::Der(e, "wrapping SignedData in Any"))
    })?;
    let ci = ContentInfo {
        content_type: ID_SIGNED_DATA,
        content: content.into(),
    };
    ci.to_der()
        .map_err(|e| crate::rsa::RsaError(crate::BackendError::Der(e, "encoding ContentInfo")))
}
