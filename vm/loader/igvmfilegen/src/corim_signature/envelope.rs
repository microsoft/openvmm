// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Operations on signed CoRIM envelopes (`#6.18(COSE_Sign1)` carrying a
//! `tagged-unsigned-corim-map` payload).
//!
//! Two public entry points:
//!
//! - [`detach_payload`] splits a bundled signed CoRIM into its CoRIM
//!   document and a detached COSE_Sign1 (nil-payload) signature.
//! - [`verify_corim_signature`] cryptographically verifies a detached
//!   signature against a document, using the issuer X.509 certificate
//!   carried in the envelope's `x5chain` / `x5bag` protected header
//!   (RFC 9360).
//!
//! # Design rationale
//!
//! Parsing and encoding both delegate to the `corim` crate's
//! [`decode_signed_corim`] / [`encode_signed_corim`] entry points -- the
//! same code path that the upstream `igvm` crate uses for its CoRIM
//! support. This keeps a single source of truth for signed-CoRIM
//! envelope handling in the workspace and ensures that any envelope we
//! accept also satisfies draft-ietf-rats-corim section 4.2 (protected header
//! must include `corim-meta` or `cwt-claims`).
//!
//! Cryptographic verification is performed via the workspace `crypto`
//! crate's RSA-PSS primitives; only PS384 is currently supported
//! (see [`verify_corim_signature`] for details).
//!
//! [`decode_signed_corim`]: corim::types::signed::decode_signed_corim
//! [`encode_signed_corim`]: corim::types::signed::encode_signed_corim

use anyhow::Context;
use corim::types::signed::CORIM_CONTENT_TYPE;
use corim::types::signed::CoseAlgorithm;
use corim::types::signed::decode_signed_corim;
use corim::types::signed::encode_signed_corim;
use crypto::HashAlgorithm;
use crypto::x509::X509Certificate;

/// Output of [`detach_payload`]: the CoRIM document plus a detached
/// COSE_Sign1 envelope (`payload` field set to nil).
#[derive(Debug)]
pub struct DetachedCorim {
    /// CBOR-encoded CoRIM document extracted from the input envelope.
    pub document: Vec<u8>,
    /// CoRIM-spec `#6.18(COSE_Sign1)` envelope with the payload slot
    /// nil, suitable for [`verify_corim_signature`] against `document`.
    pub signature: Vec<u8>,
}

/// Split a bundled (payload-embedded) COSE_Sign1 into its CoRIM document
/// payload and a detached COSE_Sign1 signature.
///
/// A signed CoRIM is `Tag(18) [ protected, unprotected, payload, signature ]`
/// where `payload` is a `bstr` containing the CBOR-encoded CoRIM document.
///
/// This function:
/// 1. Decodes the COSE_Sign1 with `corim::types::signed::decode_signed_corim`
/// 2. Extracts the raw payload bytes -> returned as the document
/// 3. Re-emits the envelope with the payload field set to nil
///    -> returned as the detached signature
///
/// The protected-header bytes and signature bytes are preserved verbatim
/// across the round-trip: `decode_signed_corim` retains the original
/// `protected_header_bytes` as-is, and `encode_signed_corim` emits them
/// unmodified. This is required because the COSE signature is computed
/// over the exact protected-header bytes.
///
/// # Errors
/// Returns an error if:
/// - the input is not a valid CoRIM-spec-compliant `#6.18(COSE_Sign1)`, or
/// - the input has a nil payload (i.e., is already detached) -- in that
///   case pass the bytes straight to [`verify_corim_signature`] instead
///   of splitting them.
pub fn detach_payload(data: &[u8]) -> anyhow::Result<DetachedCorim> {
    let mut signed = decode_signed_corim(data).context("Signed CoRIM: decode failed")?;

    let document = signed.payload.take().ok_or_else(|| {
        anyhow::anyhow!(
            "Signed CoRIM: payload is nil (already detached); pass the detached \
             signature directly instead of splitting it"
        )
    })?;

    // `payload` is now `None` -> encode produces a detached envelope.
    let signature = encode_signed_corim(&signed)
        .context("Signed CoRIM: failed to encode detached signature")?;

    tracing::debug!(
        input_size = data.len(),
        document_size = document.len(),
        detached_signature_size = signature.len(),
        "Split signed CoRIM into document payload and detached COSE_Sign1 signature"
    );

    Ok(DetachedCorim {
        document,
        signature,
    })
}

