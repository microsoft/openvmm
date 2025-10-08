// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of secure key release (SKR) scheme for stateful CVM to obtain VMGS
//! encryption keys.

use crate::IgvmAttestRequestHelper;
use crate::crypto;
use crate::igvm_attest;
use cvm_tracing::CVM_ALLOWED;
use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::EventLogId;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_REQUEST_CURRENT_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
use openhcl_attestation_protocol::igvm_attest::get::KEY_RELEASE_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::igvm_attest::get::WRAPPED_KEY_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_attestation_protocol::vmgs::AGENT_DATA_MAX_SIZE;
use openssl::pkey::Private;
use openssl::rsa::Rsa;
use pal_async::local::LocalDriver;
use tee_call::TeeCall;
use thiserror::Error;
use vmgs::Vmgs;

#[derive(Debug, Error)]
pub(crate) enum RequestVmgsEncryptionKeysError {
    #[error("failed to generate an RSA transfer key")]
    GenerateTransferKey(#[source] openssl::error::ErrorStack),
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
    Pkcs11RsaAesKeyUnwrap(#[source] crypto::Pkcs11RsaAesKeyUnwrapError),
    #[error("maximum number of attempts reached")]
    MaximumAttemptsReached,
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
    pub ingress_rsa_kek: Option<Rsa<Private>>,
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
    driver: LocalDriver,
) -> Result<VmgsEncryptionKeys, RequestVmgsEncryptionKeysError> {
    const TRANSFER_RSA_KEY_BITS: u32 = 2048;
    const MAXIMUM_RETRY_COUNT: usize = 10;
    const NO_RETRY_COUNT: usize = 1;

    // Generate an ephemeral transfer key
    let transfer_key = Rsa::generate(TRANSFER_RSA_KEY_BITS)
        .map_err(RequestVmgsEncryptionKeysError::GenerateTransferKey)?;

    let exponent = transfer_key.e().to_vec();
    let modulus = transfer_key.n().to_vec();
    let host_time = get_host_epoch_time(get).await;

    let mut igvm_attest_request_helper = IgvmAttestRequestHelper::prepare_key_release_request(
        tee_call.tee_type(),
        &exponent,
        &modulus,
        host_time,
        attestation_vm_config,
    );

    // Retry attestation call-out if necessary (if VMGS encrypted).
    // The IGVm Agent could be down for servicing, or the TDX service VM might not be ready, or a dynamic firmware
    // update could mean that the report was not verifiable.
    let vmgs_encrypted = vmgs.is_encrypted();
    let max_retry = if vmgs_encrypted {
        MAXIMUM_RETRY_COUNT
    } else {
        NO_RETRY_COUNT
    };

    let mut timer = pal_async::timer::PolledTimer::new(&driver);

    for i in 0..max_retry {
        tracing::info!(
            CVM_ALLOWED,
            attempt = i,
            "attempt to get VMGS key-encryption key"
        );

        // Get attestation report on each iteration. Failures here are fatal.
        let result = tee_call
            .get_attestation_report(igvm_attest_request_helper.get_runtime_claims_hash())
            .map_err(RequestVmgsEncryptionKeysError::GetAttestationReport)?;

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
                let ingress_rsa_kek =
                        crypto::pkcs11_rsa_aes_key_unwrap(&transfer_key, &rsa_aes_wrapped_key)
                            .map_err(RequestVmgsEncryptionKeysError::Pkcs11RsaAesKeyUnwrap)?;

                return Ok(VmgsEncryptionKeys {
                    ingress_rsa_kek: Some(ingress_rsa_kek),
                    wrapped_des_key,
                    tcb_version: result.tcb_version,
                });
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
                    retry = i,
                    igvm_error_code = &igvm_error_code,
                    igvm_http_status_code = &http_status_code,
                    retry_signal = &retry_signal,
                    error = &wrapped_key_attest_error as &dyn std::error::Error,
                    "VMGS key-encryption failed due to igvm attest error"
                );
                if !retry_signal || i == (max_retry - 1) {
                    return Err(wrapped_key_attest_error);
                }
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
                    retry = i,
                    igvm_error_code = &igvm_error_code,
                    igvm_http_status_code = &http_status_code,
                    retry_signal = &retry_signal,
                    error = &key_release_attest_error as &dyn std::error::Error,
                    "VMGS key-encryption failed due to igvm attest error"
                );
                if !retry_signal || i == (max_retry - 1) {
                    return Err(key_release_attest_error);
                }
            }
            Err(e) => {
                tracing::error!(
                    CVM_ALLOWED,
                    retry = i,
                    error = &e as &dyn std::error::Error,
                    "VMGS key-encryption key request failed due to error",
                )
            }
        }

        // Stall on retries
        timer.sleep(std::time::Duration::new(1, 0)).await;
    }

    Err(RequestVmgsEncryptionKeysError::MaximumAttemptsReached)
}

/// Get windows epoch from host via GET and covert it into unix epoch.
async fn get_host_epoch_time(get: &GuestEmulationTransportClient) -> i64 {
    const WINDOWS_EPOCH: time::OffsetDateTime = time::macros::datetime!(1601-01-01 0:00 UTC);
    const NANOS_IN_SECOND: i64 = 1_000_000_000;
    const NANOS_100_IN_SECOND: i64 = NANOS_IN_SECOND / 100;
    let response = get.host_time().await;

    let host_time_since_windows_epoch = time::Duration::new(
        response.utc / NANOS_100_IN_SECOND,
        (response.utc % NANOS_100_IN_SECOND) as i32,
    );

    let linux_time =
        WINDOWS_EPOCH + host_time_since_windows_epoch - time::OffsetDateTime::UNIX_EPOCH;

    linux_time.whole_seconds()
}

/// Make the `IGVM_ATTEST` request to GET.
async fn make_igvm_attest_requests(
    get: &GuestEmulationTransportClient,
    transfer_key: &Rsa<Private>,
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

    match igvm_attest::key_release::parse_response(&response.response, transfer_key.size() as usize)
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
