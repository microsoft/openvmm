// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `KEY_RELEASE_REQUEST` request type that supports preparing
//! runtime claims, which is a part of the request, and parsing the response, which
//! can be either in JSON or JSON web token (JWT) format defined by Azure Key Vault (AKV).

use crate::igvm_attest::Error as CommonError;
use crate::igvm_attest::parse_response_payload;
use crate::jwt::JwtError;
use crate::jwt::JwtHelper;
use openhcl_attestation_protocol::igvm_attest::akv;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum KeyReleaseError {
    #[error("the response payload size is too small to parse")]
    PayloadSizeTooSmall,
    #[error("failed to parse AKV JWT (API version > 7.2)")]
    ParseAkvJwt(#[source] JwtError),
    #[error("error occurs during AKV JWT signature verification")]
    VerifyAkvJwtSignature(#[source] JwtError),
    #[error("failed to verify AKV JWT signature")]
    VerifyAkvJwtSignatureFailed,
    #[error("failed to get wrapped key from AKV JWT body")]
    GetWrappedKeyFromAkvJwtBody(#[source] serde_json::Error),
    #[error("wrapped key in AKV JWT body is not UTF-8")]
    InvalidWrappedKeyUtf8(#[source] std::str::Utf8Error),
    #[error("error in parsing response header")]
    ParseHeader(#[source] CommonError),
}

/// Parsed result of a `KEY_RELEASE_REQUEST` response.
#[derive(Debug)]
pub(crate) struct KeyReleaseResponse {
    /// The raw RSA-AES-wrapped key blob.
    pub wrapped_key: Vec<u8>,
    /// Whether the IGVM Agent signaled that AKV used the SHA-384 variant of the
    /// composite RSA+AES key-wrap scheme (`RSA_AES_KEY_WRAP_384`). When `false`,
    /// the default scheme is used (inner RSA-OAEP using SHA-1).
    pub rsa_aes_key_wrap_384_used: bool,
    /// Unverified host-provided SHA-256 context hash, available only with V3.
    pub key_release_context_hash: Option<[u8; 32]>,
}

/// Parse a `KEY_RELEASE_REQUEST` response and return a raw wrapped key blob along
/// with the IGVM Agent signal indicating which key-wrap scheme was used.
///
/// Returns `Ok(KeyReleaseResponse)` on successfully extracting a wrapped key blob
/// from `response`, otherwise return an error.
#[cfg(test)]
pub fn parse_response(
    response: &[u8],
    rsa_modulus_size: usize,
) -> Result<KeyReleaseResponse, KeyReleaseError> {
    parse_response_with_context(response, rsa_modulus_size, None)
}

/// Validate an existing binding before interpreting the service payload. A
/// malformed payload must not hide a context mismatch and enable HW fallback.
pub(crate) fn parse_response_with_context(
    response: &[u8],
    rsa_modulus_size: usize,
    expected_context: Option<[u8; 32]>,
) -> Result<KeyReleaseResponse, KeyReleaseError> {
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestKeyReleaseResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseExtensions;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseRequestType;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
    use openhcl_attestation_protocol::igvm_attest::get::KEY_RELEASE_RESPONSE_BUFFER_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::decode_key_release_context_hash;
    use zerocopy::FromBytes;

    // Minimum acceptable payload would look like {"ciphertext":"base64URL wrapped key"}
    const AES_IC_SIZE: usize = 8;
    const CIPHER_TEXT_KEY: &str = r#"{"ciphertext":""}"#;

    let parsed = parse_response_payload::<IgvmAttestResponseExtensions>(
        response,
        IgvmAttestResponseRequestType::KeyRelease,
        KEY_RELEASE_RESPONSE_BUFFER_SIZE,
    )
    .map_err(KeyReleaseError::ParseHeader)?;

    let key_release_context_hash = parsed
        .extensions
        .key_release_context_hash
        .as_deref()
        .map(decode_key_release_context_hash)
        .transpose()
        .map_err(|err| KeyReleaseError::ParseHeader(CommonError::InvalidContextHash(err)))?;
    if expected_context.is_some() && expected_context != key_release_context_hash {
        return Err(KeyReleaseError::ParseHeader(
            CommonError::ResponseContextMismatch,
        ));
    }

    // The `rsa_aes_key_wrap_384_used` signal is only present in the version 2+
    // response header. When absent, fall back to the default (SHA-1) scheme.
    let rsa_aes_key_wrap_384_used = match parsed.header.version {
        IgvmAttestResponseVersion::VERSION_2 | IgvmAttestResponseVersion::VERSION_3 => {
            let full_header = IgvmAttestKeyReleaseResponseHeader::read_from_prefix(response)
                .map_err(|_| {
                    KeyReleaseError::ParseHeader(CommonError::ResponseHeaderInvalidFormat {
                        response_size: response.len(),
                    })
                })?
                .0; // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
            let igvm_signal = full_header.error_info.igvm_signal;
            let rsa_aes_key_wrap_384_used = igvm_signal.rsa_aes_key_wrap_384_used();

            // Record the IGVM Agent response signals that indicate whether the IGVM Agent
            // requested the corresponding actions.
            tracing::info!(
                corim_endorsement_requested = igvm_signal.corim_endorsement_requested(),
                rsa_aes_key_wrap_384_used,
                "IGVM Agent attestation response signals"
            );

            rsa_aes_key_wrap_384_used
        }
        _ => false,
    };

    let payload = parsed.payload.as_bytes();
    let wrapped_key_size = rsa_modulus_size + rsa_modulus_size + AES_IC_SIZE;
    let wrapped_key_base64_url_size = wrapped_key_size / 3 * 4;
    let minimum_payload_size = CIPHER_TEXT_KEY.len() + wrapped_key_base64_url_size - 1;

    if payload.len() < minimum_payload_size {
        Err(KeyReleaseError::PayloadSizeTooSmall)?
    }
    let wrapped_key = match serde_json::from_str::<akv::AkvKeyReleaseKeyBlob>(&parsed.payload) {
        Ok(blob) => {
            // JSON format (API version 7.2)
            blob.ciphertext
        }
        Err(_) => {
            // JWT format (API version > 7.2)
            let result = JwtHelper::<akv::AkvKeyReleaseJwtBody>::from(payload)
                .map_err(KeyReleaseError::ParseAkvJwt)?;

            // Validate the JWT signature (if exist)
            if !result.jwt.signature.is_empty() {
                if !result
                    .verify_signature()
                    .map_err(KeyReleaseError::VerifyAkvJwtSignature)?
                {
                    Err(KeyReleaseError::VerifyAkvJwtSignatureFailed)?
                }
            }
            get_wrapped_key_blob(result)?
        }
    };

    Ok(KeyReleaseResponse {
        wrapped_key,
        rsa_aes_key_wrap_384_used,
        key_release_context_hash,
    })
}

fn get_wrapped_key_blob(
    jwt: JwtHelper<akv::AkvKeyReleaseJwtBody>,
) -> Result<Vec<u8>, KeyReleaseError> {
    let key_hsm = jwt.jwt.body.response.key.key.key_hsm;
    let key_hsm = std::str::from_utf8(&key_hsm).map_err(KeyReleaseError::InvalidWrappedKeyUtf8)?;
    let key_hsm: akv::AkvKeyReleaseKeyBlob =
        serde_json::from_str(key_hsm).map_err(KeyReleaseError::GetWrappedKeyFromAkvJwtBody)?;

    Ok(key_hsm.ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_helpers::CIPHERTEXT;
    use crypto::rsa::RsaKeyPair;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseRequestType;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmSignal;
    use test_with_tracing::test;

    #[test]
    fn parse_json_all_versions_preserves_key_wrap_signal_and_context() {
        use crate::igvm_attest::tests::frame_response;
        use crate::igvm_attest::tests::v3_response;
        use base64::Engine;

        let wrapped_key = vec![42; 520];
        let payload = serde_json::json!({
            "ciphertext": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&wrapped_key)
        })
        .to_string();
        for version in [
            IgvmAttestResponseVersion::VERSION_1,
            IgvmAttestResponseVersion::VERSION_2,
            IgvmAttestResponseVersion::VERSION_3,
        ] {
            for sha384 in [false, true] {
                for context_hash in [None, Some([7; 32])] {
                    let error_info = IgvmErrorInfo {
                        igvm_signal: IgvmSignal::new()
                            .with_rsa_aes_key_wrap_384_used(sha384)
                            .with_corim_endorsement_requested(true),
                        ..Default::default()
                    };
                    let response = if version == IgvmAttestResponseVersion::VERSION_3 {
                        v3_response(
                            IgvmAttestResponseRequestType::KeyRelease,
                            &payload,
                            context_hash,
                            error_info,
                        )
                    } else {
                        frame_response(version, payload.as_bytes(), error_info)
                    };
                    let parsed = parse_response(&response, 256).unwrap();
                    assert_eq!(parsed.wrapped_key, wrapped_key);
                    assert_eq!(
                        parsed.rsa_aes_key_wrap_384_used,
                        sha384 && version != IgvmAttestResponseVersion::VERSION_1
                    );
                    assert_eq!(
                        parsed.key_release_context_hash,
                        if version == IgvmAttestResponseVersion::VERSION_3 {
                            context_hash
                        } else {
                            None
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn context_binding_is_checked_before_invalid_service_payload() {
        use crate::igvm_attest::tests::frame_response;
        use crate::igvm_attest::tests::v3_response;

        for context_hash in [None, Some([2; 32])] {
            let response = v3_response(
                IgvmAttestResponseRequestType::KeyRelease,
                "x",
                context_hash,
                IgvmErrorInfo::default(),
            );
            assert!(matches!(
                parse_response_with_context(&response, 256, Some([1; 32])),
                Err(KeyReleaseError::ParseHeader(
                    CommonError::ResponseContextMismatch
                ))
            ));
        }
        for version in [
            IgvmAttestResponseVersion::VERSION_1,
            IgvmAttestResponseVersion::VERSION_2,
        ] {
            let response = frame_response(version, b"x", IgvmErrorInfo::default());
            assert!(matches!(
                parse_response_with_context(&response, 256, Some([1; 32])),
                Err(KeyReleaseError::ParseHeader(
                    CommonError::ResponseContextMismatch
                ))
            ));
        }
        let response = v3_response(
            IgvmAttestResponseRequestType::KeyRelease,
            "x",
            Some([1; 32]),
            IgvmErrorInfo::default(),
        );
        assert!(matches!(
            parse_response_with_context(&response, 256, Some([1; 32])),
            Err(KeyReleaseError::PayloadSizeTooSmall)
        ));
    }

    #[test]
    fn invalid_context_metadata_is_rejected_before_service_payload() {
        use crate::igvm_attest::tests::frame_response;

        for extensions in [
            r#"{"key_release_context_hash":null}"#,
            r#"{"key_release_context_hash":42}"#,
            r#"{"key_release_context_hash":false}"#,
            r#"{"key_release_context_hash":[]}"#,
            r#"{"key_release_context_hash":{}}"#,
            r#"{"key_release_context_hash":""}"#,
            r#"{"key_release_context_hash":"not hex"}"#,
            r#"{"key_release_context_hash":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","key_release_context_hash":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"}"#,
        ] {
            let envelope = format!(
                r#"{{"schema_version":1,"request_type":"key_release","payload":"x","extensions":{extensions}}}"#
            );
            let response = frame_response(
                IgvmAttestResponseVersion::VERSION_3,
                envelope.as_bytes(),
                IgvmErrorInfo::default(),
            );
            for expected_context in [None, Some([1; 32])] {
                assert!(
                    matches!(
                        parse_response_with_context(&response, 256, expected_context),
                        Err(KeyReleaseError::ParseHeader(
                            CommonError::InvalidResponseEnvelope(_)
                                | CommonError::InvalidContextHash(_)
                        ))
                    ),
                    "accepted {extensions}"
                );
            }
        }
    }

    #[test]
    fn v3_jwt_still_verifies_signature() {
        use crate::igvm_attest::tests::v3_response;

        let rsa_key = RsaKeyPair::generate(2048).unwrap();
        let (header, body, signature) =
            crate::test_helpers::generate_base64_encoded_jwt_components(&rsa_key);
        let jwt = format!("{header}.{body}.{signature}");
        let response = v3_response(
            IgvmAttestResponseRequestType::KeyRelease,
            &jwt,
            Some([1; 32]),
            IgvmErrorInfo::default(),
        );
        let parsed = parse_response(&response, 256).unwrap();
        assert_eq!(parsed.wrapped_key, CIPHERTEXT.as_bytes());
        assert_eq!(parsed.key_release_context_hash, Some([1; 32]));

        let changed = if signature.starts_with('A') { "B" } else { "A" };
        let jwt = format!("{header}.{body}.{changed}{}", &signature[1..]);
        let response = v3_response(
            IgvmAttestResponseRequestType::KeyRelease,
            &jwt,
            None,
            IgvmErrorInfo::default(),
        );
        assert!(matches!(
            parse_response(&response, 256),
            Err(KeyReleaseError::VerifyAkvJwtSignatureFailed
                | KeyReleaseError::VerifyAkvJwtSignature(_))
        ));
    }

    #[test]
    fn v3_does_not_recursively_unwrap_payload() {
        use crate::igvm_attest::tests::v3_response;

        let payload = serde_json::to_string(&akv::AkvKeyReleaseKeyBlob {
            ciphertext: vec![42; 520],
        })
        .unwrap();
        let nested = serde_json::json!({
            "schema_version": 1,
            "request_type": "key_release",
            "payload": payload,
            "extensions": {}
        })
        .to_string();
        let response = v3_response(
            IgvmAttestResponseRequestType::KeyRelease,
            &nested,
            None,
            IgvmErrorInfo::default(),
        );
        assert!(parse_response(&response, 256).is_err());
    }

    #[test]
    fn v3_errors_preserve_attestation_pattern_and_signals() {
        use crate::igvm_attest::tests::frame_response;

        for version in [
            IgvmAttestResponseVersion::VERSION_2,
            IgvmAttestResponseVersion::VERSION_3,
        ] {
            for retry in [false, true] {
                for skip in [false, true] {
                    let response = frame_response(
                        version,
                        b"",
                        IgvmErrorInfo {
                            error_code: 1103,
                            http_status_code: 403,
                            igvm_signal: IgvmSignal::new()
                                .with_retry(retry)
                                .with_skip_hw_unsealing(skip),
                            ..Default::default()
                        },
                    );
                    let Err(KeyReleaseError::ParseHeader(CommonError::Attestation {
                        igvm_error_code,
                        http_status_code,
                        retry_signal,
                        skip_hw_unsealing_signal,
                    })) = parse_response(&response, 256)
                    else {
                        panic!("expected an attestation error");
                    };
                    assert_eq!(igvm_error_code, 1103);
                    assert_eq!(http_status_code, 403);
                    assert_eq!(retry_signal, retry);
                    assert_eq!(skip_hw_unsealing_signal, skip);
                }
            }
        }
    }

    #[test]
    fn get_wrapped_key_from_jwt() {
        let rsa_key = RsaKeyPair::generate(2048).unwrap();

        let (header, body, signature) =
            crate::test_helpers::generate_base64_encoded_jwt_components(&rsa_key);

        let jwt = format!("{}.{}.{}", header, body, signature);
        let jwt = JwtHelper::<akv::AkvKeyReleaseJwtBody>::from(jwt.as_bytes()).unwrap();

        let wrapped_key = get_wrapped_key_blob(jwt).unwrap();
        assert_eq!(wrapped_key, CIPHERTEXT.as_bytes());
    }

    #[test]
    fn fail_to_parse_empty_response() {
        let response = parse_response(&[], 256);
        assert!(response.is_err());
        assert_eq!(
            response.unwrap_err().to_string(),
            KeyReleaseError::ParseHeader(CommonError::ResponseSizeTooSmall { response_size: 0 })
                .to_string()
        );
    }
}