/// Cryptographically verify a detached COSE_Sign1 CoRIM signature against
/// the document it endorses.
///
/// The issuer X.509 certificate is taken from the envelope's protected
/// header per RFC 9360: `x5chain` (key 33) is preferred, with `x5bag`
/// (key 32) as a fallback. For a chain or bag, the end-entity (leaf)
/// certificate is used.
///
/// Enforces:
///
/// 1. The envelope decodes as a CoRIM-spec-compliant `#6.18(COSE_Sign1)`
///    via [`decode_signed_corim`].
/// 2. The payload is nil (detached form).
/// 3. The COSE signature bytes are non-empty.
/// 4. If the protected header carries a `content-type` (key 3), it equals
///    `"application/rim+cbor"`.
/// 5. The protected header carries an `x5chain` or `x5bag` entry.
/// 6. The protected-header algorithm is supported (see below).
/// 7. The end-entity certificate parses as DER X.509 and exposes an RSA
///    public key.
/// 8. The signature math verifies via `pss_verify` over the COSE
///    `Sig_structure1` TBS bytes built from the envelope's protected
///    header, the supplied `document`, and empty external AAD.
///
/// # Supported algorithms
///
/// Only **PS384** is currently accepted: RSA-PSS with SHA-384,
/// MGF1-SHA-384, and a salt length equal to the hash output size
/// (48 bytes), per RFC 8230 section 2 (COSE alg ID `-38`).
///
/// All other algorithms (RSA PKCS#1 v1.5, ECDSA, EdDSA, other PSS
/// variants) are rejected with a targeted error -- adding support
/// would require extending the `crypto` crate with the corresponding
/// primitives or COSE alg-ID mappings here.
///
/// # Arguments
/// * `signature` - Detached COSE_Sign1 CoRIM envelope (nil payload).
/// * `document` - The CoRIM document the signature should endorse.
pub fn verify_corim_signature(signature: &[u8], document: &[u8]) -> anyhow::Result<()> {
    let signed = decode_signed_corim(signature).context("CoRIM signature: decode failed")?;

    if !signed.is_detached() {
        anyhow::bail!(
            "CoRIM signature: payload must be nil for a detached signature; \
             embedded payloads must be split first"
        );
    }

    if signed.signature.is_empty() {
        anyhow::bail!("CoRIM signature: COSE signature bytes must be non-empty");
    }

    if let Some(ct) = &signed.protected.content_type
        && ct != CORIM_CONTENT_TYPE
    {
        anyhow::bail!(
            "CoRIM signature: protected content-type is {ct:?}, expected {CORIM_CONTENT_TYPE:?}"
        );
    }

    // Extract the issuer cert from x5chain (key 33) -- falling back to
    // x5bag (key 32). Per RFC 9360 the end-entity is the first cert
    // (chain) or the only cert (single bstr); CoseX509::end_entity()
    // hides that distinction.
    let issuer_cert_der: &[u8] = signed
        .protected
        .x5chain
        .as_ref()
        .or(signed.protected.x5bag.as_ref())
        .map(|x| x.end_entity())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "CoRIM signature: protected header carries neither x5chain (key 33) \
                 nor x5bag (key 32); cannot identify the issuer certificate"
            )
        })?;

    // Only PS384 is supported: RSA-PSS with SHA-384, MGF1-SHA-384, and
    // a salt length equal to the hash output (48 bytes) per RFC 8230
    // section 2.
    let hash = match signed.protected.alg {
        CoseAlgorithm::Ps384 => HashAlgorithm::Sha384,
        other => anyhow::bail!(
            "CoRIM signature: unsupported COSE algorithm {other} ({}). \
             Only PS384 (-38) is supported.",
            other.to_i64(),
        ),
    };

    let cert = X509Certificate::from_der(issuer_cert_der)
        .context("CoRIM signature: failed to parse issuer certificate (expected DER)")?;

    let pubkey = cert
        .public_key()
        .context("CoRIM signature: failed to extract public key from issuer certificate")?;

    let tbs = signed
        .to_be_signed_detached(document, &[])
        .context("CoRIM signature: failed to construct Sig_structure1 TBS bytes")?;

    let valid = pubkey
        .pss_verify(&tbs, &signed.signature, hash)
        .context("CoRIM signature: RSA-PSS verification primitive returned an error")?;

    if !valid {
        anyhow::bail!(
            "CoRIM signature: cryptographic verification failed; signature \
             does not match the supplied document under the issuer's public key"
        );
    }

    tracing::debug!(
        signature_size = signature.len(),
        document_size = document.len(),
        tbs_size = tbs.len(),
        issuer_cert_size = issuer_cert_der.len(),
        alg = %signed.protected.alg,
        "CoRIM signature cryptographically verified against issuer certificate from x5chain/x5bag"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use corim::cbor::value::Value;
    use corim::types::signed::CwtClaims;
    use corim::types::signed::SignedCorimBuilder;
    use test_with_tracing::test;

    const TEST_PAYLOAD: &[u8] = &[0xAA, 0xBB, 0xCC, 0xDD];

    /// Build a CoRIM-spec-compliant bundled `#6.18(COSE_Sign1)` envelope
    /// with the given inner payload and signature.
    fn make_bundled(payload: &[u8], signature: Vec<u8>) -> Vec<u8> {
        SignedCorimBuilder::new(-7_i64, payload.to_vec())
            .set_cwt_claims(CwtClaims::new("test"))
            .build_with_signature(signature)
            .unwrap()
    }

    /// Build a CoRIM-spec-compliant detached `#6.18(COSE_Sign1)` envelope
    /// (payload field is nil).
    fn make_detached(payload: &[u8], signature: Vec<u8>) -> Vec<u8> {
        SignedCorimBuilder::new(-7_i64, payload.to_vec())
            .set_cwt_claims(CwtClaims::new("test"))
            .build_detached_with_signature(signature)
            .unwrap()
    }

    #[test]
    fn split_basic_round_trip() {
        let signature = vec![0xDE; 32];
        let bundled = make_bundled(TEST_PAYLOAD, signature.clone());

        let detached = detach_payload(&bundled).unwrap();
        assert_eq!(detached.document, TEST_PAYLOAD);

        // The detached envelope round-trips as a nil-payload COSE_Sign1
        // with the original signature bytes preserved verbatim.
        let decoded = decode_signed_corim(&detached.signature).unwrap();
        assert_eq!(decoded.signature, signature);
        assert!(decoded.payload.is_none());
    }

    #[test]
    fn split_preserves_signed_bytes() {
        // The detached envelope must keep the protected-header bytes
        // and signature bytes verbatim -- otherwise external signature
        // verification would fail.
        let signature = vec![0x01; 64];
        let bundled = make_bundled(&[0xCA, 0xFE, 0xBA, 0xBE], signature.clone());

        let original = decode_signed_corim(&bundled).unwrap();
        let detached = detach_payload(&bundled).unwrap();
        let after = decode_signed_corim(&detached.signature).unwrap();

        assert_eq!(
            after.protected_header_bytes,
            original.protected_header_bytes
        );
        assert_eq!(after.signature, original.signature);
        assert!(after.payload.is_none());
    }

    #[test]
    fn split_already_detached_errors() {
        let detached = make_detached(TEST_PAYLOAD, vec![0xDE; 32]);
        let err = detach_payload(&detached).unwrap_err();
        assert!(
            err.to_string().contains("already detached"),
            "Error should mention already detached: {err}"
        );
    }

    #[test]
    fn split_empty_errors() {
        assert!(detach_payload(&[]).is_err());
    }

    #[test]
    fn split_large_payload_round_trip() {
        let payload: Vec<u8> = (0..256).map(|i| (i & 0xFF) as u8).collect();
        let signature = vec![0xAB; 64];
        let bundled = make_bundled(&payload, signature.clone());

        let detached = detach_payload(&bundled).unwrap();
        assert_eq!(detached.document, payload);

        let decoded = decode_signed_corim(&detached.signature).unwrap();
        assert_eq!(decoded.signature, signature);
        assert!(decoded.payload.is_none());
    }

    // ---------- verify_corim_signature ----------

    use crate::corim_signature::test_helpers::SIGNER;
    use crate::corim_signature::test_helpers::sign_envelope_for;
    use crate::corim_signature::test_helpers::sign_envelope_no_cert;
    use crate::corim_signature::test_helpers::sign_envelope_with;
    use corim::types::signed::COSE_HEADER_ALG;
    use corim::types::signed::COSE_HEADER_CONTENT_TYPE;
    use corim::types::signed::COSE_HEADER_CWT_CLAIMS;
    use corim::types::signed::COSE_HEADER_X5CHAIN;
    use corim::types::signed::cwt::CWT_CLAIM_ISS;
    use corim::types::tags::TAG_SIGNED_CORIM;
    use crypto::rsa::RsaKeyPair;

    #[test]
    fn verify_ps384_round_trip() {
        let document = b"corim-document-bytes";
        let envelope = sign_envelope_for(document, "test");

        verify_corim_signature(&envelope, document).expect("valid PS384 signature should verify");
    }

    #[test]
    fn verify_rejects_tampered_document() {
        let document = b"original-document";
        let tampered = b"tampered-document";
        let envelope = sign_envelope_for(document, "test");

        let err = verify_corim_signature(&envelope, tampered).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cryptographic verification failed"),
            "Error should report verification failure: {msg}"
        );
    }

    #[test]
    fn verify_rejects_wrong_issuer() {
        // Sign with a fresh key but embed the shared SIGNER's cert in
        // x5chain. The verifier extracts the (wrong) cert from the
        // header and fails to verify the signature against it.
        let document = b"corim-document";
        let signer_key = RsaKeyPair::generate(2048).expect("signer keygen");
        let envelope = sign_envelope_with(&signer_key, document, &SIGNER.cert_der, "test");

        let err = verify_corim_signature(&envelope, document).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cryptographic verification failed"),
            "Error should report verification failure: {msg}"
        );
    }

    #[test]
    fn verify_rejects_unsupported_algorithm() {
        // ES256 is a modeled COSE alg but not accepted; only PS384 is.
        let envelope = SignedCorimBuilder::new(CoseAlgorithm::Es256, b"corim-document".to_vec())
            .set_cwt_claims(CwtClaims::new("test"))
            .add_protected(COSE_HEADER_X5CHAIN, Value::Bytes(b"dummy".to_vec()))
            .build_detached_with_signature(vec![0xDE; 64])
            .unwrap();
        let err = verify_corim_signature(&envelope, b"corim-document").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported COSE algorithm"),
            "Error should report unsupported algorithm: {msg}"
        );
    }

    #[test]
    fn verify_rejects_malformed_cert() {
        // Embed garbage bytes in x5chain. The signature math will run
        // only after cert parsing, so the from_der failure fires first.
        let document = b"corim-document";
        let envelope = sign_envelope_with(
            &SIGNER.key,
            document,
            b"not-a-der-encoded-x509-cert",
            "test",
        );

        let err = verify_corim_signature(&envelope, document).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("issuer certificate"),
            "Error should mention issuer certificate: {msg}"
        );
    }

    #[test]
    fn verify_rejects_missing_issuer_cert() {
        // Envelope without x5chain or x5bag -> cert extraction fails
        // before any signature math runs.
        let envelope = sign_envelope_no_cert(&SIGNER.key, b"doc");
        let err = verify_corim_signature(&envelope, b"doc").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("x5chain") && msg.contains("x5bag"),
            "Error should mention x5chain/x5bag: {msg}"
        );
    }

    #[test]
    fn verify_rejects_attached_payload() {
        let bundled = make_bundled(b"doc", vec![0xDE; 32]);
        let err = verify_corim_signature(&bundled, b"doc").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nil") || msg.contains("embedded"),
            "Error should mention nil or embedded: {msg}"
        );
    }

    #[test]
    fn verify_rejects_empty_signature() {
        let sig = make_detached(b"doc", vec![]);
        let err = verify_corim_signature(&sig, b"doc").unwrap_err();
        assert!(
            err.to_string().contains("non-empty"),
            "Error should mention non-empty: {err}"
        );
    }

    #[test]
    fn verify_rejects_empty_input() {
        assert!(verify_corim_signature(&[], b"doc").is_err());
    }

    #[test]
    fn verify_rejects_untagged_envelope() {
        // CoRIM mandates `#6.18` wrapping; a raw 4-element array
        // without Tag(18) must be rejected by decode_signed_corim.
        let cose = Value::Array(vec![
            Value::Bytes(vec![]),
            Value::Map(vec![]),
            Value::Null,
            Value::Bytes(vec![0xFF; 32]),
        ]);
        let buf = corim::cbor::encode(&cose).unwrap();
        assert!(verify_corim_signature(&buf, b"doc").is_err());
    }

    #[test]
    fn verify_rejects_wrong_content_type() {
        // Manually build a detached envelope whose protected header carries
        // a non-CoRIM content-type. We do this via the raw CBOR codec
        // because SignedCorimBuilder always emits "application/rim+cbor".
        let protected_map = Value::Map(vec![
            (
                Value::Integer(COSE_HEADER_ALG.into()),
                Value::Integer(CoseAlgorithm::Ps384.to_i64().into()),
            ),
            (
                Value::Integer(COSE_HEADER_CONTENT_TYPE.into()),
                Value::Text("application/x-other".into()),
            ),
            (
                Value::Integer(COSE_HEADER_CWT_CLAIMS.into()),
                Value::Map(vec![(
                    Value::Integer(CWT_CLAIM_ISS.into()),
                    Value::Text("test".into()),
                )]),
            ),
        ]);
        let protected_bytes = corim::cbor::encode(&protected_map).unwrap();
        let cose = Value::Tag(
            TAG_SIGNED_CORIM,
            Box::new(Value::Array(vec![
                Value::Bytes(protected_bytes),
                Value::Map(vec![]),
                Value::Null,
                Value::Bytes(vec![0xAB; 16]),
            ])),
        );
        let buf = corim::cbor::encode(&cose).unwrap();
        let err = verify_corim_signature(&buf, b"doc").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("content-type"),
            "Error should mention content-type: {msg}"
        );
    }
}
