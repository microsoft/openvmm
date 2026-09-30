// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared CoRIM test helpers.
//!
//! Generating an RSA-2048 keypair is the slow part of any PS384 signing
//! test (often hundreds of milliseconds). Centralizing the keygen behind
//! a single `LazyLock` makes the whole CoRIM test suite share one key
//! and one self-signed cert, which keeps the suite under a second.

use corim::cbor::value::Value;
use corim::types::signed::COSE_HEADER_X5CHAIN;
use corim::types::signed::CoseAlgorithm;
use corim::types::signed::CwtClaims;
use corim::types::signed::SignedCorimBuilder;
use crypto::HashAlgorithm;
use crypto::rsa::RsaKeyPair;
use crypto::x509::X509Certificate;
use std::sync::LazyLock;

/// Shared 2048-bit RSA key + self-signed cert reused across every test
/// that needs to produce a real PS384-signed envelope.
pub(crate) struct TestSigner {
    pub(crate) key: RsaKeyPair,
    pub(crate) cert_der: Vec<u8>,
}

pub(crate) static SIGNER: LazyLock<TestSigner> = LazyLock::new(|| {
    let key = RsaKeyPair::generate(2048).expect("RSA keygen");
    let cert_der = X509Certificate::build_self_signed(&key, "US", "WA", "Redmond", "Test", "test")
        .expect("self-signed cert")
        .to_der()
        .expect("cert to_der");
    TestSigner { key, cert_der }
});

/// Build a PS384-signed detached CoRIM envelope over `document` using
/// the shared test key, embedding the shared self-signed cert in the
/// `x5chain` protected header so verification accepts it.
///
/// `iss` is included in the CWT claims so two callers can produce
/// envelopes with distinct protected-header bytes (and therefore
/// distinct signatures) over the same document.
pub(crate) fn sign_envelope_for(document: &[u8], iss: &str) -> Vec<u8> {
    sign_envelope_with(&SIGNER.key, document, &SIGNER.cert_der, iss)
}

/// Like [`sign_envelope_for`], but lets the caller supply a specific
/// key and cert. Used by tests that exercise the wrong-issuer or
/// malformed-cert verification paths.
pub(crate) fn sign_envelope_with(
    key: &RsaKeyPair,
    document: &[u8],
    cert_for_x5chain: &[u8],
    iss: &str,
) -> Vec<u8> {
    let mut builder = SignedCorimBuilder::new(CoseAlgorithm::Ps384, document.to_vec())
        .set_cwt_claims(CwtClaims::new(iss))
        .add_protected(COSE_HEADER_X5CHAIN, Value::Bytes(cert_for_x5chain.to_vec()));
    let tbs = builder.to_be_signed(&[]).expect("TBS bytes");
    let sig = key
        .pss_sign(&tbs, HashAlgorithm::Sha384)
        .expect("RSA-PSS sign");
    builder
        .build_detached_with_signature(sig)
        .expect("envelope builds")
}

/// Build a PS384-signed detached envelope without any `x5chain`/`x5bag`
/// header (used to exercise the missing-cert rejection path).
pub(crate) fn sign_envelope_no_cert(key: &RsaKeyPair, document: &[u8]) -> Vec<u8> {
    let mut builder = SignedCorimBuilder::new(CoseAlgorithm::Ps384, document.to_vec())
        .set_cwt_claims(CwtClaims::new("test"));
    let tbs = builder.to_be_signed(&[]).expect("TBS bytes");
    let sig = key
        .pss_sign(&tbs, HashAlgorithm::Sha384)
        .expect("RSA-PSS sign");
    builder
        .build_detached_with_signature(sig)
        .expect("envelope builds")
}
