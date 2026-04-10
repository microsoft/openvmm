// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of secure key release (SKR) scheme for stateful CVM to obtain VMGS
//! encryption keys.

use crate::IgvmAttestRequestHelper;
use crate::igvm_attest;
use crypto::rsa::RsaKeyPair;
use cvm_tracing::CVM_ALLOWED;
use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::EventLogId;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_REQUEST_CURRENT_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
use openhcl_attestation_protocol::igvm_attest::get::KEY_RELEASE_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::igvm_attest::get::WRAPPED_KEY_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_attestation_protocol::vmgs::AGENT_DATA_MAX_SIZE;
use tee_call::TeeCall;
use thiserror::Error;
use vmgs::Vmgs;

#[derive(Debug, Error)]
pub(crate) enum RequestVmgsEncryptionKeysError {
    #[error("failed to generate an RSA transfer key")]
    GenerateTransferKey(#[source] crypto::rsa::RsaError),
    #[error("failed to get a TEE attestation report")]
    GetAttestationReport(#[source] tee_call::Error),
    #[error("failed to create an IgvmAttest WRAPPED_KEY request")]
    CreateIgvmAttestWrappedKeyRequest(#[source] igvm_attest::Error),
    #[error("failed to make an IgvmAttest WRAPPED_KEY GET request")]
    SendIgvmAttestWrappedKeyRequest(#[source] guest_emulation_transport::error::IgvmAttestError),
    #[error("failed to parse the IgvmAttest WRAPPED_KEY response")]
    ParseIgvmAttestWrappedKeyResponse(#[source] igvm_attest::wrapped_key::WrappedKeyError),
    #[error(
        "failed to get a valid IgvmAttest WRAPPED_KEY response that is required because agent data from VMGS is empty"
    )]
    RequiredButInvalidIgvmAttestWrappedKeyResponse,
    #[error("wrapped key from WRAPPED_KEY response is empty")]
    EmptyWrappedKey,
    #[error(
        "key reference size {key_reference_size} from the WRAPPED_KEY response was larger than expected {expected_size}"
    )]
    InvalidKeyReferenceSize {
        key_reference_size: usize,
        expected_size: usize,
    },
    #[error("key reference from the WRAPPED_KEY response is empty")]
    EmptyKeyReference,
    #[error("failed to create an IgvmAttest KEY_RELEASE request")]
    CreateIgvmAttestKeyReleaseRequest(#[source] igvm_attest::Error),
    #[error("failed to make an IgvmAttest KEY_RELEASE GET request")]
    SendIgvmAttestKeyReleaseRequest(#[source] guest_emulation_transport::error::IgvmAttestError),
    #[error("failed to parse the IgvmAttest KEY_RELEASE response")]
    ParseIgvmAttestKeyReleaseResponse(#[source] igvm_attest::key_release::KeyReleaseError),
    #[error("PKCS11 RSA AES key unwrap failed")]
    Pkcs11RsaAesKeyUnwrap(#[source] Pkcs11RsaAesKeyUnwrapError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Pkcs11RsaAesKeyUnwrapError {
    #[error("expected wrapped AES key blob to be {0} bytes, but found {1} bytes")]
    UndersizedWrappedAesKey(usize, usize),
    #[error("wrapped RSA key blob cannot be empty")]
    EmptyWrappedRsaKey,
    #[error("RSA unwrap failed")]
    RsaUnwrap(#[source] crypto::rsa::RsaError),
    #[error("AES unwrap failed")]
    AesUnwrap(#[source] crypto::aes_key_wrap::AesKeyWrapError),
    #[error("failed to parse PKCS#8 DER as RSA key")]
    ParsePkcs8Der(#[source] crypto::rsa::RsaError),
}

/// PKCS#11 RSA AES key unwrap implementation.
fn pkcs11_rsa_aes_key_unwrap(
    unwrapping_rsa_key: &RsaKeyPair,
    wrapped_key_blob: &[u8],
) -> Result<RsaKeyPair, Pkcs11RsaAesKeyUnwrapError> {
    use crypto::rsa::OaepHashAlgorithm;

    let modulus_size = unwrapping_rsa_key.modulus_size();

    let (wrapped_aes_key, wrapped_rsa_key) = wrapped_key_blob
        .split_at_checked(modulus_size)
        .ok_or_else(|| {
            Pkcs11RsaAesKeyUnwrapError::UndersizedWrappedAesKey(
                modulus_size,
                wrapped_key_blob.len(),
            )
        })?;

    if wrapped_rsa_key.is_empty() {
        return Err(Pkcs11RsaAesKeyUnwrapError::EmptyWrappedRsaKey);
    }

    let unwrapped_aes_key = unwrapping_rsa_key
        .oaep_decrypt(wrapped_aes_key, OaepHashAlgorithm::Sha1)
        .map_err(Pkcs11RsaAesKeyUnwrapError::RsaUnwrap)?;
    let unwrapped_rsa_key = crypto::aes_key_wrap::AesKeyWrap::new(&unwrapped_aes_key)
        .and_then(|kw| kw.unwrapper()?.unwrap(wrapped_rsa_key))
        .map_err(Pkcs11RsaAesKeyUnwrapError::AesUnwrap)?;
    let unwrapped_rsa_key = RsaKeyPair::from_pkcs8_der(&unwrapped_rsa_key)
        .map_err(Pkcs11RsaAesKeyUnwrapError::ParsePkcs8Der)?;

    Ok(unwrapped_rsa_key)
}

/// The return values of [`make_igvm_attest_requests`].
struct WrappedKeyVmgsEncryptionKeys {
    /// RSA-AES-wrapped key blob. This field is always present (required).
    rsa_aes_wrapped_key: Vec<u8>,
    /// Optional wrapped DiskEncryptionSettings key blob.
    wrapped_des_key: Option<Vec<u8>>,
}

/// The return values of [`request_vmgs_encryption_keys`].
#[derive(Default)]
pub struct VmgsEncryptionKeys {
    /// Optional ingress RSA key-encryption key.
    /// `None` indicate secure key release failed.
    pub ingress_rsa_kek: Option<RsaKeyPair>,
    /// Optional DiskEncryptionSettings key used by key rotation.
    pub wrapped_des_key: Option<Vec<u8>>,
    /// Optional TCB version used by hardware key sealing.
    pub tcb_version: Option<u64>,
}

/// Request the VMGS encryption keys via host call-outs with optional retry logic.
pub async fn request_vmgs_encryption_keys(
    get: &GuestEmulationTransportClient,
    tee_call: &dyn TeeCall,
    vmgs: &Vmgs,
    attestation_vm_config: &AttestationVmConfig,
    agent_data: &mut [u8; AGENT_DATA_MAX_SIZE],
) -> Result<VmgsEncryptionKeys, (RequestVmgsEncryptionKeysError, bool)> {
    const TRANSFER_RSA_KEY_BITS: u32 = 2048;

    // Generate an ephemeral transfer key
    let transfer_key = RsaKeyPair::generate(TRANSFER_RSA_KEY_BITS).map_err(|e| {
        (
            RequestVmgsEncryptionKeysError::GenerateTransferKey(e),
            false,
        )
    })?;

    let exponent = transfer_key.public_exponent();
    let modulus = transfer_key.modulus();
    let host_time = get.host_time().await.to_jiff().timestamp().as_second();

    let mut igvm_attest_request_helper = IgvmAttestRequestHelper::prepare_key_release_request(
        tee_call.tee_type(),
        &exponent,
        &modulus,
        host_time,
        attestation_vm_config,
    );

    let vmgs_encrypted = vmgs.encrypted();

    tracing::info!(CVM_ALLOWED, "attempt to get VMGS key-encryption key");

    // Get attestation report each time this function is called. Failures here are fatal.
    let result = tee_call
        .get_attestation_report(igvm_attest_request_helper.get_runtime_claims_hash())
        .map_err(|e| {
            (
                RequestVmgsEncryptionKeysError::GetAttestationReport(e),
                false,
            )
        })?;

    // Get tenant keys based on attestation results, this might fail.
    match make_igvm_attest_requests(
        get,
        &transfer_key,
        &mut igvm_attest_request_helper,
        &result.report,
        agent_data,
        vmgs_encrypted,
    )
    .await
    {
        Ok(WrappedKeyVmgsEncryptionKeys {
            rsa_aes_wrapped_key,
            wrapped_des_key,
        }) => {
            let ingress_rsa_kek = pkcs11_rsa_aes_key_unwrap(&transfer_key, &rsa_aes_wrapped_key)
                .map_err(|e| (RequestVmgsEncryptionKeysError::Pkcs11RsaAesKeyUnwrap(e), false))?;

            Ok(VmgsEncryptionKeys {
                ingress_rsa_kek: Some(ingress_rsa_kek),
                wrapped_des_key,
                tcb_version: result.tcb_version,
            })
        }
        Err(
            wrapped_key_attest_error @ RequestVmgsEncryptionKeysError::ParseIgvmAttestWrappedKeyResponse(
                igvm_attest::wrapped_key::WrappedKeyError::ParseHeader(
                    igvm_attest::Error::Attestation {
                        igvm_error_code,
                        http_status_code,
                        retry_signal,
                    },
                ),
            ),
        ) => {
            tracing::error!(
                CVM_ALLOWED,
                igvm_error_code = &igvm_error_code,
                igvm_http_status_code = &http_status_code,
                retry_signal = &retry_signal,
                error = &wrapped_key_attest_error as &dyn std::error::Error,
                "VMGS key-encryption failed due to igvm attest error"
            );
            Err((wrapped_key_attest_error, retry_signal))
        }
        Err(
            key_release_attest_error @ RequestVmgsEncryptionKeysError::ParseIgvmAttestKeyReleaseResponse(
                igvm_attest::key_release::KeyReleaseError::ParseHeader(
                    igvm_attest::Error::Attestation {
                        igvm_error_code,
                        http_status_code,
                        retry_signal,
                    },
                ),
            ),
        ) => {
            tracing::error!(
                CVM_ALLOWED,
                igvm_error_code = &igvm_error_code,
                igvm_http_status_code = &http_status_code,
                retry_signal = &retry_signal,
                error = &key_release_attest_error as &dyn std::error::Error,
                "VMGS key-encryption failed due to igvm attest error"
            );
            Err((key_release_attest_error, retry_signal))
        }
        Err(e) => {
            tracing::error!(
                CVM_ALLOWED,
                error = &e as &dyn std::error::Error,
                "VMGS key-encryption key request failed due to error",
            );
            Err((e, true))
        }
    }
}

/// Make the `IGVM_ATTEST` request to GET.
async fn make_igvm_attest_requests(
    get: &GuestEmulationTransportClient,
    transfer_key: &RsaKeyPair,
    igvm_attest_request_helper: &mut IgvmAttestRequestHelper,
    attestation_report: &[u8],
    agent_data: &mut [u8; AGENT_DATA_MAX_SIZE],
    vmgs_encrypted: bool,
) -> Result<WrappedKeyVmgsEncryptionKeys, RequestVmgsEncryptionKeysError> {
    // When VMGS is encrypted, empty `agent_data` from VMGS implies that the data required by the
    // KeyRelease request needs to come from the WrappedKey response.
    let wrapped_key_required = vmgs_encrypted && agent_data.iter().all(|&x| x == 0);

    // Attempt to get wrapped DiskEncryptionSettings key
    igvm_attest_request_helper.set_request_type(IgvmAttestRequestType::WRAPPED_KEY_REQUEST);
    let request = igvm_attest_request_helper
        .create_request(IGVM_ATTEST_REQUEST_CURRENT_VERSION, attestation_report)
        .map_err(RequestVmgsEncryptionKeysError::CreateIgvmAttestWrappedKeyRequest)?;

    let response = match get
        .igvm_attest([].into(), request, WRAPPED_KEY_RESPONSE_BUFFER_SIZE)
        .await
    {
        Ok(response) => response,
        Err(e) => {
            if wrapped_key_required {
                // Notify host if WrappedKey is required for diagnosis.
                get.event_log_fatal(EventLogId::WRAPPED_KEY_REQUIRED_BUT_INVALID)
                    .await;
            }

            return Err(RequestVmgsEncryptionKeysError::SendIgvmAttestWrappedKeyRequest(e));
        }
    };

    let wrapped_des_key = match igvm_attest::wrapped_key::parse_response(&response.response) {
        Ok(parsed_response) => {
            if parsed_response.wrapped_key.is_empty() {
                Err(RequestVmgsEncryptionKeysError::EmptyWrappedKey)?
            }

            // Update the key reference data to the response contents
            if parsed_response.key_reference.is_empty() {
                Err(RequestVmgsEncryptionKeysError::EmptyKeyReference)?
            }

            if parsed_response.key_reference.len() > AGENT_DATA_MAX_SIZE {
                Err(RequestVmgsEncryptionKeysError::InvalidKeyReferenceSize {
                    key_reference_size: parsed_response.key_reference.len(),
                    expected_size: AGENT_DATA_MAX_SIZE,
                })?
            }

            // Make sure rewriting the whole `agent_data` buffer
            let new_agent_data = if parsed_response.key_reference.len() < AGENT_DATA_MAX_SIZE {
                let mut data = parsed_response.key_reference;
                data.resize(AGENT_DATA_MAX_SIZE, 0);
                data
            } else {
                parsed_response.key_reference
            };

            agent_data.copy_from_slice(&new_agent_data[..]);

            Some(parsed_response.wrapped_key)
        }
        Err(
            igvm_attest::wrapped_key::WrappedKeyError::ParseHeader(
                igvm_attest::Error::ResponseSizeTooSmall { .. },
            )
            | igvm_attest::wrapped_key::WrappedKeyError::PayloadSizeTooSmall,
        ) => {
            // The request does not succeed.
            // Return an error if WrappedKey is required, otherwise ignore the error and set the `wrapped_des_key` to None.
            if wrapped_key_required {
                // Notify host if WrappedKey is required for diagnosis.
                get.event_log_fatal(EventLogId::WRAPPED_KEY_REQUIRED_BUT_INVALID)
                    .await;

                return Err(
                    RequestVmgsEncryptionKeysError::RequiredButInvalidIgvmAttestWrappedKeyResponse,
                );
            } else {
                None
            }
        }
        Err(e) => {
            if wrapped_key_required {
                // Notify host if WrappedKey is required for diagnosis.
                get.event_log_fatal(EventLogId::WRAPPED_KEY_REQUIRED_BUT_INVALID)
                    .await;
            }

            return Err(RequestVmgsEncryptionKeysError::ParseIgvmAttestWrappedKeyResponse(e));
        }
    };

    igvm_attest_request_helper.set_request_type(IgvmAttestRequestType::KEY_RELEASE_REQUEST);
    let request = igvm_attest_request_helper
        .create_request(IGVM_ATTEST_REQUEST_CURRENT_VERSION, attestation_report)
        .map_err(RequestVmgsEncryptionKeysError::CreateIgvmAttestKeyReleaseRequest)?;

    // Get tenant keys based on attestation results
    let response = match get
        .igvm_attest(
            agent_data.to_vec(),
            request,
            KEY_RELEASE_RESPONSE_BUFFER_SIZE,
        )
        .await
    {
        Ok(response) => response,
        Err(e) => {
            // Notify host for diagnosis.
            get.event_log_fatal(EventLogId::KEY_NOT_RELEASED).await;

            return Err(RequestVmgsEncryptionKeysError::SendIgvmAttestKeyReleaseRequest(e));
        }
    };

    match igvm_attest::key_release::parse_response(&response.response, transfer_key.modulus_size())
    {
        Ok(rsa_aes_wrapped_key) => Ok(WrappedKeyVmgsEncryptionKeys {
            rsa_aes_wrapped_key,
            wrapped_des_key,
        }),
        Err(e) => {
            // Notify host for diagnosis.
            get.event_log_fatal(EventLogId::KEY_NOT_RELEASED).await;

            Err(RequestVmgsEncryptionKeysError::ParseIgvmAttestKeyReleaseResponse(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::rsa::OaepHashAlgorithm;

    #[test]
    fn fail_to_unwrap_pkcs11_rsa_aes_with_undersized_wrapped_key_blob() {
        let rsa = RsaKeyPair::generate(2048).unwrap();

        // undersized aes key blob
        let wrapped_key_blob = vec![0; 256 - 1];
        let result = pkcs11_rsa_aes_key_unwrap(&rsa, &wrapped_key_blob);
        assert!(matches!(
            result,
            Err(Pkcs11RsaAesKeyUnwrapError::UndersizedWrappedAesKey(
                256, 255
            ))
        ));

        // empty rsa key blob
        let wrapped_key_blob = vec![0; 256];
        let result = pkcs11_rsa_aes_key_unwrap(&rsa, &wrapped_key_blob);
        assert!(matches!(
            result,
            Err(Pkcs11RsaAesKeyUnwrapError::EmptyWrappedRsaKey)
        ));
    }

    #[test]
    fn pkcs11_rsa_aes_key_unwrap_roundtrip() {
        let target_key = RsaKeyPair::generate(2048).unwrap();
        let pkcs8_target_key = target_key.to_pkcs8_der().unwrap();

        let mut wrapping_aes_key = [0u8; 32];
        getrandom::fill(&mut wrapping_aes_key).expect("rng failure");

        let wrapping_rsa_key = RsaKeyPair::generate(2048).unwrap();
        let wrapped_aes_key = wrapping_rsa_key
            .oaep_encrypt(&wrapping_aes_key, OaepHashAlgorithm::Sha1)
            .unwrap();
        let wrapped_target_key = crypto::aes_key_wrap::AesKeyWrap::new(&wrapping_aes_key)
            .unwrap()
            .wrapper()
            .unwrap()
            .wrap(&pkcs8_target_key)
            .unwrap();
        let wrapped_key_blob = [wrapped_aes_key, wrapped_target_key].concat();
        let unwrapped_target_key =
            pkcs11_rsa_aes_key_unwrap(&wrapping_rsa_key, wrapped_key_blob.as_slice()).unwrap();
        assert_eq!(
            unwrapped_target_key.to_private_key_der().unwrap(),
            target_key.to_private_key_der().unwrap()
        );
    }
}
