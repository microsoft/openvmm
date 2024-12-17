// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of secure key release (SKR) scheme for stateful CVM to obtain VMGS
//! encryption keys.

use crate::crypto;
use crate::igvm_attest;
use crate::IgvmAttestRequestHelper;
use cvm_tracing::CVM_ALLOWED;
use guest_emulation_transport::GuestEmulationTransportClient;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_attestation_protocol::igvm_attest::get::KEY_RELEASE_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::igvm_attest::get::WRAPPED_KEY_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::vmgs::AGENT_DATA_MAX_SIZE;
use openssl::pkey::Private;
use openssl::rsa::Rsa;
use pal_async::local::LocalDriver;
use tee_call::TeeCall;
use thiserror::Error;
use vmgs::EncryptionAlgorithm;
use vmgs::Vmgs;

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub(crate) enum RequestVmgsEncryptionKeysError {
    #[error("failed to generate an RSA transfer key")]
    GenerateTransferKey(#[source] openssl::error::ErrorStack),
    #[error("failed to get a TEE attestation report")]
    GetAttestationReport(#[source] tee_call::Error),
    #[error("failed to create IgvmAttest WRAPPED_KEY request")]
    CreateIgvmAttestWrappedKeyRequest(#[source] igvm_attest::Error),
    #[error("failed to make an IgvmAttest WRAPPED_KEY GET request")]
    SendIgvmAttestWrappedKeyRequest(#[source] guest_emulation_transport::error::IgvmAttestError),
    #[error("failed to parse IgvmAttest WRAPPED_KEY response")]
    ParseIgvmAttestWrappedKeyResponse(#[source] igvm_attest::wrapped_key::WrappedKeyError),
    #[error("wrapped key from WRAPPED_KEY response is empty")]
    EmptyWrappedKey,
    #[error("key reference size {key_reference_size} from WRAPPED_KEY response was larger than expected {expected_size}")]
    InvalidKeyReferenceSize {
        key_reference_size: usize,
        expected_size: usize,
    },
    #[error("key reference from WRAPPED_KEY response is empty")]
    EmptyKeyReference,
    #[error("failed to create IgvmAttest KEY_RELEASE request")]
    CreateIgvmAttestKeyReleaseRequest(#[source] igvm_attest::Error),
    #[error("failed to make an IgvmAttest KEY_RELEASE GET request")]
    SendIgvmAttestKeyReleaseRequest(#[source] guest_emulation_transport::error::IgvmAttestError),
    #[error("failed to parse IgvmAttest KEY_RELEASE response")]
    ParseIgvmAttestKeyReleaseResponse(#[source] igvm_attest::key_release::KeyReleaseError),
    #[error("PKCS11 RSA AES key unwrap failed")]
    Pkcs11RsaAesKeyUnwrap(#[source] crypto::Pkcs11RsaAesKeyUnwrapError),
}

/// The return values of [`make_igvm_attest_requests`].
struct WrappedKeyVmgsEncryptionKeys {
    /// Optional RSA-AES-wrapped key blob.
    rsa_aes_wrapped_key: Option<Vec<u8>>,
    /// Optional wrapped DiskEncryptionSettings key blob.
    wrapped_des_key: Option<Vec<u8>>,
}

/// The return values of [`request_vmgs_encryption_keys`].
#[derive(Default)]
pub struct VmgsEncryptionKeys {
    /// Optional ingress RSA key-encryption key.
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
    let max_retry = if vmgs.get_encryption_algorithm() != EncryptionAlgorithm::NONE {
        MAXIMUM_RETRY_COUNT
    } else {
        NO_RETRY_COUNT
    };

    let mut wrapped_vmgs_keks = WrappedKeyVmgsEncryptionKeys {
        rsa_aes_wrapped_key: None,
        wrapped_des_key: None,
    };
    let mut tcb_version = None;
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

        tcb_version = result.tcb_version;

        // Get tenant keys based on attestation results, this might fail.
        match make_igvm_attest_requests(
            get,
            &transfer_key,
            &mut igvm_attest_request_helper,
            &result.report,
            agent_data,
        )
        .await
        {
            Ok(WrappedKeyVmgsEncryptionKeys {
                rsa_aes_wrapped_key,
                wrapped_des_key,
            }) if rsa_aes_wrapped_key.is_some() => {
                wrapped_vmgs_keks = WrappedKeyVmgsEncryptionKeys {
                    rsa_aes_wrapped_key,
                    wrapped_des_key,
                };

                break;
            }
            Ok(WrappedKeyVmgsEncryptionKeys {
                rsa_aes_wrapped_key: _,
                wrapped_des_key: _,
            }) if i == (max_retry - 1) => {
                tracing::error!("VMGS key-encryption failed after max number of attempts");
                break;
            }
            Ok(WrappedKeyVmgsEncryptionKeys {
                rsa_aes_wrapped_key: _,
                wrapped_des_key: _,
            }) => {
                tracing::warn!(CVM_ALLOWED, retry = i, "Failed to get VMGS key-encryption")
            }
            Err(e) if i == (max_retry - 1) => Err(e)?,
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

    let ingress_rsa_kek = if let Some(rsa_aes_wrapped_key) = wrapped_vmgs_keks.rsa_aes_wrapped_key {
        Some(
            crypto::pkcs11_rsa_aes_key_unwrap(&transfer_key, &rsa_aes_wrapped_key)
                .map_err(RequestVmgsEncryptionKeysError::Pkcs11RsaAesKeyUnwrap)?,
        )
    } else {
        tracing::error!(CVM_ALLOWED, "failed to unwrap VMGS key-encryption key");

        get.event_log_fatal(guest_emulation_transport::api::EventLogId::KEY_NOT_RELEASED)
            .await;

        None
    };

    Ok(VmgsEncryptionKeys {
        ingress_rsa_kek,
        wrapped_des_key: wrapped_vmgs_keks.wrapped_des_key,
        tcb_version,
    })
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
) -> Result<WrappedKeyVmgsEncryptionKeys, RequestVmgsEncryptionKeysError> {
    // Attempt to get wrapped DiskEncryptionSettings key
    igvm_attest_request_helper.set_request_type(
        openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
    );
    let request = igvm_attest_request_helper
        .create_request(attestation_report)
        .map_err(RequestVmgsEncryptionKeysError::CreateIgvmAttestWrappedKeyRequest)?;

    let response = get
        .igvm_attest([].into(), request, WRAPPED_KEY_RESPONSE_BUFFER_SIZE)
        .await
        .map_err(RequestVmgsEncryptionKeysError::SendIgvmAttestWrappedKeyRequest)?;

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
        // The request does not succeed. Ignore the wrapped des key.
        Err(igvm_attest::wrapped_key::WrappedKeyError::ResponseSizeTooSmall) => None,
        Err(e) => Err(RequestVmgsEncryptionKeysError::ParseIgvmAttestWrappedKeyResponse(e))?,
    };

    igvm_attest_request_helper.set_request_type(
        openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType::KEY_RELEASE_REQUEST,
    );
    let request = igvm_attest_request_helper
        .create_request(attestation_report)
        .map_err(RequestVmgsEncryptionKeysError::CreateIgvmAttestKeyReleaseRequest)?;

    // Get tenant keys based on attestation results
    let response = get
        .igvm_attest(
            agent_data.to_vec(),
            request,
            KEY_RELEASE_RESPONSE_BUFFER_SIZE,
        )
        .await
        .map_err(RequestVmgsEncryptionKeysError::SendIgvmAttestKeyReleaseRequest)?;

    match igvm_attest::key_release::parse_response(&response.response, transfer_key.size() as usize)
    {
        Ok(rsa_aes_wrapped_key) => Ok(WrappedKeyVmgsEncryptionKeys {
            rsa_aes_wrapped_key: Some(rsa_aes_wrapped_key),
            wrapped_des_key,
        }),
        Err(igvm_attest::key_release::KeyReleaseError::ResponseSizeTooSmall) => {
            // The request does not succeed
            Ok(WrappedKeyVmgsEncryptionKeys {
                rsa_aes_wrapped_key: None,
                wrapped_des_key: None,
            })
        }
        Err(e) => Err(RequestVmgsEncryptionKeysError::ParseIgvmAttestKeyReleaseResponse(e))?,
    }
}
