// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `KEY_RELEASE_REQUEST` request type that supports preparing
//! runtime claims, which is a part of the request, and parsing the response, which
//! can be either in JSON or JSON web token (JWT) format defined by Azure Key Vault (AKV).

use base64::Engine;
use openhcl_attestation_protocol::igvm_attest::akv;
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::rsa::Padding;
use openssl::sign::Verifier;
use openssl::x509::X509VerifyResult;
use openssl::x509::X509;
use thiserror::Error;

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub(crate) enum KeyReleaseError {
    #[error("the response size is too small to parse")]
    ResponseSizeTooSmall,
    #[error("failed to parse AKV JWT (API version > 7.2)")]
    ParseAkvJwt(#[source] AkvKeyReleaseJwtError),
    #[error("error occurs during AKV JWT signature verification")]
    VerifyAkvJwtSignature(#[source] AkvKeyReleaseJwtError),
    #[error("failed to verify AKV JWT signature")]
    VerifyAkvJwtSignatureFailed,
    #[error("failed to get wrapped key from AKV JWT body")]
    GetWrappedKeyFromAkvJwtBody(#[source] AkvKeyReleaseJwtError),
}

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub(crate) enum AkvKeyReleaseJwtError {
    #[error("invalid JWT format, data: {0}")]
    InvalidJwtFormat(String),
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
    #[error("failed to deserialize `key_hsm` into JSON")]
    KeyHsmBlobToJson(#[source] serde_json::Error),
}

#[allow(missing_docs)] // self-explanatory fields
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
    #[error("Unsupported signing algorithm {0:?}")]
    UnsupportedSigningAlgorithm(String),
}

#[allow(missing_docs)] // self-explanatory fields
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

/// Hold the parsed content of a JWT returned by AKV.
#[derive(Debug)]
struct AkvKeyReleaseJwt {
    header: akv::AkvKeyReleaseJwtHeader,
    body: akv::AkvKeyReleaseJwtBody,
    signature: Vec<u8>,
}

/// Helper struct for parsing and validating the JWT returned by AKV.
struct AkvKeyReleaseJwtHelper {
    jwt: AkvKeyReleaseJwt,
    // Raw bytes of `header.body` that is used to generate
    // the signature
    payload: String,
}

/// Parse a `KEY_RELEASE_REQUEST` response and return a raw wrapped key blob.
///
/// Returns `Ok(Vec<u8>)` on successfully extracting a wrapped key blob from `response`,
/// otherwise return an error.
pub fn parse_response(
    response: &[u8],
    rsa_modulus_size: usize,
) -> Result<Vec<u8>, KeyReleaseError> {
    // Minimum acceptable payload would look like {"ciphertext":"base64URL wrapped key"}
    const AES_IC_SIZE: usize = 8;
    const CIPHER_TEXT_KEY: &str = r#"{"ciphertext":""}"#;
    const HEADER_SIZE: usize = size_of::<
        openhcl_attestation_protocol::igvm_attest::get::IgvmAttestKeyReleaseResponseHeader,
    >();

    let wrapped_key_size = rsa_modulus_size + rsa_modulus_size + AES_IC_SIZE;
    let wrapped_key_base64_url_size = wrapped_key_size / 3 * 4;
    let minimum_response_size =
        CIPHER_TEXT_KEY.len() + wrapped_key_base64_url_size - 1 + HEADER_SIZE;

    if response.is_empty() || response.len() < minimum_response_size {
        Err(KeyReleaseError::ResponseSizeTooSmall)?
    }

    let payload = &response[HEADER_SIZE..];
    let data_utf8 = String::from_utf8_lossy(payload);
    let wrapped_key = match serde_json::from_str::<akv::AkvKeyReleaseKeyBlob>(&data_utf8) {
        Ok(blob) => {
            // JSON format (API version 7.2)
            blob.ciphertext
        }
        Err(_) => {
            // JWT format (API version > 7.2)
            let result =
                AkvKeyReleaseJwtHelper::from(payload).map_err(KeyReleaseError::ParseAkvJwt)?;

            // Validate the JWT signature (if exist)
            if !result.jwt.signature.is_empty() {
                if !result
                    .verify_signature()
                    .map_err(KeyReleaseError::VerifyAkvJwtSignature)?
                {
                    Err(KeyReleaseError::VerifyAkvJwtSignatureFailed)?
                }
            }
            result
                .get_wrapped_key_blob()
                .map_err(KeyReleaseError::GetWrappedKeyFromAkvJwtBody)?
        }
    };

    Ok(wrapped_key)
}

impl AkvKeyReleaseJwtHelper {
    /// Parse the given JWT
    fn from(data: &[u8]) -> Result<Self, AkvKeyReleaseJwtError> {
        // A JWT looks like:
        // Base64URL(Header).Base64URL(Body).Base64URL(Signature)
        // Header and Body are JSON payloads

        let utf8 = String::from_utf8_lossy(data);

        let [header, body, signature]: [&str; 3] = utf8
            .split('.')
            .collect::<Vec<&str>>()
            .try_into()
            .map_err(|_| AkvKeyReleaseJwtError::InvalidJwtFormat(utf8.to_string()))?;

        let (signature, payload) = if !signature.is_empty() {
            let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(signature)
                .map_err(AkvKeyReleaseJwtError::DecodeBase64UrlJwtSignature)?;

            (signature, [header, ".", body].concat())
        } else {
            (vec![], "".to_string())
        };

        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header)
            .map_err(AkvKeyReleaseJwtError::DecodeBase64UrlJwtHeader)?;
        let header = String::from_utf8_lossy(&header);
        let header: akv::AkvKeyReleaseJwtHeader =
            serde_json::from_str(&header).map_err(AkvKeyReleaseJwtError::JwtHeaderToJson)?;

        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(AkvKeyReleaseJwtError::DecodeBase64UrlJwtBody)?;
        let body = String::from_utf8_lossy(&body);
        let body: akv::AkvKeyReleaseJwtBody =
            serde_json::from_str(&body).map_err(AkvKeyReleaseJwtError::JwtBodyToJson)?;

        Ok(Self {
            jwt: AkvKeyReleaseJwt {
                header,
                body,
                signature,
            },
            payload,
        })
    }

    /// Verify the JWT signature
    fn verify_signature(&self) -> Result<bool, AkvKeyReleaseJwtError> {
        let alg = &self.jwt.header.alg;
        let x5c = &self.jwt.header.x5c;
        let cert_chain: Vec<X509> = x5c
            .iter()
            .map(|encoded_cert| {
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(encoded_cert)
                    .map_err(AkvKeyReleaseJwtError::DecodeBase64JwtX509Certificate)?;
                X509::from_der(&raw).map_err(AkvKeyReleaseJwtError::RawBytesToX509)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let pkey = validate_cert_chain(&cert_chain)?;

        let result =
            verify_jwt_signature(alg, &pkey, self.payload.as_bytes(), &self.jwt.signature)?;

        Ok(result)
    }

    fn get_wrapped_key_blob(&self) -> Result<Vec<u8>, AkvKeyReleaseJwtError> {
        let key_hsm = &self.jwt.body.response.key.key.key_hsm;
        let key_hsm = String::from_utf8_lossy(key_hsm);
        let key_hsm: akv::AkvKeyReleaseKeyBlob =
            serde_json::from_str(&key_hsm).map_err(AkvKeyReleaseJwtError::KeyHsmBlobToJson)?;

        Ok(key_hsm.ciphertext)
    }
}

/// Helper function for JWT signature validation using OpenSSL.
fn verify_jwt_signature(
    alg: &str,
    pkey: &PKey<openssl::pkey::Public>,
    payload: &[u8],
    signature: &[u8],
) -> Result<bool, JwtSignatureVerificationError> {
    let result = match alg {
        "RS256" => {
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
        alg => Err(JwtSignatureVerificationError::UnsupportedSigningAlgorithm(
            alg.to_string(),
        ))?,
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

    const HEADER: &str = r#"{"typ":"JWT","alg":"RS256"}"#;
    const KEY_HSM: &str = "http://example.com";
    // Example signature from https://www.rfc-editor.org/rfc/rfc7519
    const BASES64_SIGNATURE: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

    #[test]
    fn jwt_from_bytes() {
        let base64_header =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(HEADER.as_bytes());

        let base64_key_hsm =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(KEY_HSM.as_bytes());
        let body = akv::AkvKeyReleaseJwtBody {
            response: akv::AkvKeyReleaseResponse {
                key: akv::AkvKeyReleaseKeyObject {
                    key: akv::AkvJwk {
                        key_hsm: base64_key_hsm.as_bytes().to_vec(),
                    },
                },
            },
        };
        let base64_body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&body).unwrap().as_bytes());

        let jwt = format!("{}.{}.{}", base64_header, base64_body, BASES64_SIGNATURE);

        let jwt = AkvKeyReleaseJwtHelper::from(jwt.as_bytes()).unwrap();

        assert_eq!(jwt.jwt.header.alg, "RS256");
        assert_eq!(
            jwt.jwt.body.response.key.key.key_hsm,
            base64_key_hsm.as_bytes()
        );
    }

    #[test]
    fn jwt_from_bytes_with_empty_signature() {
        let base64_header =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(HEADER.as_bytes());

        let base64_key_hsm =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(KEY_HSM.as_bytes());
        let body = akv::AkvKeyReleaseJwtBody {
            response: akv::AkvKeyReleaseResponse {
                key: akv::AkvKeyReleaseKeyObject {
                    key: akv::AkvJwk {
                        key_hsm: base64_key_hsm.as_bytes().to_vec(),
                    },
                },
            },
        };
        let base64_body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&body).unwrap().as_bytes());

        let jwt = format!("{}.{}.{}", base64_header, base64_body, "");
        let jwt = AkvKeyReleaseJwtHelper::from(jwt.as_bytes()).unwrap();

        assert_eq!(jwt.jwt.signature, Vec::<u8>::from([]));
    }

    #[test]
    fn fail_to_verify_non_rs256_signature() {
        let rsa_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let pem = rsa_key.public_key_to_pem().unwrap();
        let public = PKey::public_key_from_pem(&pem).unwrap();

        let outcome = verify_jwt_signature("HS256", &public, &[], &[]);

        assert!(outcome.is_err());
        assert_eq!(
            outcome.unwrap_err().to_string(),
            "Unsupported signing algorithm \"HS256\"".to_string()
        );
    }

    #[test]
    fn fail_to_verify_inconsistent_rs256_signature() {
        let dsa_key = openssl::dsa::Dsa::generate(2048).unwrap();
        let pem = dsa_key.public_key_to_pem().unwrap();
        let public = PKey::public_key_from_pem(&pem).unwrap();

        let outcome = verify_jwt_signature("RS256", &public, &[], &[]);

        assert!(outcome.is_err());
        assert_eq!(
            outcome.unwrap_err().to_string(),
            "invalid key type Id(116), expected Id(6)".to_string()
        );
    }

    #[test]
    fn fail_to_parse_empty_response() {
        let response = parse_response(&[], 256);
        assert!(response.is_err());
        assert_eq!(
            response.unwrap_err().to_string(),
            KeyReleaseError::ResponseSizeTooSmall.to_string()
        );
    }
}
