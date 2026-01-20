// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module handles parsing JSON Web Token (JWT) data.

use base64::Engine;
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::rsa::Padding;
use openssl::sign::Verifier;
use openssl::x509::X509;
use openssl::x509::X509VerifyResult;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Write;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum JwtError {
    #[error("JWT data is not valid UTF-8: {0}")]
    NonUtf8JwtData(String),
    #[error("invalid JWT format, data: {0}")]
    InvalidJwtFormat(String),
    #[error("JWT header is not valid UTF-8: {0}")]
    NonUtf8JwtHeader(String),
    #[error("JWT body is not valid UTF-8: {0}")]
    NonUtf8JwtBody(String),
    #[error("failed to decode JWT header in base64 url format")]
    DecodeBase64UrlJwtHeader(#[source] base64::DecodeError),
    #[error("failed to decode JWT body in base64 url format")]
    DecodeBase64UrlJwtBody(#[source] base64::DecodeError),
    #[error("failed to decode JWT signature in base64 url format")]
    DecodeBase64UrlJwtSignature(#[source] base64::DecodeError),
    #[error("failed to deserialize Jwt header into JSON")]
    JwtHeaderToJson(#[source] serde_json::Error),
    #[error("failed to deserialize Jwt body into JSON")]
    JwtBodyToJson(#[source] serde_json::Error),
    #[error("failed to decode X.509 certificate base64 format")]
    DecodeBase64JwtX509Certificate(#[source] base64::DecodeError),
    #[error("failed to convert raw bytes into X509 struct")]
    RawBytesToX509(#[source] openssl::error::ErrorStack),
    #[error("failed to validate certificate chain")]
    CertificateChainValidation(#[from] CertificateChainValidationError),
    #[error("failed to verify JWT signature")]
    JwtSignatureVerification(#[from] JwtSignatureVerificationError),
}

#[derive(Debug, Error)]
pub(crate) enum JwtSignatureVerificationError {
    #[error("invalid key type {key_type:?}, expected {expected_type:?}")]
    InvalidKeyType {
        key_type: openssl::pkey::Id,
        expected_type: openssl::pkey::Id,
    },
    #[error("Verifier::new() failed")]
    VerifierNew(#[source] openssl::error::ErrorStack),
    #[error("Verifier set_rsa_padding() with PKCS1 failed")]
    VerifierSetRsaPaddingPkcs1(#[source] openssl::error::ErrorStack),
    #[error("Verifier update() failed")]
    VerifierUpdate(#[source] openssl::error::ErrorStack),
    #[error("Verifier verify() failed")]
    VerifierVerify(#[source] openssl::error::ErrorStack),
}

#[derive(Debug, Error)]
pub(crate) enum CertificateChainValidationError {
    #[error("certificate chain is empty")]
    CertChainIsEmpty,
    #[error("failed to get public key from the certificate")]
    GetPublicKeyFromCertificate(#[source] openssl::error::ErrorStack),
    #[error("failed to verify the child certificate signature with parent public key")]
    VerifyChildSignatureWithParentPublicKey(#[source] openssl::error::ErrorStack),
    #[error("cert chain validation failed -- signature mismatch")]
    CertChainSignatureMismatch,
    #[error("cert chain validation failed -- subject and issuer mismatch")]
    CertChainSubjectIssuerMismatch,
}

/// JWT signature algorithms.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum JwtAlgorithm {
    /// RSA signature with SHA-256
    RS256,
}

/// Subset of a standard JWT header.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JwtHeader {
    /// Indicates the signing algorithm. "none" indicates the JWT is unsigned.
    pub alg: JwtAlgorithm,
    /// The certificate chain used to validate the signature if the JWT is signed.
    #[serde(default)]
    pub x5c: Vec<String>,
}

/// Parsed content of a JWT.
#[derive(Debug)]
pub(crate) struct Jwt<B> {
    pub header: JwtHeader,
    pub body: B,
    pub signature: Vec<u8>,
}

/// Helper struct for parsing and validating a JWT.
pub(crate) struct JwtHelper<B> {
    pub jwt: Jwt<B>,
    // Raw bytes of `header.body` used to generate the signature.
    pub payload: String,
}

impl<B: DeserializeOwned> JwtHelper<B> {
    /// Parse the given JWT.
    pub fn from(data: &[u8]) -> Result<Self, JwtError> {
        // A JWT looks like:
        // Base64URL(Header).Base64URL(Body).Base64URL(Signature)
        // Header and Body are JSON payloads

        // Utf8Error is ignored below but will be used in `string_from_utf8_preserve_invalid_bytes`
        let utf8 = std::str::from_utf8(data)
            .map_err(|_| JwtError::NonUtf8JwtData(string_from_utf8_preserve_invalid_bytes(data)))?;

        let [header, body, signature]: [&str; 3] = utf8
            .split('.')
            .collect::<Vec<&str>>()
            .try_into()
            .map_err(|_| JwtError::InvalidJwtFormat(utf8.to_string()))?;

        let (signature, payload) = if !signature.is_empty() {
            let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(signature)
                .map_err(JwtError::DecodeBase64UrlJwtSignature)?;

            (signature, [header, ".", body].concat())
        } else {
            (vec![], "".to_string())
        };

        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header)
            .map_err(JwtError::DecodeBase64UrlJwtHeader)?;
        let header = std::str::from_utf8(&header).map_err(|_| {
            JwtError::NonUtf8JwtHeader(string_from_utf8_preserve_invalid_bytes(header.as_slice()))
        })?;
        let header: JwtHeader = serde_json::from_str(header).map_err(JwtError::JwtHeaderToJson)?;

        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(JwtError::DecodeBase64UrlJwtBody)?;
        let body = std::str::from_utf8(&body).map_err(|_| {
            JwtError::NonUtf8JwtBody(string_from_utf8_preserve_invalid_bytes(body.as_slice()))
        })?;
        let body: B = serde_json::from_str(body).map_err(JwtError::JwtBodyToJson)?;

        Ok(Self {
            jwt: Jwt {
                header,
                body,
                signature,
            },
            payload,
        })
    }

    /// Get the cert chain from the JWT's x5c header.
    pub fn cert_chain(&self) -> Result<Vec<X509>, JwtError> {
        self.jwt
            .header
            .x5c
            .iter()
            .map(|encoded_cert| {
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(encoded_cert)
                    .map_err(JwtError::DecodeBase64JwtX509Certificate)?;
                X509::from_der(&raw).map_err(JwtError::RawBytesToX509)
            })
            .collect::<Result<Vec<_>, _>>()
    }

    /// Verify the JWT's signature. Ok(true) means a valid signature; Ok(false)
    /// or Err indicate an invalid signature or other error.
    pub fn verify_signature(&self) -> Result<bool, JwtError> {
        let alg = &self.jwt.header.alg;
        let pkey = validate_cert_chain(&self.cert_chain()?)?;

        let result =
            verify_jwt_signature(alg, &pkey, self.payload.as_bytes(), &self.jwt.signature)?;

        Ok(result)
    }
}

/// Convert a potentially non UTF-8 byte array into a string with non UTF-8 characters represented
/// as hexadecimal escape sequences.
fn string_from_utf8_preserve_invalid_bytes(bytes: &[u8]) -> String {
    let mut accumulator = String::new();

    let mut index = 0;
    while index < bytes.len() {
        match std::str::from_utf8(&bytes[index..]) {
            Ok(utf8_str) => {
                accumulator.push_str(utf8_str);
                break;
            }
            Err(err) => {
                let (valid, invalid) = bytes[index..].split_at(err.valid_up_to());

                // Unwrap is unreachable here because the bytes are guaranteed to be valid UTF-8
                accumulator.push_str(std::str::from_utf8(valid).unwrap());

                if let Some(invalid_byte_length) = err.error_len() {
                    for byte in &invalid[..invalid_byte_length] {
                        let _ = write!(accumulator, "\\x{byte:02X}");
                    }
                    // Move index past processed bytes
                    index += err.valid_up_to() + invalid_byte_length;
                } else {
                    // In the event that the error length cannot be found (e.g.: unexpected end of input)
                    // just capture the remaining bytes as hex escape sequences
                    for byte in invalid {
                        let _ = write!(accumulator, "\\x{byte:02X}");
                    }

                    break;
                }
            }
        }
    }

    accumulator
}

/// Helper function for JWT signature validation using OpenSSL.
fn verify_jwt_signature(
    alg: &JwtAlgorithm,
    pkey: &PKey<openssl::pkey::Public>,
    payload: &[u8],
    signature: &[u8],
) -> Result<bool, JwtSignatureVerificationError> {
    let result = match alg {
        JwtAlgorithm::RS256 => {
            if pkey.id() != openssl::pkey::Id::RSA {
                Err(JwtSignatureVerificationError::InvalidKeyType {
                    key_type: pkey.id(),
                    expected_type: openssl::pkey::Id::RSA,
                })?
            }

            let mut verifier = Verifier::new(MessageDigest::sha256(), pkey)
                .map_err(JwtSignatureVerificationError::VerifierNew)?;
            verifier
                .set_rsa_padding(Padding::PKCS1)
                .map_err(JwtSignatureVerificationError::VerifierSetRsaPaddingPkcs1)?;
            verifier
                .update(payload)
                .map_err(JwtSignatureVerificationError::VerifierUpdate)?;
            verifier
                .verify(signature)
                .map_err(JwtSignatureVerificationError::VerifierVerify)?
        }
    };

    Ok(result)
}

/// Helper function for x509 certificate chain validation using OpenSSL.
fn validate_cert_chain(
    cert_chain: &[X509],
) -> Result<PKey<openssl::pkey::Public>, CertificateChainValidationError> {
    if cert_chain.is_empty() {
        Err(CertificateChainValidationError::CertChainIsEmpty)?
    }

    // Only validate the subject-issuer pair and signature (without validity)
    // assuming there is no trusted time source
    for i in 0..cert_chain.len() {
        if i < cert_chain.len() - 1 {
            let child = &cert_chain[i];
            let parent = &cert_chain[i + 1];
            let public_key = parent
                .public_key()
                .map_err(CertificateChainValidationError::GetPublicKeyFromCertificate)?;

            let verified = child.verify(&public_key).map_err(
                CertificateChainValidationError::VerifyChildSignatureWithParentPublicKey,
            )?;
            if !verified {
                Err(CertificateChainValidationError::CertChainSignatureMismatch)?
            }

            let issued = parent.issued(child);
            if issued != X509VerifyResult::OK {
                Err(CertificateChainValidationError::CertChainSubjectIssuerMismatch)?
            }
        }
    }

    cert_chain[0]
        .public_key()
        .map_err(CertificateChainValidationError::GetPublicKeyFromCertificate)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_helpers::CIPHERTEXT;
    use openhcl_attestation_protocol::igvm_attest::akv;
    use openssl::x509::X509Name;

    /// Empty JWT body type to use for parsing invalid JWTs.
    #[derive(Debug, Serialize, Deserialize)]
    struct JwtTestBody {}

    #[test]
    fn generate_string_from_non_utf8_bytes() {
        // valid UTF-8 strings
        let data = "Some utf8 data".as_bytes();
        let result = string_from_utf8_preserve_invalid_bytes(data);
        assert_eq!(result, "Some utf8 data");

        let data = "Some utf8 data 😊".as_bytes();
        let result = string_from_utf8_preserve_invalid_bytes(data);
        assert_eq!(result, "Some utf8 data 😊");

        let data = "😊".as_bytes();
        let result = string_from_utf8_preserve_invalid_bytes(data);
        assert_eq!(result, "😊");

        // valid and invalid UTF-8 strings
        let mut data = "Some utf8 data ".as_bytes().to_vec();
        data.push(0x91);
        data.push(0x92);
        data.extend(" with some non-utf8 data".as_bytes());
        data.push(0x93);
        assert_eq!(
            string_from_utf8_preserve_invalid_bytes(data.as_slice()),
            "Some utf8 data \\x91\\x92 with some non-utf8 data\\x93"
        );

        let mut data = vec![0x91];
        data.extend("😊".as_bytes());
        let result = string_from_utf8_preserve_invalid_bytes(data.as_slice());
        assert_eq!(result, "\\x91😊");

        let mut data = "😊".as_bytes().to_vec();
        data.push(0x91);
        let result = string_from_utf8_preserve_invalid_bytes(data.as_slice());
        assert_eq!(result, "😊\\x91");

        let mut data = "Some utf8 data 😊".as_bytes().to_vec();
        data.push(0x91);
        data.push(0x92);
        data.extend(" with some non-utf8 data".as_bytes());
        data.push(0x93);
        assert_eq!(
            string_from_utf8_preserve_invalid_bytes(data.as_slice()),
            "Some utf8 data 😊\\x91\\x92 with some non-utf8 data\\x93"
        );

        // invalid UTF-8 strings
        let data = vec![0x91, 0x92, 0x93];
        let result = string_from_utf8_preserve_invalid_bytes(data.as_slice());
        assert_eq!(result, "\\x91\\x92\\x93");

        // UTF-16 string
        let data = "UTF-16 encoded"
            .encode_utf16()
            .collect::<Vec<u16>>()
            .iter()
            .flat_map(|character| character.to_ne_bytes())
            .collect::<Vec<u8>>();
        let result = string_from_utf8_preserve_invalid_bytes(data.as_slice());
        assert_eq!(result, "U\0T\0F\0-\x001\x006\0 \0e\0n\0c\0o\0d\0e\0d\0");
    }

    #[test]
    fn jwt_from_bytes() {
        let rsa_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let private = PKey::from_rsa(rsa_key.clone()).unwrap();

        let (header, body, signature) =
            crate::test_helpers::generate_base64_encoded_jwt_components(&private);

        let jwt = format!("{}.{}.{}", header, body, signature);
        let jwt = JwtHelper::<akv::AkvKeyReleaseJwtBody>::from(jwt.as_bytes()).unwrap();

        assert_eq!(jwt.jwt.header.alg, JwtAlgorithm::RS256);

        let key_hsm = akv::AkvKeyReleaseKeyBlob {
            ciphertext: CIPHERTEXT.as_bytes().to_vec(),
        };

        assert_eq!(
            jwt.jwt.body.response.key.key.key_hsm,
            serde_json::to_string(&key_hsm).unwrap().as_bytes()
        );
    }

    #[test]
    fn jwt_from_bytes_with_empty_signature() {
        let rsa_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let private = PKey::from_rsa(rsa_key.clone()).unwrap();

        let (header, body, _) =
            crate::test_helpers::generate_base64_encoded_jwt_components(&private);

        let jwt = format!("{}.{}.{}", header, body, "");
        let jwt = JwtHelper::<akv::AkvKeyReleaseJwtBody>::from(jwt.as_bytes()).unwrap();

        assert_eq!(jwt.jwt.signature, Vec::<u8>::from([]));
    }

    #[test]
    fn successfully_verify_jwt_signature() {
        let rsa_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let private = PKey::from_rsa(rsa_key).unwrap();

        let (header, body, signature) =
            crate::test_helpers::generate_base64_encoded_jwt_components(&private);

        let jwt = format!("{}.{}.{}", header, body, signature);
        let jwt = JwtHelper::<akv::AkvKeyReleaseJwtBody>::from(jwt.as_bytes()).unwrap();

        let verification_succeeded = jwt.verify_signature().unwrap();
        assert!(verification_succeeded);
    }

    #[test]
    fn successfully_verify_jwt_signature_helper_function() {
        let rsa_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let private = PKey::from_rsa(rsa_key.clone()).unwrap();
        let pem = rsa_key.public_key_to_pem().unwrap();
        let public = PKey::public_key_from_pem(&pem).unwrap();

        let payload = "test";
        let mut signer = openssl::sign::Signer::new(MessageDigest::sha256(), &private).unwrap();
        signer.set_rsa_padding(Padding::PKCS1).unwrap();
        signer.update(payload.as_bytes()).unwrap();
        let signature = signer.sign_to_vec().unwrap();

        let verification_succeeded = verify_jwt_signature(
            &JwtAlgorithm::RS256,
            &public,
            payload.as_bytes(),
            signature.as_slice(),
        )
        .unwrap();
        assert!(verification_succeeded);
    }

    #[test]
    fn fail_to_verify_inconsistent_rs256_signature() {
        let dsa_key = openssl::dsa::Dsa::generate(2048).unwrap();
        let pem = dsa_key.public_key_to_pem().unwrap();
        let public = PKey::public_key_from_pem(&pem).unwrap();

        let outcome = verify_jwt_signature(&JwtAlgorithm::RS256, &public, &[], &[]);

        assert!(outcome.is_err());
        assert_eq!(
            outcome.unwrap_err().to_string(),
            "invalid key type Id(116), expected Id(6)".to_string()
        );
    }

    #[test]
    fn fail_to_verify_empty_certificate_chain() {
        let outcome = validate_cert_chain(&[]);

        assert!(outcome.is_err());
        assert_eq!(
            outcome.unwrap_err().to_string(),
            CertificateChainValidationError::CertChainIsEmpty.to_string()
        );
    }

    #[test]
    fn fail_to_verify_certificate_chain_with_various_signers() {
        let cert_rsa_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let cert_private = PKey::from_rsa(cert_rsa_key).unwrap();

        let intermediate_rsa_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let intermediate_private = PKey::from_rsa(intermediate_rsa_key).unwrap();

        let root_rsa_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let root_private = PKey::from_rsa(root_rsa_key).unwrap();

        let cert = crate::test_helpers::generate_x509(&cert_private);
        let intermediate = crate::test_helpers::generate_x509(&intermediate_private);
        let root = crate::test_helpers::generate_x509(&root_private);

        let cert_chain = vec![cert, intermediate, root];

        let outcome = validate_cert_chain(&cert_chain);

        assert!(outcome.is_err());
        assert_eq!(
            outcome.unwrap_err().to_string(),
            CertificateChainValidationError::CertChainSignatureMismatch.to_string()
        );
    }

    #[test]
    fn fail_to_verify_certificate_chain_with_mismatched_subject_and_issuer() {
        let rsa_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let private = PKey::from_rsa(rsa_key).unwrap();
        let public = private.public_key_to_pem().unwrap();
        let public = PKey::public_key_from_pem(&public).unwrap();

        let cert = crate::test_helpers::generate_x509(&private);
        let intermediate = crate::test_helpers::generate_x509(&private);

        let mut root = X509::builder().unwrap();

        root.set_pubkey(&public).unwrap();

        root.set_version(2).unwrap();
        root.set_serial_number(
            &openssl::bn::BigNum::from_u32(1)
                .unwrap()
                .to_asn1_integer()
                .unwrap(),
        )
        .unwrap();
        root.set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        root.set_not_after(&openssl::asn1::Asn1Time::days_from_now(365).unwrap())
            .unwrap();

        let mut name = X509Name::builder().unwrap();
        name.append_entry_by_text("C", "US").unwrap();
        name.append_entry_by_text("ST", "Washington").unwrap();
        name.append_entry_by_text("L", "Redmond").unwrap();
        name.append_entry_by_text("O", "ACME INC").unwrap();
        name.append_entry_by_text("CN", "acme.com").unwrap();
        let name = name.build();
        root.set_subject_name(&name).unwrap();
        root.set_issuer_name(&name).unwrap();

        root.sign(&private, MessageDigest::sha256()).unwrap();
        let root = root.build();

        let cert_chain = vec![cert, intermediate, root];

        let outcome = validate_cert_chain(&cert_chain);

        assert!(outcome.is_err());
        assert_eq!(
            outcome.unwrap_err().to_string(),
            CertificateChainValidationError::CertChainSubjectIssuerMismatch.to_string()
        );
    }

    #[test]
    fn fail_to_parse_non_utf8_jwt_segments() {
        // entire data is not valid UTF-8
        let mut data = "Some utf8 data ".as_bytes().to_vec();
        data.push(0x91);
        data.push(0x92);
        data.extend(" with some non-utf8 data".as_bytes());
        data.push(0x93);

        let data_result = JwtHelper::<JwtTestBody>::from(&data);
        assert!(data_result.is_err());
        assert_eq!(
            data_result.err().unwrap().to_string(),
            "JWT data is not valid UTF-8: Some utf8 data \\x91\\x92 with some non-utf8 data\\x93"
                .to_string()
        );

        // valid components
        let private_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let (header, body, signature) = crate::test_helpers::generate_base64_encoded_jwt_components(
            &PKey::from_rsa(private_key).unwrap(),
        );

        // header is not valid UTF-8
        let mut invalid_header = "header".as_bytes().to_vec();
        invalid_header.push(0x91);
        let invalid_header =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&invalid_header);

        let data = format!("{}.{}.{}", invalid_header, body, signature);

        let header_result = JwtHelper::<JwtTestBody>::from(data.as_bytes());
        assert!(header_result.is_err());
        assert_eq!(
            header_result.err().unwrap().to_string(),
            "JWT header is not valid UTF-8: header\\x91".to_string()
        );

        // body is not valid UTF-8
        let mut invalid_body = "body".as_bytes().to_vec();
        invalid_body.push(0x91);
        let invalid_body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&invalid_body);

        let data = format!("{}.{}.{}", header, invalid_body, signature);

        let body_result = JwtHelper::<JwtTestBody>::from(data.as_bytes());
        assert!(body_result.is_err());
        assert_eq!(
            body_result.err().unwrap().to_string(),
            "JWT body is not valid UTF-8: body\\x91".to_string()
        );
    }
}
