// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module handles parsing JSON Web Token (JWT) data.

use base64::Engine;
use crypto::rsa::RsaPublicKey;
use crypto::x509::X509Certificate;
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
    RawBytesToX509(#[source] crypto::x509::X509Error),
    #[error("failed to validate certificate chain")]
    CertificateChainValidation(#[from] CertificateChainValidationError),
    #[error("failed to verify JWT signature")]
    JwtSignatureVerification(#[source] JwtSignatureVerificationError),
}

#[derive(Debug, Error)]
pub(crate) enum JwtSignatureVerificationError {
    #[error("RSA signature verification failed")]
    VerifySignature(#[source] crypto::rsa::RsaError),
}

#[derive(Debug, Error)]
pub(crate) enum CertificateChainValidationError {
    #[error("certificate chain is empty")]
    CertChainIsEmpty,
    #[error("failed to get public key from the certificate")]
    GetPublicKeyFromCertificate(#[source] crypto::x509::X509Error),
    #[error("failed to verify the child certificate signature with parent public key")]
    VerifyChildSignatureWithParentPublicKey(#[source] crypto::x509::X509Error),
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
    pub fn cert_chain(&self) -> Result<Vec<X509Certificate>, JwtError> {
        self.jwt
            .header
            .x5c
            .iter()
            .map(|encoded_cert| {
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(encoded_cert)
                    .map_err(JwtError::DecodeBase64JwtX509Certificate)?;
                X509Certificate::from_der(&raw).map_err(JwtError::RawBytesToX509)
            })
            .collect::<Result<Vec<_>, _>>()
    }

    /// Verify the JWT's signature. Ok(true) means a valid signature; Ok(false)
    /// or Err indicate an invalid signature or other error.
    pub fn verify_signature(&self) -> Result<bool, JwtError> {
        let alg = &self.jwt.header.alg;
        let pkey = validate_cert_chain(&self.cert_chain()?)?;
        verify_jwt_signature(alg, &pkey, self.payload.as_bytes(), &self.jwt.signature)
            .map_err(JwtError::JwtSignatureVerification)
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

/// Helper function for JWT signature validation.
fn verify_jwt_signature(
    alg: &JwtAlgorithm,
    pkey: &RsaPublicKey,
    payload: &[u8],
    signature: &[u8],
) -> Result<bool, JwtSignatureVerificationError> {
    match alg {
        JwtAlgorithm::RS256 => pkey
            .pkcs1_verify(payload, signature, crypto::rsa::HashAlgorithm::Sha256)
            .map_err(JwtSignatureVerificationError::VerifySignature),
    }
}

/// Helper function for x509 certificate chain validation.
fn validate_cert_chain(
    cert_chain: &[X509Certificate],
) -> Result<RsaPublicKey, CertificateChainValidationError> {
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
            if !issued {
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
    use crypto::rsa::RsaKeyPair;
    use openhcl_attestation_protocol::igvm_attest::akv;

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
        let rsa_key = RsaKeyPair::generate(2048).unwrap();

        let (header, body, signature) =
            crate::test_helpers::generate_base64_encoded_jwt_components(&rsa_key);

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
        let rsa_key = RsaKeyPair::generate(2048).unwrap();

        let (header, body, _) =
            crate::test_helpers::generate_base64_encoded_jwt_components(&rsa_key);

        let jwt = format!("{}.{}.{}", header, body, "");
        let jwt = JwtHelper::<akv::AkvKeyReleaseJwtBody>::from(jwt.as_bytes()).unwrap();

        assert_eq!(jwt.jwt.signature, Vec::<u8>::from([]));
    }

    #[test]
    fn successfully_verify_jwt_signature() {
        let rsa_key = RsaKeyPair::generate(2048).unwrap();

        let (header, body, signature) =
            crate::test_helpers::generate_base64_encoded_jwt_components(&rsa_key);

        let jwt = format!("{}.{}.{}", header, body, signature);
        let jwt = JwtHelper::<akv::AkvKeyReleaseJwtBody>::from(jwt.as_bytes()).unwrap();

        let verification_succeeded = jwt.verify_signature().unwrap();
        assert!(verification_succeeded);
    }

    #[test]
    fn successfully_verify_jwt_signature_helper_function() {
        let rsa_key = RsaKeyPair::generate(2048).unwrap();

        let payload = "test";
        let signature = rsa_key
            .pkcs1_sign(payload.as_bytes(), crypto::rsa::HashAlgorithm::Sha256)
            .unwrap();

        let cert = crate::test_helpers::generate_x509(&rsa_key);
        let public = cert.public_key().unwrap();

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
    fn fail_to_verify_empty_certificate_chain() {
        let outcome = validate_cert_chain(&[]);

        assert!(outcome.is_err());
        assert!(matches!(
            outcome,
            Err(CertificateChainValidationError::CertChainIsEmpty)
        ));
    }

    #[test]
    fn fail_to_verify_certificate_chain_with_various_signers() {
        let cert_rsa_key = RsaKeyPair::generate(2048).unwrap();
        let intermediate_rsa_key = RsaKeyPair::generate(2048).unwrap();
        let root_rsa_key = RsaKeyPair::generate(2048).unwrap();

        let cert = crate::test_helpers::generate_x509(&cert_rsa_key);
        let intermediate = crate::test_helpers::generate_x509(&intermediate_rsa_key);
        let root = crate::test_helpers::generate_x509(&root_rsa_key);

        let cert_chain = vec![cert, intermediate, root];

        let outcome = validate_cert_chain(&cert_chain);

        assert!(outcome.is_err());
        assert!(matches!(
            outcome,
            Err(CertificateChainValidationError::CertChainSignatureMismatch)
        ));
    }

    #[test]
    fn fail_to_verify_certificate_chain_with_mismatched_subject_and_issuer() {
        let rsa_key = RsaKeyPair::generate(2048).unwrap();

        let cert = crate::test_helpers::generate_x509(&rsa_key);
        let intermediate = crate::test_helpers::generate_x509(&rsa_key);

        // Build root cert with different subject name
        let root = X509Certificate::build_self_signed(
            &rsa_key,
            "US",
            "Washington",
            "Redmond",
            "ACME INC",
            "acme.com",
        )
        .unwrap();

        let cert_chain = vec![cert, intermediate, root];

        let outcome = validate_cert_chain(&cert_chain);

        assert!(outcome.is_err());
        assert!(matches!(
            outcome,
            Err(CertificateChainValidationError::CertChainSubjectIssuerMismatch)
        ));
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
        let rsa_key = RsaKeyPair::generate(2048).unwrap();
        let (header, body, signature) =
            crate::test_helpers::generate_base64_encoded_jwt_components(&rsa_key);

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
