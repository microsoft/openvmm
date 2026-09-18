// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This modules implements attestation protocols for Underhill to support TVM
//! and CVM, including getting a tenant key via secure key release (SKR) for
//! unlocking VMGS and requesting an attestation key (AK) certificate for TPM.
//! The module also implements the VMGS unlocking process based on SKR.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

mod hardware_key_sealing;
mod igvm_attest;
mod jwt;
mod key_protector;
pub mod runtime_sealing;
mod secure_key_release;
mod vmgs;

#[cfg(test)]
mod test_helpers;

pub use igvm_attest::Error as IgvmAttestError;
pub use igvm_attest::IgvmAttestRequestHelper;
pub use igvm_attest::ak_cert::parse_response as parse_ak_cert_response;

use crate::hardware_key_sealing::HardwareKeySealingError;
use crate::jwt::JwtError;
use crate::jwt::JwtHelper;
use ::vmgs::EncryptionAlgorithm;
use ::vmgs::GspType;
use ::vmgs::Vmgs;
use crypto::rsa::RsaKeyPair;
use crypto::sha_256::sha_256;
use cvm_tracing::CVM_ALLOWED;
use get_protocol::dps_json::GuestStateEncryptionPolicy;
use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::GspExtendedStatusFlags;
use guest_emulation_transport::api::GuestStateProtection;
use guest_emulation_transport::api::GuestStateProtectionById;
use guid::Guid;
use hardware_key_sealing::HardwareDerivedKeys;
use key_protector::GetKeysFromKeyProtectorError;
use key_protector::KeyProtectorExt as _;
use mesh::MeshPayload;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::HardwareSealingPolicy;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::VmgsProvisioner;
use openhcl_attestation_protocol::vmgs::AES_GCM_KEY_LENGTH;
use openhcl_attestation_protocol::vmgs::AGENT_DATA_MAX_SIZE;
use openhcl_attestation_protocol::vmgs::HW_KEY_PROTECTOR_CURRENT_VERSION;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtectorV3;
use openhcl_attestation_protocol::vmgs::KeyProtector;
use openhcl_attestation_protocol::vmgs::SecurityProfile;
use pal_async::local::LocalDriver;
use secure_key_release::VmgsEncryptionKeys;
use serde::Deserialize;
use serde::Serialize;
use static_assertions::const_assert_eq;
use std::fmt::Debug;
use tee_call::KeyDerivationPolicy;
use tee_call::REPORT_DATA_SIZE;
use tee_call::TeeCall;
use tee_call::TeeType;
use thiserror::Error;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// An attestation error.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(AttestationErrorInner);

impl<T: Into<AttestationErrorInner>> From<T> for Error {
    fn from(value: T) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Error)]
enum AttestationErrorInner {
    #[error("read security profile from vmgs")]
    ReadSecurityProfile(#[source] vmgs::ReadFromVmgsError),
    #[error("failed to get derived keys")]
    GetDerivedKeys(#[source] GetDerivedKeysError),
    #[error("failed to read key protector from vmgs")]
    ReadKeyProtector(#[source] vmgs::ReadFromVmgsError),
    #[error("failed to read key protector by id from vmgs")]
    ReadKeyProtectorById(#[source] vmgs::ReadFromVmgsError),
    #[error("failed to unlock vmgs data store")]
    UnlockVmgsDataStore(#[source] UnlockVmgsDataStoreError),
    #[error("failed to finalize required hardware sealing")]
    FinalizeHardwareSealing(#[source] FinalizeHardwareSealingError),
    #[error("failed to read guest secret key from vmgs")]
    ReadGuestSecretKey(#[source] vmgs::ReadFromVmgsError),
    #[error("failed to verify VMGS provenance")]
    Provenance(#[source] ProvenanceError),
    #[error("failed to get an attestation report")]
    GetAttestationReport(#[source] tee_call::Error),
    #[error(
        "host requested HardwareSealing GSP, but hardware sealing is not available \
         (tee_available={tee_available}, hardware_sealing_policy={hardware_sealing_policy:?})"
    )]
    HardwareSealingRequestedButNotAvailable {
        tee_available: bool,
        hardware_sealing_policy: HardwareSealingPolicy,
    },
}

#[derive(Debug, Error)]
enum GetDerivedKeysError {
    #[error("failed to get ingress/egress keys from the key protector")]
    GetKeysFromKeyProtector(#[source] GetKeysFromKeyProtectorError),
    #[error("failed to fetch GSP")]
    FetchGuestStateProtectionById(
        #[source] guest_emulation_transport::error::GuestStateProtectionByIdError,
    ),
    #[error("GSP By Id required, but no GSP By Id found")]
    GspByIdRequiredButNotFound,
    #[error("failed to unseal the ingress key using hardware derived keys")]
    UnsealIngressKeyUsingHardwareDerivedKeys(#[source] HardwareKeySealingError),
    #[error("failed to get an ingress key from hardware key protector")]
    GetIngressKeyFromHardwareKeyProtectorFailed,
    #[error("failed to get an ingress key from key protector")]
    GetIngressKeyFromKpFailed,
    #[error("failed to get an ingress key from guest state protection")]
    GetIngressKeyFromKGspFailed,
    #[error("failed to get an ingress key from guest state protection by id")]
    GetIngressKeyFromKGspByIdFailed,
    #[error("Encryption cannot be disabled if VMGS was previously encrypted")]
    DisableVmgsEncryptionFailed,
    #[error("VMGS encryption is required, but no encryption sources were found")]
    EncryptionRequiredButNotFound,
    #[error("failed to seal the egress key using hardware derived keys")]
    SealEgressKeyUsingHardwareDerivedKeys(#[source] HardwareKeySealingError),
    #[error("failed to write to `FileId::HW_KEY_PROTECTOR` in vmgs")]
    VmgsWriteHardwareKeyProtector(#[source] vmgs::WriteToVmgsError),
    #[error("failed to get derived key by id")]
    GetDerivedKeyById(#[source] GetDerivedKeysByIdError),
    #[error("failed to derive an ingress key")]
    DeriveIngressKey(#[source] crypto::kbkdf::KbkdfError),
    #[error("failed to derive an egress key")]
    DeriveEgressKey(#[source] crypto::kbkdf::KbkdfError),
    #[error("Hardware sealing is required, but not supported")]
    HardwareSealingRequiredButNotSupported,
}

#[derive(Debug, Error)]
enum GetDerivedKeysByIdError {
    #[error("failed to derive an egress key based on current vm bios guid")]
    DeriveEgressKeyUsingCurrentVmId(#[source] crypto::kbkdf::KbkdfError),
    #[error("invalid derived egress key size {key_size}, expected {expected_size}")]
    InvalidDerivedEgressKeySize {
        key_size: usize,
        expected_size: usize,
    },
    #[error("failed to derive an ingress key based on key protector Id from vmgs")]
    DeriveIngressKeyUsingKeyProtectorId(#[source] crypto::kbkdf::KbkdfError),
    #[error("invalid derived egress key size {key_size}, expected {expected_size}")]
    InvalidDerivedIngressKeySize {
        key_size: usize,
        expected_size: usize,
    },
}

#[derive(Debug, Error)]
enum UnlockVmgsDataStoreError {
    #[error("failed to unlock vmgs with the existing egress key")]
    VmgsUnlockUsingExistingEgressKey(#[source] ::vmgs::Error),
    #[error("failed to unlock vmgs with the existing ingress key")]
    VmgsUnlockUsingExistingIngressKey(#[source] ::vmgs::Error),
    #[error("failed to write key protector to vmgs")]
    WriteKeyProtector(#[source] vmgs::WriteToVmgsError),
    #[error("failed to write key protector by id to vmgs")]
    WriteKeyProtectorById(#[source] vmgs::WriteToVmgsError),
    #[error("failed to update the vmgs encryption key")]
    UpdateVmgsEncryptionKey(#[source] ::vmgs::Error),
    #[error("failed to persist all key protectors")]
    PersistAllKeyProtectors(#[source] PersistAllKeyProtectorsError),
}

#[derive(Debug, Error)]
enum PersistAllKeyProtectorsError {
    #[error("failed to write key protector to vmgs")]
    KeyProtector(#[source] vmgs::WriteToVmgsError),
    #[error("failed to write key protector by id to vmgs")]
    KeyProtectorById(#[source] vmgs::WriteToVmgsError),
    #[error("failed to write hardware key protector to vmgs")]
    HardwareKeyProtector(#[source] vmgs::WriteToVmgsError),
}

#[derive(Debug, Error)]
enum ProvenanceError {
    #[error("failed to decode provenance doc")]
    DecodeProvenanceDoc(#[source] JwtError),
    #[error("failed to verify JWT signature")]
    VerifySignature(#[source] JwtError),
    #[error("invalid signature")]
    InvalidSignature,
    #[error("missing leaf certificate subject common name")]
    MissingLeafCertSubjectName,
    #[error("invalid root certificate")]
    InvalidRootCert,
    #[error("failed to convert VMGSID data")]
    InvalidVmgsidData(#[source] std::str::Utf8Error),
    #[error("failed to parse VMGSID seed data")]
    ParseVmgsidSeedData,
    #[error("failed to decode VMGSID seed data")]
    DecodeVmgsidData(#[source] hex::FromHexError),
    #[error("X509 certificate error")]
    X509Error(#[source] crypto::x509::X509Error),
    #[error("SP800-108 KDF error")]
    KdfError(#[source] crypto::kbkdf::KbkdfError),
    #[error("failed to parse VMGSID")]
    ParseVmgsid(#[source] guid::ParseError),
}

// Operation types for provisioning telemetry.
#[derive(Debug)]
enum LogOpType {
    BeginDecryptVmgs,
    DecryptVmgs,
    ConvertEncryptionType,
}

/// Label used by `derive_key`
const VMGS_KEY_DERIVE_LABEL: &[u8; 7] = b"VMGSKEY";

/// KBKDF from SP800-108, using HMAC-SHA-256.
fn derive_key(
    key: &[u8],
    context: &[u8],
    label: &[u8],
) -> Result<[u8; AES_GCM_KEY_LENGTH], crypto::kbkdf::KbkdfError> {
    let output = crypto::kbkdf::kbkdf_hmac_sha256(key, context, label, AES_GCM_KEY_LENGTH)?;
    Ok(output.try_into().unwrap())
}

#[derive(Debug)]
struct Keys {
    ingress: [u8; AES_GCM_KEY_LENGTH],
    decrypt_egress: Option<[u8; AES_GCM_KEY_LENGTH]>,
    encrypt_egress: [u8; AES_GCM_KEY_LENGTH],
}

/// Key protector settings
#[derive(Clone, Copy)]
struct KeyProtectorSettings {
    /// Whether to update key protector
    should_write_kp: bool,
    /// Whether GSP by id is used
    use_gsp_by_id: bool,
    /// Whether hardware key sealing is used
    use_hardware_unlock: bool,
    /// GSP type used for decryption (for logging)
    decrypt_gsp_type: GspType,
    /// GSP type used for encryption (for logging)
    encrypt_gsp_type: GspType,
}

/// Helper struct for [`protocol::vmgs::KeyProtectorById`]
struct KeyProtectorById {
    /// The instance of [`protocol::vmgs::KeyProtectorById`].
    pub inner: openhcl_attestation_protocol::vmgs::KeyProtectorById,
    /// Indicate if the instance is read from the VMGS file.
    pub found_id: bool,
}

/// Host attestation settings obtained via the GET GSP call-out.
pub struct HostAttestationSettings {
    /// Whether refreshing tpm seeds is needed.
    pub refresh_tpm_seeds: bool,
}

/// The return values of [`get_derived_keys`].
struct DerivedKeyResult {
    /// Optional derived keys.
    derived_keys: Option<Keys>,
    /// The instance of [`KeyProtectorSettings`].
    key_protector_settings: KeyProtectorSettings,
    /// The instance of [`GspExtendedStatusFlags`] returned by GSP.
    gsp_extended_status_flags: GspExtendedStatusFlags,
    /// Optional hardware key protector.
    hardware_key_protector: Option<HardwareKeyProtectorV3>,
    /// This attempt sealed and successfully wrote its egress DEK's protector
    /// before unlock. Never inferred from an existing VMGS entry.
    hardware_key_protector_written: bool,
}

/// Only returned after this attempt successfully unlocks and persists VMGS.
struct UnlockResult {
    state_refresh_request: bool,
    /// A freshly sealed protector for the active DEK was written and flushed.
    hardware_sealed: bool,
}

/// The return values of [`initialize_platform_security`].
pub struct PlatformAttestationData {
    /// The instance of [`HostAttestationSettings`].
    pub host_attestation_settings: HostAttestationSettings,
    /// The agent data used by an attestation request.
    pub agent_data: Option<Vec<u8>>,
    /// The guest secret key.
    pub guest_secret_key: Option<Vec<u8>>,
    /// Runtime-only floor collected from trusted local boot reports, without
    /// extra hardware calls. Reports are retained across SKR errors and unlock
    /// retries, but export requires successful hardware sealing and persistence
    /// for the active DEK in the successful unlock attempt, including a final
    /// flush. `None` also means unsupported/disabled sealing, no report, or a
    /// malformed, incompatible, or lowered observation. In that case runtime
    /// hardware resealing must stay disabled; do not lazily initialize after an event.
    pub runtime_tcb_floor: Option<runtime_sealing::RuntimeTcbFloor>,
}

/// The attestation type to use.
#[derive(Debug, MeshPayload, Copy, Clone, PartialEq, Eq)]
pub enum AttestationType {
    /// Use the SEV-SNP TEE for attestation.
    Snp,
    /// Use the TDX TEE for attestation.
    Tdx,
    /// Use the VBS TEE for attestation.
    Vbs,
    /// Use the CCA TEE for attestation,
    Cca,
    /// Use trusted host-based attestation.
    Host,
}

/// Request VMGS encryption keys and unlock the VMGS.
/// If successful, return the state refresh and hardware sealing outcomes for
/// this attempt. If unsuccessful, return an error and a bool indicating
/// whether to retry.
async fn try_unlock_vmgs(
    get: &GuestEmulationTransportClient,
    bios_guid: Guid,
    attestation_vm_config: &AttestationVmConfig,
    vmgs: &mut Vmgs,
    tee_call: Option<&dyn TeeCall>,
    guest_state_encryption_policy: GuestStateEncryptionPolicy,
    strict_encryption_policy: bool,
    require_hardware_sealing: bool,
    agent_data: &mut [u8; AGENT_DATA_MAX_SIZE],
    key_protector_by_id: &mut KeyProtectorById,
    boot_tcb_floor: &mut runtime_sealing::BootTcbFloor,
) -> Result<UnlockResult, (AttestationErrorInner, bool)> {
    let skr_response = if let Some(tee_call) = tee_call {
        if !require_hardware_sealing {
            tracing::info!(CVM_ALLOWED, "Retrieving key-encryption key");
            // Retrieve the tenant key via attestation
            secure_key_release::request_vmgs_encryption_keys(
                get,
                tee_call,
                vmgs,
                attestation_vm_config,
                agent_data,
                boot_tcb_floor,
            )
            .await
        } else {
            tracing::info!(
                CVM_ALLOWED,
                "Getting attestation report only for hardware sealing"
            );

            let report = tee_call
                .get_attestation_report(&[0; REPORT_DATA_SIZE])
                .map_err(|e| (AttestationErrorInner::GetAttestationReport(e), false))?;
            boot_tcb_floor.observe(tee_call, &report);

            Ok(VmgsEncryptionKeys {
                ingress_rsa_kek: None,
                wrapped_des_key: None,
                key_derivation_svn: report.key_derivation_svn,
            })
        }
    } else {
        tracing::info!(CVM_ALLOWED, "Key-encryption key retrieval not required");

        // Attestation is unavailable, assume no tenant key
        Ok(VmgsEncryptionKeys::default())
    };

    let retry = match &skr_response {
        Ok(_) => false,
        Err((_, r)) => *r,
    };

    let skip_hw_unsealing = matches!(
        &skr_response,
        Err((
            secure_key_release::RequestVmgsEncryptionKeysError::ParseIgvmAttestKeyReleaseResponse(
                igvm_attest::key_release::KeyReleaseError::ParseHeader(
                    igvm_attest::Error::Attestation {
                        skip_hw_unsealing_signal: true,
                        ..
                    },
                ),
            ),
            _,
        ))
    );

    let VmgsEncryptionKeys {
        ingress_rsa_kek,
        wrapped_des_key,
        key_derivation_svn,
    } = match skr_response {
        Ok(k) => {
            tracing::info!(CVM_ALLOWED, "Successfully retrieved key-encryption key");
            k
        }
        Err((e, _)) => {
            // Non-fatal, allowing for hardware-based recovery
            tracing::error!(
                CVM_ALLOWED,
                error = &e as &dyn std::error::Error,
                "Failed to retrieve key-encryption key"
            );

            VmgsEncryptionKeys::default()
        }
    };

    let mut key_protector = if !require_hardware_sealing {
        // Determine the minimal size of a DEK entry based on whether `wrapped_des_key` is present
        let dek_minimal_size = if wrapped_des_key.is_some() {
            key_protector::AES_WRAPPED_AES_KEY_LENGTH
        } else {
            key_protector::RSA_WRAPPED_AES_KEY_LENGTH
        };

        // Read Key Protector blob from VMGS
        tracing::info!(
            CVM_ALLOWED,
            dek_minimal_size = dek_minimal_size,
            "Reading key protector from VMGS"
        );

        vmgs::read_key_protector(vmgs, dek_minimal_size)
            .await
            .map_err(|e| (AttestationErrorInner::ReadKeyProtector(e), false))?
    } else {
        tracing::info!(
            CVM_ALLOWED,
            "Hardware sealing is required, skip reading key protector from VMGS"
        );
        KeyProtector::new_zeroed()
    };

    let start_time = std::time::SystemTime::now();
    let vmgs_encrypted = vmgs.encrypted();

    // Only the `Hash` policy mixes the OpenHCL measurement into the hardware key
    // derivation; the other policies do not, allowing the sealed key to survive
    // OpenHCL measurement changes (e.g. servicing).
    let mix_measurement = matches!(
        attestation_vm_config.hardware_sealing_policy,
        HardwareSealingPolicy::Hash
    );

    let key_derivation_policy = key_derivation_svn.map(|svn| KeyDerivationPolicy {
        svn,
        mix_measurement,
    });

    tracing::info!(
        CVM_ALLOWED,
        key_derivation_policy=?key_derivation_policy,
        vmgs_encrypted,
        op_type = ?LogOpType::BeginDecryptVmgs,
        "Deriving keys"
    );

    let derived_keys_result = get_derived_keys(
        get,
        tee_call,
        vmgs,
        &mut key_protector,
        key_protector_by_id,
        bios_guid,
        attestation_vm_config,
        vmgs_encrypted,
        ingress_rsa_kek.as_ref(),
        wrapped_des_key.as_deref(),
        key_derivation_policy,
        guest_state_encryption_policy,
        strict_encryption_policy,
        require_hardware_sealing,
        skip_hw_unsealing,
    )
    .await
    .map_err(|e| {
        tracing::error!(
            CVM_ALLOWED,
            op_type = ?LogOpType::DecryptVmgs,
            success = false,
            err = &e as &dyn std::error::Error,
            latency = std::time::SystemTime::now()
                .duration_since(start_time)
                .map_or(0, |d| d.as_millis()),
            "Failed to derive keys"
        );
        (AttestationErrorInner::GetDerivedKeys(e), retry)
    })?;

    tracing::info!("Unlocking VMGS");

    // Capture key identity before consuming Keys. A deferred protector is
    // persisted by unlock_vmgs_data_store; early writes are tracked explicitly.
    // Neither a preexisting entry nor success in a failed attempt qualifies.
    let sealed_egress_key = derived_keys_result
        .derived_keys
        .as_ref()
        .filter(|_| {
            derived_keys_result.hardware_key_protector.is_some()
                || derived_keys_result.hardware_key_protector_written
        })
        .map(|keys| keys.encrypt_egress);

    if let Err(e) = unlock_vmgs_data_store(
        vmgs,
        vmgs_encrypted,
        &mut key_protector,
        key_protector_by_id,
        derived_keys_result.hardware_key_protector,
        derived_keys_result.derived_keys,
        derived_keys_result.key_protector_settings,
        bios_guid,
    )
    .await
    {
        tracing::error!(
            CVM_ALLOWED,
            op_type = ?LogOpType::DecryptVmgs,
            success = false,
            err = &e as &dyn std::error::Error,
            latency = std::time::SystemTime::now()
                .duration_since(start_time)
                .map_or(0, |d| d.as_millis()),
            "Failed to unlock datastore"
        );
        get.event_log_fatal(guest_emulation_transport::api::EventLogId::ATTESTATION_FAILED)
            .await;

        Err((AttestationErrorInner::UnlockVmgsDataStore(e), retry))?;
    }

    // Hardware recovery is essential even in stateful mode if SKR/GSP did
    // not supply the ingress DEK. Do not treat that path as an optional backup.
    let sealing_required = require_hardware_sealing
        || derived_keys_result
            .key_protector_settings
            .use_hardware_unlock;
    let hardware_sealed =
        match finalize_hardware_sealing(vmgs, sealed_egress_key, sealing_required).await {
            Ok(sealed) => sealed,
            Err(error) => {
                tracing::error!(
                    CVM_ALLOWED,
                    op_type = ?LogOpType::DecryptVmgs,
                    success = false,
                    error = &error as &dyn std::error::Error,
                    "Failed to finalize required hardware sealing"
                );
                get.event_log_fatal(guest_emulation_transport::api::EventLogId::ATTESTATION_FAILED)
                    .await;
                // Unlock may already have rotated the DEK and written metadata.
                // Do not replay that flow using the SKR retry flag after a flush
                // failure. Abort boot rather than report unconfirmed durability.
                return Err((AttestationErrorInner::FinalizeHardwareSealing(error), false));
            }
        };

    tracing::info!(
        CVM_ALLOWED,
        op_type = ?LogOpType::DecryptVmgs,
        success = true,
        decrypt_gsp_type = ?derived_keys_result
            .key_protector_settings
            .decrypt_gsp_type,
        encrypt_gsp_type = ?derived_keys_result
            .key_protector_settings
            .encrypt_gsp_type,
        latency = std::time::SystemTime::now().duration_since(start_time).map_or(0, |d| d.as_millis()),
        "Unlocked datastore"
    );

    Ok(UnlockResult {
        state_refresh_request: derived_keys_result
            .gsp_extended_status_flags
            .state_refresh_request(),
        hardware_sealed,
    })
}

#[derive(Debug, Error)]
enum FinalizeHardwareSealingError {
    #[error("no hardware protector was sealed for this unlock attempt")]
    MissingSealedKey,
    #[error("active VMGS encryption key is unavailable")]
    ActiveKey(#[source] ::vmgs::Error),
    #[error("sealed DEK does not match the active VMGS encryption key")]
    ActiveKeyMismatch,
    #[error("failed to flush the hardware protector and VMGS metadata")]
    Flush(#[source] ::vmgs::Error),
}

/// Check that the sealed DEK is active, then flush completed boot writes.
/// Required sealing (including hardware-unseal recovery) must fail boot if
/// identity or durability cannot be confirmed. Optional backup failures only
/// disable runtime enrollment. No keys are regenerated or writes replayed here.
async fn finalize_hardware_sealing(
    vmgs: &mut Vmgs,
    sealed_egress_key: Option<[u8; AES_GCM_KEY_LENGTH]>,
    required: bool,
) -> Result<bool, FinalizeHardwareSealingError> {
    if sealed_egress_key.is_none() && !required {
        return Ok(false);
    }
    let result = async {
        let sealed_egress_key =
            sealed_egress_key.ok_or(FinalizeHardwareSealingError::MissingSealedKey)?;
        // Old-egress recovery can leave another key active. Compare trusted
        // in-memory key state, never an untrusted protector header.
        let active = vmgs
            .active_encryption_key()
            .map_err(FinalizeHardwareSealingError::ActiveKey)?;
        if !constant_time_eq::constant_time_eq_32(active, &sealed_egress_key) {
            return Err(FinalizeHardwareSealingError::ActiveKeyMismatch);
        }
        vmgs.flush()
            .await
            .map_err(FinalizeHardwareSealingError::Flush)
    }
    .await;
    match result {
        Ok(()) => Ok(true),
        Err(error) if required => Err(error),
        Err(error) => {
            tracelimit::warn_ratelimited!(
                CVM_ALLOWED,
                error = &error as &dyn std::error::Error,
                "Failed to finalize optional hardware sealing; runtime hardware resealing disabled"
            );
            Ok(false)
        }
    }
}

/// If required, attest platform. Gets VMGS datastore key.
///
/// Returns `refresh_tpm_seeds` (the host side GSP service indicating
/// whether certain state needs to be updated), along with the fully
/// initialized VMGS client.
pub async fn initialize_platform_security(
    get: &GuestEmulationTransportClient,
    bios_guid: Guid,
    attestation_vm_config: &AttestationVmConfig,
    vmgs: &mut Vmgs,
    tee_call: Option<&dyn TeeCall>,
    suppress_attestation: bool,
    driver: LocalDriver,
    guest_state_encryption_policy: GuestStateEncryptionPolicy,
    strict_encryption_policy: bool,
) -> Result<PlatformAttestationData, Error> {
    const MAXIMUM_RETRY_COUNT: usize = 10;
    const NO_RETRY_COUNT: usize = 1;

    tracing::info!(CVM_ALLOWED,
        tee_type=?tee_call.map(|tee| tee.tee_type()),
        secure_boot=attestation_vm_config.secure_boot,
        tpm_enabled=attestation_vm_config.tpm_enabled,
        tpm_persisted=attestation_vm_config.tpm_persisted,
        // Hardware sealing requires a TEE that can derive keys, so support is
        // gated on both the TEE being present and exposing `get_derived_key`.
        hardware_sealing_supported=tee_call.and_then(|tee| tee.supports_get_derived_key()).is_some(),
        hardware_sealing_policy=?attestation_vm_config.hardware_sealing_policy,
        "Reading security profile");

    // Read Security Profile from VMGS
    // Currently this only includes "Key Reference" data, which is not attested data, is opaque to the
    // OpenHCL, and is passed to the IGVMm agent outside of the report contents.
    let SecurityProfile { mut agent_data } = vmgs::read_security_profile(vmgs)
        .await
        .map_err(AttestationErrorInner::ReadSecurityProfile)?;

    // Hardware sealing is *required* (the only source of the VMGS DEK) when all
    // hold: the VM is a CVM (tee_call available), it is stateless
    // (suppress_attestation = true, so SKR is bypassed), the host requests
    // `HardwareSealing`, and the attested config permits it
    // (hardware_sealing_policy != None). Host invariant: the host selects
    // exclusive hardware sealing only in stateless mode paired with
    // `HardwareSealingPolicy::{Hash, Signer}`. In stateful mode it is at most a
    // backup recovery path, so this flag stays false.
    let require_hardware_sealing = tee_call.is_some()
        && suppress_attestation
        && matches!(
            guest_state_encryption_policy,
            GuestStateEncryptionPolicy::HardwareSealing
        )
        && !matches!(
            attestation_vm_config.hardware_sealing_policy,
            HardwareSealingPolicy::None
        );

    // TDX has no signer/identity key-policy register to substitute for `MRTD`
    // when deriving a measurement-independent key, so signer-based hardware
    // sealing can never succeed on TDX. Reject the unsupported combination here,
    // where both the sealing policy and the TEE type are known, so we can fail
    // closed with a precise diagnostic instead of only tripping the low-level
    // guard deep in `TdxCall::get_derived_key` (kept as defense in depth).
    if matches!(tee_call.map(|tee| tee.tee_type()), Some(TeeType::Tdx))
        && matches!(
            attestation_vm_config.hardware_sealing_policy,
            HardwareSealingPolicy::Signer
        )
    {
        tracing::error!(
            CVM_ALLOWED,
            "signer-based hardware sealing is not supported on TDX; TDX has no \
             identity key-policy register to bind a measurement-independent key to"
        );
        // Exclusive (stateless) hardware sealing is the only encryption source,
        // so fail closed. In the stateful (backup) case the error log above
        // suffices; the low-level `tee_call` guard skips the backup seal.
        if require_hardware_sealing {
            return Err(
                AttestationErrorInner::HardwareSealingRequestedButNotAvailable {
                    tee_available: tee_call.is_some(),
                    hardware_sealing_policy: attestation_vm_config.hardware_sealing_policy,
                }
                .into(),
            );
        }
    }

    // A stateful (`!suppress_attestation`) request for `HardwareSealing` breaks
    // the invariant above. We don't fail closed because the normal attestation
    // flow still encrypts the VMGS (the directive is just downgraded to a
    // backup); warn so the host bug is observable.
    if !suppress_attestation
        && matches!(
            guest_state_encryption_policy,
            GuestStateEncryptionPolicy::HardwareSealing
        )
    {
        tracing::warn!(
            CVM_ALLOWED,
            ?guest_state_encryption_policy,
            hardware_sealing_policy = ?attestation_vm_config.hardware_sealing_policy,
            "host requested exclusive hardware sealing in stateful mode; \
             ignoring and proceeding with the normal attestation flow"
        );
    }

    // Attestation is suppressed and hardware sealing is not required, indicating that VMGS encryption is bypassed.
    // Skip the attestation flow and return the `agent_data` that is required by TPM AK cert request.
    if suppress_attestation && !require_hardware_sealing {
        // This branch is the normal stateless (attestation-suppressed) path when
        // hardware sealing is not requested. The one exception is when the host
        // requested `GuestStateEncryptionPolicy::HardwareSealing` but failed to
        // provide a usable hardware sealing policy on a CVM (the host invariant
        // above), in which case we fail closed rather than silently downgrading
        // to no encryption.
        if matches!(
            guest_state_encryption_policy,
            GuestStateEncryptionPolicy::HardwareSealing
        ) {
            return Err(
                AttestationErrorInner::HardwareSealingRequestedButNotAvailable {
                    tee_available: tee_call.is_some(),
                    hardware_sealing_policy: attestation_vm_config.hardware_sealing_policy,
                }
                .into(),
            );
        }

        tracing::info!(
            CVM_ALLOWED,
            ?guest_state_encryption_policy,
            hardware_sealing_policy = ?attestation_vm_config.hardware_sealing_policy,
            tee_available = tee_call.is_some(),
            "Suppressing attestation; VMGS encryption is bypassed"
        );

        return Ok(PlatformAttestationData {
            host_attestation_settings: HostAttestationSettings {
                refresh_tpm_seeds: false,
            },
            agent_data: Some(agent_data.to_vec()),
            guest_secret_key: None,
            runtime_tcb_floor: None,
        });
    }

    let (mut key_protector_by_id, vm_id_changed) = if !require_hardware_sealing {
        // Read VM id from VMGS
        tracing::info!(CVM_ALLOWED, "Reading VM ID from VMGS");
        let key_protector_by_id = match vmgs::read_key_protector_by_id(vmgs).await {
            Ok(key_protector_by_id) => KeyProtectorById {
                inner: key_protector_by_id,
                found_id: true,
            },
            Err(vmgs::ReadFromVmgsError::EntryNotFound(_)) => KeyProtectorById {
                inner: openhcl_attestation_protocol::vmgs::KeyProtectorById::new_zeroed(),
                found_id: false,
            },
            Err(e) => { Err(AttestationErrorInner::ReadKeyProtectorById(e)) }?,
        };

        // Check if the VM id has been changed since last boot with KP write
        let vm_id_changed = if key_protector_by_id.found_id {
            let changed = key_protector_by_id.inner.id_guid != bios_guid;
            if changed {
                tracing::info!("VM Id has changed since last boot");
            };
            changed
        } else {
            // Previous id in KP not found means this is the first boot or the GspById
            // is not provisioned, treat id as unchanged for this case.
            false
        };

        (key_protector_by_id, vm_id_changed)
    } else {
        // When hardware sealing is required, the key protector by id is not used, and VM id change does not trigger state refresh.
        (
            KeyProtectorById {
                inner: openhcl_attestation_protocol::vmgs::KeyProtectorById::new_zeroed(),
                found_id: false,
            },
            false,
        )
    };

    // Retry attestation call-out if necessary (if VMGS encrypted).
    // The IGVm Agent could be down for servicing, or the TDX service VM might not be ready, or a dynamic firmware
    // update could mean that the report was not verifiable.
    let vmgs_encrypted: bool = vmgs.encrypted();
    let max_retry = if vmgs_encrypted {
        MAXIMUM_RETRY_COUNT
    } else {
        NO_RETRY_COUNT
    };

    let mut timer = pal_async::timer::PolledTimer::new(&driver);
    let mut i = 0;
    // Observe existing reports at their source, independently of SKR success.
    // Never recreate this collector on retry or request a fallback report.
    let mut boot_tcb_floor = runtime_sealing::BootTcbFloor::new(tee_call, attestation_vm_config);

    let UnlockResult {
        state_refresh_request: state_refresh_request_from_gsp,
        hardware_sealed,
    } = loop {
        tracing::info!(CVM_ALLOWED, attempt = i, "attempt to unlock VMGS file");

        let response = try_unlock_vmgs(
            get,
            bios_guid,
            attestation_vm_config,
            vmgs,
            tee_call,
            guest_state_encryption_policy,
            strict_encryption_policy,
            require_hardware_sealing,
            &mut agent_data,
            &mut key_protector_by_id,
            &mut boot_tcb_floor,
        )
        .await;

        match response {
            Ok(result) => break result,
            Err((e, false)) => Err(e)?,
            Err((e, true)) => {
                if i >= max_retry - 1 {
                    Err(e)?
                }
            }
        }

        // Stall on retries
        timer.sleep(std::time::Duration::new(1, 0)).await;
        i += 1;
    };

    let host_attestation_settings = HostAttestationSettings {
        refresh_tpm_seeds: { state_refresh_request_from_gsp | vm_id_changed },
    };

    tracing::info!(
        CVM_ALLOWED,
        state_refresh_request_from_gsp = state_refresh_request_from_gsp,
        vm_id_changed = vm_id_changed,
        "determine if refreshing tpm seeds is needed"
    );

    // Read guest secret key from unlocked VMGS
    let guest_secret_key = match vmgs::read_guest_secret_key(vmgs).await {
        Ok(data) => Some(data.guest_secret_key.to_vec()),
        Err(vmgs::ReadFromVmgsError::EntryNotFound(_)) => None,
        Err(e) => return Err(AttestationErrorInner::ReadGuestSecretKey(e).into()),
    };

    Ok(PlatformAttestationData {
        host_attestation_settings,
        agent_data: Some(agent_data.to_vec()),
        guest_secret_key,
        runtime_tcb_floor: boot_tcb_floor.finish().filter(|_| hardware_sealed),
    })
}

/// Get ingress and egress keys for the VMGS, unlock VMGS,
/// remove old key if necessary, and update KP.
/// If key rolling did not complete successfully last time, there may be an
/// old egress key in the VMGS, whose contents can be controlled by the host.
/// This key can be used to attempt decryption but must not be used to
/// re-encrypt the VMGS.
async fn unlock_vmgs_data_store(
    vmgs: &mut Vmgs,
    vmgs_encrypted: bool,
    key_protector: &mut KeyProtector,
    key_protector_by_id: &mut KeyProtectorById,
    hardware_key_protector: Option<HardwareKeyProtectorV3>,
    derived_keys: Option<Keys>,
    key_protector_settings: KeyProtectorSettings,
    bios_guid: Guid,
) -> Result<(), UnlockVmgsDataStoreError> {
    let mut new_key = false; // Indicate if we need to add a new key after unlock

    let Some(Keys {
        ingress: new_ingress_key,
        decrypt_egress: old_egress_key,
        encrypt_egress: new_egress_key,
    }) = derived_keys
    else {
        tracing::info!(
            CVM_ALLOWED,
            "Encryption disabled, skipping unlock vmgs data store"
        );
        return Ok(());
    };

    if !constant_time_eq::constant_time_eq_32(&new_ingress_key, &new_egress_key) {
        tracing::trace!(CVM_ALLOWED, "EgressKey is different than IngressKey");
        new_key = true;
    }

    // Call unlock_with_encryption_key using ingress_key if datastore is encrypted
    let mut provision = false;
    if vmgs_encrypted {
        tracing::info!(CVM_ALLOWED, "Decrypting vmgs file...");
        if let Err(e) = vmgs.unlock_with_encryption_key(&new_ingress_key).await {
            if let Some(key) = old_egress_key {
                // Key rolling did not complete successfully last time and there's an old
                // egress key in the VMGS. It may be needed for decryption.
                tracing::info!(CVM_ALLOWED, "Old EgressKey found");
                vmgs.unlock_with_encryption_key(&key)
                    .await
                    .map_err(UnlockVmgsDataStoreError::VmgsUnlockUsingExistingEgressKey)?;
            } else {
                Err(UnlockVmgsDataStoreError::VmgsUnlockUsingExistingIngressKey(
                    e,
                ))?
            }
        }
    } else {
        // The datastore is not encrypted which means it's during provision.
        tracing::info!(
            CVM_ALLOWED,
            "vmgs data store is not encrypted, provisioning."
        );
        provision = true;
    }

    tracing::info!(
        CVM_ALLOWED,
        should_write_kp = key_protector_settings.should_write_kp,
        use_gsp_by_id = key_protector_settings.use_gsp_by_id,
        use_hardware_unlock = key_protector_settings.use_hardware_unlock,
        "key protector settings"
    );

    if key_protector_settings.should_write_kp {
        // Update on disk KP with all seeds used, to allow for disaster recovery
        vmgs::write_key_protector(key_protector, vmgs)
            .await
            .map_err(UnlockVmgsDataStoreError::WriteKeyProtector)?;

        if key_protector_settings.use_gsp_by_id {
            vmgs::write_key_protector_by_id(&mut key_protector_by_id.inner, vmgs, false, bios_guid)
                .await
                .map_err(UnlockVmgsDataStoreError::WriteKeyProtectorById)?;
        }
    }

    if provision || new_key {
        // Add the new egress key. If we are not provisioning, then this will
        // also remove the old key. This will also remove the inactive key if
        // last time we failed to remove it.
        vmgs.update_encryption_key(&new_egress_key, EncryptionAlgorithm::AES_GCM)
            .await
            .map_err(UnlockVmgsDataStoreError::UpdateVmgsEncryptionKey)?;
    }

    // Persist KP to VMGS
    persist_all_key_protectors(
        vmgs,
        key_protector,
        key_protector_by_id,
        hardware_key_protector.as_ref(),
        bios_guid,
        key_protector_settings,
    )
    .await
    .map_err(UnlockVmgsDataStoreError::PersistAllKeyProtectors)
}

/// Update data store keys with key protectors.
///         VMGS encryption can come from combinations of three sources,
///         a Tenant Key (KEK), GSP, and GSP By Id.
///         There is an Ingress Key (previously used to lock the VMGS),
///         and an Egress Key (new key for locking the VMGS), and these
///         keys can be derived differently, where KEK is
///         always used if available, and GSP is preferred to GSP By Id.
///         Ingress                     Possible Egress in order of preference [Ingress]
///         - No Encryption             - All
///         - GSP By Id                 - KEK + GSP, KEK + GSP By Id, GSP, [GSP By Id]
///         - GSP (v10 VM and later)    - KEK + GSP, [GSP]
///         - KEK (IVM only)            - KEK + GSP, KEK + GSP By Id, [KEK]
///         - KEK + GSP By Id           - KEK + GSP, [KEK + GSP By Id]
///         - KEK + GSP                 - [KEK + GSP]
///
/// NOTE: for TVM parity, only None, Gsp By Id v9.1, and Gsp By Id / Gsp v10.0 are used.
#[expect(clippy::fn_params_excessive_bools)]
async fn get_derived_keys(
    get: &GuestEmulationTransportClient,
    tee_call: Option<&dyn TeeCall>,
    vmgs: &mut Vmgs,
    key_protector: &mut KeyProtector,
    key_protector_by_id: &mut KeyProtectorById,
    bios_guid: Guid,
    attestation_vm_config: &AttestationVmConfig,
    is_encrypted: bool,
    ingress_rsa_kek: Option<&RsaKeyPair>,
    wrapped_des_key: Option<&[u8]>,
    key_derivation_policy: Option<KeyDerivationPolicy>,
    guest_state_encryption_policy: GuestStateEncryptionPolicy,
    strict_encryption_policy: bool,
    require_hardware_sealing: bool,
    skip_hw_unsealing: bool,
) -> Result<DerivedKeyResult, GetDerivedKeysError> {
    tracing::info!(
        CVM_ALLOWED,
        ?guest_state_encryption_policy,
        strict_encryption_policy,
        "encryption policy"
    );

    let mut key_protector_settings = KeyProtectorSettings {
        should_write_kp: true,
        use_gsp_by_id: false,
        use_hardware_unlock: false,
        decrypt_gsp_type: GspType::None,
        encrypt_gsp_type: GspType::None,
    };

    let mut derived_keys = Keys {
        ingress: [0u8; AES_GCM_KEY_LENGTH],
        decrypt_egress: None,
        encrypt_egress: [0u8; AES_GCM_KEY_LENGTH],
    };

    // Ingress / Egress seed values depend on what happened previously to the datastore
    let ingress_idx = (key_protector.active_kp % 2) as usize;
    let egress_idx = if ingress_idx == 0 { 1 } else { 0 } as usize;

    let found_dek = !key_protector.dek[ingress_idx]
        .dek_buffer
        .iter()
        .all(|&x| x == 0);

    // Handle key released via attestation process (tenant key) to get keys from KeyProtector
    let (ingress_key, mut decrypt_egress_key, encrypt_egress_key, no_kek) =
        if let Some(ingress_kek) = ingress_rsa_kek {
            let keys = match key_protector.unwrap_and_rotate_keys(
                ingress_kek,
                wrapped_des_key,
                ingress_idx,
                egress_idx,
            ) {
                Ok(keys) => keys,
                Err(e)
                    if matches!(
                        e,
                        GetKeysFromKeyProtectorError::DesKeyRsaUnwrap(_)
                            | GetKeysFromKeyProtectorError::IngressDekRsaUnwrap(_)
                    ) =>
                {
                    get.event_log_fatal(
                        guest_emulation_transport::api::EventLogId::DEK_DECRYPTION_FAILED,
                    )
                    .await;

                    return Err(GetDerivedKeysError::GetKeysFromKeyProtector(e));
                }
                Err(e) => return Err(GetDerivedKeysError::GetKeysFromKeyProtector(e)),
            };
            (
                keys.ingress,
                keys.decrypt_egress,
                keys.encrypt_egress,
                false,
            )
        } else {
            (
                [0u8; AES_GCM_KEY_LENGTH],
                None,
                [0u8; AES_GCM_KEY_LENGTH],
                true,
            )
        };

    // Handle various sources of Guest State Protection
    let existing_unencrypted = !vmgs.encrypted() && !vmgs.was_provisioned_this_boot();
    let is_gsp_by_id = key_protector_by_id.found_id && key_protector_by_id.inner.ported != 1;
    let is_gsp = key_protector.gsp[ingress_idx].gsp_length != 0;
    tracing::info!(
        CVM_ALLOWED,
        is_encrypted,
        is_gsp_by_id,
        is_gsp,
        found_dek,
        "initial vmgs encryption state"
    );
    let mut requires_gsp_by_id = is_gsp_by_id;

    // Attempt GSP
    let (gsp_response, gsp_available, no_gsp, requires_gsp) = if require_hardware_sealing {
        // In stateless + hardware sealing mode, the VMGS DEK comes exclusively
        // from hardware sealing, and the TPM seeds must remain stable across
        // boots. Skip the GSP host callout entirely so that a host-provided
        // `state_refresh_request` can never trigger a TPM seed refresh.
        tracing::info!(
            CVM_ALLOWED,
            "Hardware sealing is required, skip GSP callout"
        );
        (GuestStateProtection::new_zeroed(), false, true, false)
    } else {
        tracing::info!(CVM_ALLOWED, "attempting GSP");

        let response = get_gsp_data(get, key_protector).await;

        tracing::info!(
            CVM_ALLOWED,
            request_data_length_in_vmgs = key_protector.gsp[ingress_idx].gsp_length,
            no_rpc_server = response.extended_status_flags.no_rpc_server(),
            requires_rpc_server = response.extended_status_flags.requires_rpc_server(),
            encrypted_gsp_length = response.encrypted_gsp.length,
            "GSP response"
        );

        let no_gsp_available =
            response.extended_status_flags.no_rpc_server() || response.encrypted_gsp.length == 0;

        let no_gsp = no_gsp_available
            // disable if auto and pre-existing guest state is not encrypted or
            // encrypted using GspById to prevent encryption changes without
            // explicit intent
            || (matches!(
                guest_state_encryption_policy,
                GuestStateEncryptionPolicy::Auto
            ) && (is_gsp_by_id || existing_unencrypted))
            // disable per encryption policy (first boot only, unless strict)
            || (matches!(
                guest_state_encryption_policy,
                GuestStateEncryptionPolicy::GspById | GuestStateEncryptionPolicy::None
            ) && (!is_gsp || strict_encryption_policy));

        let requires_gsp = is_gsp
            || response.extended_status_flags.requires_rpc_server()
            || (matches!(
                guest_state_encryption_policy,
                GuestStateEncryptionPolicy::GspKey
            ) && strict_encryption_policy);

        // If the VMGS is encrypted, but no key protection data is found,
        // assume GspById encryption is enabled, but no ID file was written.
        if is_encrypted && !requires_gsp_by_id && !requires_gsp && !found_dek {
            requires_gsp_by_id = true;
        }

        (response, !no_gsp_available, no_gsp, requires_gsp)
    };

    // Attempt GSP By Id protection if GSP is not available, when changing
    // schemes, or as requested. Skipped entirely in hardware sealing mode
    // for the same reason the GSP callout is skipped above.
    let (gsp_response_by_id, gsp_by_id_available, no_gsp_by_id) = if !require_hardware_sealing
        && (no_gsp || requires_gsp_by_id)
    {
        tracing::info!(CVM_ALLOWED, "attempting GSP By Id");

        let gsp_response_by_id = get
            .guest_state_protection_data_by_id()
            .await
            .map_err(GetDerivedKeysError::FetchGuestStateProtectionById)?;

        let no_gsp_by_id_available = gsp_response_by_id.extended_status_flags.no_registry_file();

        let no_gsp_by_id = no_gsp_by_id_available
                // disable if auto and pre-existing guest state is unencrypted
                // to prevent encryption changes without explicit intent
                || (matches!(
                    guest_state_encryption_policy,
                    GuestStateEncryptionPolicy::Auto
                ) && existing_unencrypted)
                // disable per encryption policy (first boot only, unless strict)
                || (matches!(
                    guest_state_encryption_policy,
                    GuestStateEncryptionPolicy::None
                ) && (!requires_gsp_by_id || strict_encryption_policy));

        if no_gsp_by_id && requires_gsp_by_id {
            Err(GetDerivedKeysError::GspByIdRequiredButNotFound)?
        }

        (
            gsp_response_by_id,
            Some(!no_gsp_by_id_available),
            no_gsp_by_id,
        )
    } else {
        (GuestStateProtectionById::new_zeroed(), None, true)
    };

    // If sources of encryption used last are missing, attempt to unseal VMGS key with hardware key
    if (no_kek && found_dek)
        || (no_gsp && requires_gsp)
        || (no_gsp_by_id && requires_gsp_by_id)
        || (require_hardware_sealing && is_encrypted)
    {
        // If possible, get ingressKey from hardware sealed data
        let (hardware_key_protector, hardware_derived_keys) = if let Some(tee_call) = tee_call {
            let hardware_key_protector = match vmgs::read_hardware_key_protector(vmgs).await {
                Ok(hardware_key_protector) => Some(hardware_key_protector),
                Err(e) => {
                    // non-fatal
                    tracing::warn!(
                        CVM_ALLOWED,
                        error = &e as &dyn std::error::Error,
                        "failed to read HW_KEY_PROTECTOR from Vmgs"
                    );
                    None
                }
            };

            let hardware_derived_keys = tee_call.supports_get_derived_key().and_then(|tee_call| {
                if let Some(hardware_key_protector) = &hardware_key_protector {
                    // `key_derivation_policy` returns `None` for formats that
                    // cannot be re-derived by this OpenHCL (v1, which always
                    // mixes the measurement, or an unknown version/tee-type).
                    let Some(policy) = hardware_key_protector.key_derivation_policy() else {
                        tracing::error!(
                            CVM_ALLOWED,
                            version = hardware_key_protector.version(),
                            current_version = HW_KEY_PROTECTOR_CURRENT_VERSION,
                            "incompatible HW_KEY_PROTECTOR; skip VMGS DEK unsealing with hardware key protector."
                        );
                        return None;
                    };

                    match HardwareDerivedKeys::derive_key(tee_call, attestation_vm_config, policy) {
                        Ok(hardware_derived_key) => Some(hardware_derived_key),
                        Err(e) => {
                            // non-fatal
                            tracing::warn!(
                                CVM_ALLOWED,
                                error = &e as &dyn std::error::Error,
                                version = hardware_key_protector.version(),
                                "failed to derive hardware keys using HW_KEY_PROTECTOR",
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            });

            // When the IGVM agent signals skip_hw_unsealing, set both
            // hardware_key_protector and hardware_derived_keys to None
            // so the code falls through to the scheme-specific error below.
            // When hardware sealing keys were actually available, additionally
            // emit a warning and a host event that make the skip visible.
            if skip_hw_unsealing {
                if hardware_key_protector.is_some() && hardware_derived_keys.is_some() {
                    tracing::warn!(
                        CVM_ALLOWED,
                        "Skipping hardware unsealing of VMGS DEK as signaled by IGVM agent"
                    );
                    get.event_log_fatal(
                        guest_emulation_transport::api::EventLogId::DEK_HARDWARE_UNSEALING_SKIPPED,
                    )
                    .await;

                    (None, None)
                } else {
                    tracing::info!(
                        CVM_ALLOWED,
                        hardware_key_protector = hardware_key_protector.is_some(),
                        hardware_derived_keys = hardware_derived_keys.is_some(),
                        "skip_hw_unsealing signaled but hardware key data not available, \
                         falling through to scheme-specific error"
                    );
                    (None, None)
                }
            } else {
                (hardware_key_protector, hardware_derived_keys)
            }
        } else {
            (None, None)
        };

        if let (Some(hardware_key_protector), Some(hardware_derived_keys)) =
            (hardware_key_protector, hardware_derived_keys)
        {
            let dek = match hardware_key_protector.unseal_key(&hardware_derived_keys) {
                Ok(dek) => dek,
                Err(e @ HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed)
                    if require_hardware_sealing =>
                {
                    tracing::error!(
                        CVM_ALLOWED,
                        "hardware unsealing failed due to inconsistent hardware-derived keys"
                    );

                    get.event_log_fatal(
                        guest_emulation_transport::api::EventLogId::DEK_HARDWARE_SEALING_INVALID_KEY,
                    )
                    .await;

                    return Err(GetDerivedKeysError::UnsealIngressKeyUsingHardwareDerivedKeys(e));
                }
                Err(e) => {
                    return Err(GetDerivedKeysError::UnsealIngressKeyUsingHardwareDerivedKeys(e));
                }
            };

            derived_keys.ingress = dek;
            derived_keys.decrypt_egress = None;

            let hardware_key_protector = if require_hardware_sealing && is_encrypted {
                // Generate a new key on every boot for key rotation
                let mut new_dek = [0u8; AES_GCM_KEY_LENGTH];
                getrandom::fill(&mut new_dek).expect("rng failure");

                let updated_hardware_key_protector =
                    hardware_key_sealing::seal_key(&hardware_derived_keys, &new_dek)
                        .map_err(GetDerivedKeysError::SealEgressKeyUsingHardwareDerivedKeys)?;

                derived_keys.encrypt_egress = new_dek;

                tracing::info!(
                    CVM_ALLOWED,
                    "Non-first boot with VMGS hardware sealing mode. Generate a new random key for VMGS DEK rotation."
                );

                // Use the updated key protector in the exclusive hardware sealing scenario
                // to support per-boot key rotation
                updated_hardware_key_protector
            } else {
                derived_keys.encrypt_egress = derived_keys.ingress;

                tracing::warn!(
                    CVM_ALLOWED,
                    "Using hardware-derived key to recover VMGS DEK"
                );

                // Re-seal the recovered DEK as a v3 protector (also migrates a
                // legacy v2 protector to the current format).
                hardware_key_sealing::seal_key(&hardware_derived_keys, &derived_keys.ingress)
                    .map_err(GetDerivedKeysError::SealEgressKeyUsingHardwareDerivedKeys)?
            };

            key_protector_settings.should_write_kp = false;
            key_protector_settings.use_hardware_unlock = true;

            return Ok(DerivedKeyResult {
                derived_keys: Some(derived_keys),
                key_protector_settings,
                gsp_extended_status_flags: gsp_response.extended_status_flags,
                hardware_key_protector: Some(hardware_key_protector),
                hardware_key_protector_written: false,
            });
        } else {
            if require_hardware_sealing && is_encrypted {
                get.event_log_fatal(
                    guest_emulation_transport::api::EventLogId::DEK_HARDWARE_SEALING_FAILED,
                )
                .await;
                return Err(GetDerivedKeysError::GetIngressKeyFromHardwareKeyProtectorFailed);
            } else if no_kek && found_dek {
                return Err(GetDerivedKeysError::GetIngressKeyFromKpFailed);
            } else if no_gsp && requires_gsp {
                return Err(GetDerivedKeysError::GetIngressKeyFromKGspFailed);
            } else {
                // no_gsp_by_id && requires_gsp_by_id
                return Err(GetDerivedKeysError::GetIngressKeyFromKGspByIdFailed);
            }
        }
    }

    tracing::info!(
        CVM_ALLOWED,
        kek = !no_kek,
        gsp_available,
        gsp = !no_gsp,
        gsp_by_id_available = ?gsp_by_id_available,
        gsp_by_id = !no_gsp_by_id,
        hw_sealing = require_hardware_sealing,
        "Encryption sources"
    );

    // Attempt to get hardware derived keys
    let hardware_derived_keys = tee_call
        .and_then(|tee_call| tee_call.supports_get_derived_key())
        .and_then(|tee_call| {
            if let Some(policy) = key_derivation_policy {
                match HardwareDerivedKeys::derive_key(tee_call, attestation_vm_config, policy) {
                    Ok(keys) => Some(keys),
                    Err(e) => {
                        // non-fatal
                        tracing::warn!(
                            CVM_ALLOWED,
                            error = &e as &dyn std::error::Error,
                            "failed to derive hardware keys"
                        );
                        None
                    }
                }
            } else {
                None
            }
        });

    // Let hardware sealing take precedence over other sources if it's required
    if require_hardware_sealing && !is_encrypted {
        let Some(hardware_derived_keys) = hardware_derived_keys else {
            get.event_log_fatal(
                guest_emulation_transport::api::EventLogId::DEK_HARDWARE_SEALING_FAILED,
            )
            .await;
            return Err(GetDerivedKeysError::HardwareSealingRequiredButNotSupported);
        };

        let mut new_dek = [0u8; AES_GCM_KEY_LENGTH];
        getrandom::fill(&mut new_dek).expect("rng failure");

        let hardware_key_protector =
            match hardware_key_sealing::seal_key(&hardware_derived_keys, &new_dek) {
                Ok(hardware_key_protector) => hardware_key_protector,
                Err(e) => {
                    get.event_log_fatal(
                        guest_emulation_transport::api::EventLogId::DEK_HARDWARE_SEALING_FAILED,
                    )
                    .await;
                    return Err(GetDerivedKeysError::SealEgressKeyUsingHardwareDerivedKeys(
                        e,
                    ));
                }
            };

        derived_keys.ingress = [0u8; AES_GCM_KEY_LENGTH];
        derived_keys.decrypt_egress = None;
        derived_keys.encrypt_egress = new_dek;

        tracing::info!(
            CVM_ALLOWED,
            "First boot with VMGS hardware sealing mode. Generate a new random key for VMGS encryption."
        );

        return Ok(DerivedKeyResult {
            derived_keys: Some(derived_keys),
            key_protector_settings,
            gsp_extended_status_flags: gsp_response.extended_status_flags,
            hardware_key_protector: Some(hardware_key_protector),
            hardware_key_protector_written: false,
        });
    }

    // Check if sources of encryption are available
    if no_kek && no_gsp && no_gsp_by_id {
        if is_encrypted {
            Err(GetDerivedKeysError::DisableVmgsEncryptionFailed)?
        }
        match guest_state_encryption_policy {
            // fail if some minimum level of encryption was required
            GuestStateEncryptionPolicy::GspById
            | GuestStateEncryptionPolicy::GspKey
            | GuestStateEncryptionPolicy::HardwareSealing => {
                Err(GetDerivedKeysError::EncryptionRequiredButNotFound)?
            }
            GuestStateEncryptionPolicy::Auto | GuestStateEncryptionPolicy::None => {
                tracing::info!(CVM_ALLOWED, "No VMGS encryption used.");

                return Ok(DerivedKeyResult {
                    derived_keys: None,
                    key_protector_settings,
                    gsp_extended_status_flags: gsp_response.extended_status_flags,
                    hardware_key_protector: None,
                    hardware_key_protector_written: false,
                });
            }
        }
    }

    let mut hardware_key_protector_written = false;

    // Use tenant key (KEK only)
    if no_gsp && no_gsp_by_id {
        tracing::info!(CVM_ALLOWED, "No GSP used with SKR");

        derived_keys.ingress = ingress_key;
        derived_keys.decrypt_egress = decrypt_egress_key;
        derived_keys.encrypt_egress = encrypt_egress_key;

        if let Some(hardware_derived_keys) = hardware_derived_keys {
            let hardware_key_protector = hardware_key_sealing::seal_key(
                &hardware_derived_keys,
                &derived_keys.encrypt_egress,
            )
            .map_err(GetDerivedKeysError::SealEgressKeyUsingHardwareDerivedKeys)?;
            vmgs::write_hardware_key_protector(&hardware_key_protector, vmgs)
                .await
                .map_err(GetDerivedKeysError::VmgsWriteHardwareKeyProtector)?;
            hardware_key_protector_written = true;

            tracing::info!(CVM_ALLOWED, "hardware key protector updated (no GSP used)");
        }

        return Ok(DerivedKeyResult {
            derived_keys: Some(derived_keys),
            key_protector_settings,
            gsp_extended_status_flags: gsp_response.extended_status_flags,
            hardware_key_protector: None,
            hardware_key_protector_written,
        });
    }

    // GSP By Id derives keys differently,
    // because key is shared across VMs different context must be used (Id GUID)
    if (no_kek && no_gsp) || requires_gsp_by_id {
        let derived_keys_by_id =
            get_derived_keys_by_id(key_protector_by_id, bios_guid, gsp_response_by_id)
                .map_err(GetDerivedKeysError::GetDerivedKeyById)?;

        if no_kek && no_gsp {
            if matches!(
                guest_state_encryption_policy,
                GuestStateEncryptionPolicy::GspById | GuestStateEncryptionPolicy::Auto
            ) {
                tracing::info!(CVM_ALLOWED, "Using GspById");
            } else {
                // Log a warning here to indicate that the VMGS state is out of
                // sync with the VM's configuration.
                //
                // This should only happen if strict encryption policy is
                // disabled and one of the following is true:
                // - The VM is configured to have no encryption, but it already
                //   has GspById encryption.
                // - The VM is configured to use GspKey, but GspKey is not
                //   available and GspById is.
                tracing::warn!(CVM_ALLOWED, "Allowing GspById");
            };

            // Not required for Id protection
            key_protector_settings.should_write_kp = false;
            key_protector_settings.use_gsp_by_id = true;
            key_protector_settings.decrypt_gsp_type = GspType::GspById;
            key_protector_settings.encrypt_gsp_type = GspType::GspById;

            return Ok(DerivedKeyResult {
                derived_keys: Some(derived_keys_by_id),
                key_protector_settings,
                gsp_extended_status_flags: gsp_response.extended_status_flags,
                hardware_key_protector: None,
                hardware_key_protector_written: false,
            });
        }

        derived_keys.ingress = derived_keys_by_id.ingress;

        tracing::info!(
            CVM_ALLOWED,
            op_type = ?LogOpType::ConvertEncryptionType,
            "Converting GSP method."
        );
    }

    let egress_seed;
    let mut ingress_seed = None;

    // To get to this point, either KEK or GSP must be available
    // Mix tenant key with GSP key to create data store encryption keys
    // Covers possible egress combinations:
    // GSP, GSP + KEK, GSP By Id + KEK

    if requires_gsp_by_id || no_gsp {
        // If DEK exists, ingress is either KEK or KEK + GSP By Id
        // If no DEK, then ingress was Gsp By Id (derived above)
        if found_dek {
            if requires_gsp_by_id {
                ingress_seed = Some(
                    gsp_response_by_id.seed.buffer[..gsp_response_by_id.seed.length as usize]
                        .to_vec(),
                );
                key_protector_settings.decrypt_gsp_type = GspType::GspById;
            } else {
                derived_keys.ingress = ingress_key;
            }
        } else {
            key_protector_settings.decrypt_gsp_type = GspType::GspById;
        }

        // Choose best available egress seed
        if no_gsp {
            egress_seed =
                gsp_response_by_id.seed.buffer[..gsp_response_by_id.seed.length as usize].to_vec();
            key_protector_settings.use_gsp_by_id = true;
            key_protector_settings.encrypt_gsp_type = GspType::GspById;
        } else {
            egress_seed =
                gsp_response.new_gsp.buffer[..gsp_response.new_gsp.length as usize].to_vec();
            key_protector_settings.encrypt_gsp_type = GspType::GspKey;
        }
    } else {
        // `no_gsp` is false, using `gsp_response`

        if gsp_response.decrypted_gsp[ingress_idx].length == 0
            && gsp_response.decrypted_gsp[egress_idx].length == 0
        {
            tracing::info!(CVM_ALLOWED, "Applying GSP.");

            // VMGS has never had any GSP applied.
            // Leave ingress key untouched, derive egress key with new seed.
            egress_seed =
                gsp_response.new_gsp.buffer[..gsp_response.new_gsp.length as usize].to_vec();

            // Ingress key is either zero or tenant only.
            // Only copy in the case where a tenant key was released.
            if !no_kek {
                derived_keys.ingress = ingress_key;
            }

            key_protector_settings.encrypt_gsp_type = GspType::GspKey;
        } else {
            tracing::info!(CVM_ALLOWED, "Using existing GSP.");

            ingress_seed = Some(
                gsp_response.decrypted_gsp[ingress_idx].buffer
                    [..gsp_response.decrypted_gsp[ingress_idx].length as usize]
                    .to_vec(),
            );

            if gsp_response.decrypted_gsp[egress_idx].length == 0 {
                // Derive ingress with saved seed, derive egress with new seed.
                egress_seed =
                    gsp_response.new_gsp.buffer[..gsp_response.new_gsp.length as usize].to_vec();
            } else {
                // System failed during data store unlock, and is in indeterminate state.
                // The egress key might have been applied, or the ingress key might be valid.
                // Use saved KP, derive ingress/egress keys to attempt recovery.
                // Do not update the saved KP with new seed value.
                egress_seed = gsp_response.decrypted_gsp[egress_idx].buffer
                    [..gsp_response.decrypted_gsp[egress_idx].length as usize]
                    .to_vec();
                key_protector_settings.should_write_kp = false;
                decrypt_egress_key = Some(encrypt_egress_key);
            }

            key_protector_settings.decrypt_gsp_type = GspType::GspKey;
            key_protector_settings.encrypt_gsp_type = GspType::GspKey;
        }
    }

    // Derive key used to lock data store previously
    if let Some(seed) = ingress_seed {
        derived_keys.ingress = derive_key(&ingress_key, &seed, VMGS_KEY_DERIVE_LABEL)
            .map_err(GetDerivedKeysError::DeriveIngressKey)?;
    }

    // Always derive a new egress key using best available seed
    derived_keys.decrypt_egress = decrypt_egress_key
        .map(|key| derive_key(&key, &egress_seed, VMGS_KEY_DERIVE_LABEL))
        .transpose()
        .map_err(GetDerivedKeysError::DeriveEgressKey)?;

    derived_keys.encrypt_egress =
        derive_key(&encrypt_egress_key, &egress_seed, VMGS_KEY_DERIVE_LABEL)
            .map_err(GetDerivedKeysError::DeriveEgressKey)?;

    if key_protector_settings.should_write_kp {
        // Update with all seeds used, but do not write until data store is unlocked
        key_protector.gsp[egress_idx]
            .gsp_buffer
            .copy_from_slice(&gsp_response.encrypted_gsp.buffer);
        key_protector.gsp[egress_idx].gsp_length = gsp_response.encrypted_gsp.length;

        if let Some(hardware_derived_keys) = hardware_derived_keys {
            let hardware_key_protector = hardware_key_sealing::seal_key(
                &hardware_derived_keys,
                &derived_keys.encrypt_egress,
            )
            .map_err(GetDerivedKeysError::SealEgressKeyUsingHardwareDerivedKeys)?;

            vmgs::write_hardware_key_protector(&hardware_key_protector, vmgs)
                .await
                .map_err(GetDerivedKeysError::VmgsWriteHardwareKeyProtector)?;
            hardware_key_protector_written = true;

            tracing::info!(CVM_ALLOWED, "hardware key protector updated");
        }
    }

    if matches!(
        guest_state_encryption_policy,
        GuestStateEncryptionPolicy::GspKey | GuestStateEncryptionPolicy::Auto
    ) {
        tracing::info!(CVM_ALLOWED, "Using Gsp");
    } else {
        // Log a warning here to indicate that the VMGS state is out of
        // sync with the VM's configuration.
        //
        // This should only happen if the VM is configured to have no
        // encryption or GspById encryption, but it already has GspKey
        // encryption and strict encryption policy is disabled.
        tracing::warn!(CVM_ALLOWED, "Allowing Gsp");
    }

    Ok(DerivedKeyResult {
        derived_keys: Some(derived_keys),
        key_protector_settings,
        gsp_extended_status_flags: gsp_response.extended_status_flags,
        hardware_key_protector: None,
        hardware_key_protector_written,
    })
}

/// Update data store keys with key protectors based on VmUniqueId & host seed.
fn get_derived_keys_by_id(
    key_protector_by_id: &mut KeyProtectorById,
    bios_guid: Guid,
    gsp_response_by_id: GuestStateProtectionById,
) -> Result<Keys, GetDerivedKeysByIdError> {
    // This does not handle tenant encrypted VMGS files or Isolated VM,
    // or the case where an unlock/relock fails and a snapshot is
    // made from that file (the Id cannot change in that failure path).
    // When converted to a later scheme, Egress Key will be overwritten.

    // Always derive a new egress key from current VmUniqueId
    let new_egress_key = derive_key(
        &gsp_response_by_id.seed.buffer[..gsp_response_by_id.seed.length as usize],
        bios_guid.as_bytes(),
        VMGS_KEY_DERIVE_LABEL,
    )
    .map_err(GetDerivedKeysByIdError::DeriveEgressKeyUsingCurrentVmId)?;

    if new_egress_key.len() != AES_GCM_KEY_LENGTH {
        Err(GetDerivedKeysByIdError::InvalidDerivedEgressKeySize {
            key_size: new_egress_key.len(),
            expected_size: AES_GCM_KEY_LENGTH,
        })?
    }

    // Ingress values depend on what happened previously to the datastore.
    // If not previously encrypted (no saved Id), then Ingress Key not required.
    let new_ingress_key = if key_protector_by_id.inner.id_guid != Guid::default() {
        // Derive key used to lock data store previously
        derive_key(
            &gsp_response_by_id.seed.buffer[..gsp_response_by_id.seed.length as usize],
            key_protector_by_id.inner.id_guid.as_bytes(),
            VMGS_KEY_DERIVE_LABEL,
        )
        .map_err(GetDerivedKeysByIdError::DeriveIngressKeyUsingKeyProtectorId)?
    } else {
        // If data store is not encrypted, Ingress should equal Egress
        new_egress_key
    };

    if new_ingress_key.len() != AES_GCM_KEY_LENGTH {
        Err(GetDerivedKeysByIdError::InvalidDerivedIngressKeySize {
            key_size: new_ingress_key.len(),
            expected_size: AES_GCM_KEY_LENGTH,
        })?
    }

    Ok(Keys {
        ingress: new_ingress_key,
        decrypt_egress: None,
        encrypt_egress: new_egress_key,
    })
}

/// Prepare the request payload and request GSP from the host via GET.
async fn get_gsp_data(
    get: &GuestEmulationTransportClient,
    key_protector: &mut KeyProtector,
) -> GuestStateProtection {
    use openhcl_attestation_protocol::vmgs::GSP_BUFFER_SIZE;
    use openhcl_attestation_protocol::vmgs::NUMBER_KP;

    const_assert_eq!(guest_emulation_transport::api::NUMBER_GSP, NUMBER_KP as u32);
    const_assert_eq!(
        guest_emulation_transport::api::GSP_CIPHERTEXT_MAX,
        GSP_BUFFER_SIZE as u32
    );

    let mut encrypted_gsp =
        [guest_emulation_transport::api::GspCiphertextContent::new_zeroed(); NUMBER_KP];

    for (i, gsp) in encrypted_gsp.iter_mut().enumerate().take(NUMBER_KP) {
        if key_protector.gsp[i].gsp_length == 0 {
            continue;
        }

        gsp.buffer[..key_protector.gsp[i].gsp_length as usize].copy_from_slice(
            &key_protector.gsp[i].gsp_buffer[..key_protector.gsp[i].gsp_length as usize],
        );

        gsp.length = key_protector.gsp[i].gsp_length;
    }

    get.guest_state_protection_data(encrypted_gsp, GspExtendedStatusFlags::new())
        .await
}

/// Update Key Protector to remove 2nd protector, and write to VMGS
async fn persist_all_key_protectors(
    vmgs: &mut Vmgs,
    key_protector: &mut KeyProtector,
    key_protector_by_id: &mut KeyProtectorById,
    hardware_key_protector: Option<&HardwareKeyProtectorV3>,
    bios_guid: Guid,
    key_protector_settings: KeyProtectorSettings,
) -> Result<(), PersistAllKeyProtectorsError> {
    use openhcl_attestation_protocol::vmgs::NUMBER_KP;

    if key_protector_settings.use_gsp_by_id && !key_protector_settings.should_write_kp {
        vmgs::write_key_protector_by_id(&mut key_protector_by_id.inner, vmgs, false, bios_guid)
            .await
            .map_err(PersistAllKeyProtectorsError::KeyProtectorById)?;
    } else {
        // When a hardware key protector is present, the VMGS DEK is sealed by the
        // hardware-derived key (either recovered via hardware unsealing, or
        // freshly generated/rotated in exclusive hardware sealing mode). In that
        // case persist the hardware key protector instead of altering the regular
        // key protector.
        if let Some(hardware_key_protector) = hardware_key_protector {
            vmgs::write_hardware_key_protector(hardware_key_protector, vmgs)
                .await
                .map_err(PersistAllKeyProtectorsError::HardwareKeyProtector)?;
        } else {
            // Remove ingress KP & DEK, no longer applies to data store
            key_protector.dek[key_protector.active_kp as usize % NUMBER_KP]
                .dek_buffer
                .fill(0);
            key_protector.gsp[key_protector.active_kp as usize % NUMBER_KP].gsp_length = 0;
            key_protector.active_kp += 1;

            vmgs::write_key_protector(key_protector, vmgs)
                .await
                .map_err(PersistAllKeyProtectorsError::KeyProtector)?;
        }

        // Update Id data to indicate this scheme is no longer in use
        if !key_protector_settings.use_gsp_by_id
            && key_protector_by_id.found_id
            && key_protector_by_id.inner.ported == 0
        {
            key_protector_by_id.inner.ported = 1;
            vmgs::write_key_protector_by_id(&mut key_protector_by_id.inner, vmgs, true, bios_guid)
                .await
                .map_err(PersistAllKeyProtectorsError::KeyProtectorById)?;
        }
    }

    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct ProvenanceJwtBody {
    #[serde(rename = "VMGSID")]
    pub vmgsid: String,
}

/// Read the VMGS provenance doc and produce runtime claims
pub fn get_provenance_claims(prov_file: &[u8]) -> Result<VmgsProvisioner, Error> {
    let jwt = JwtHelper::<ProvenanceJwtBody>::from(prov_file)
        .map_err(ProvenanceError::DecodeProvenanceDoc)
        .map_err(AttestationErrorInner::Provenance)?;
    let valid = jwt
        .verify_signature()
        .map_err(ProvenanceError::VerifySignature)
        .map_err(AttestationErrorInner::Provenance)?;

    if !valid {
        return Err(Error(AttestationErrorInner::Provenance(
            ProvenanceError::InvalidSignature,
        )));
    }

    let cert_chain = jwt
        .cert_chain()
        .map_err(ProvenanceError::DecodeProvenanceDoc)
        .map_err(AttestationErrorInner::Provenance)?;
    let leaf = &cert_chain[0];

    let sn = leaf
        .subject_common_name()
        .map_err(ProvenanceError::X509Error)
        .map_err(AttestationErrorInner::Provenance)?
        .ok_or(AttestationErrorInner::Provenance(
            ProvenanceError::MissingLeafCertSubjectName,
        ))?;

    let root = cert_chain.last().ok_or(AttestationErrorInner::Provenance(
        ProvenanceError::InvalidRootCert,
    ))?;
    let digest = sha_256(
        &(root
            .to_der()
            .map_err(ProvenanceError::X509Error)
            .map_err(AttestationErrorInner::Provenance)?),
    );
    let signer = format!(
        "did:x509:0:sha256:{}:subject:{}",
        hex::encode_upper(digest),
        sn
    );
    let vmgsid = jwt.jwt.body.vmgsid;

    Ok(VmgsProvisioner {
        id: Guid::parse(vmgsid.as_bytes())
            .map_err(ProvenanceError::ParseVmgsid)
            .map_err(AttestationErrorInner::Provenance)?,
        signer,
    })
}

/// Derive the expected VMGSID from the encrypted seed data.
pub fn derive_vmgsid(seed_file: &[u8]) -> Result<Guid, Error> {
    let seed_file_str = str::from_utf8(seed_file)
        .map_err(ProvenanceError::InvalidVmgsidData)
        .map_err(AttestationErrorInner::Provenance)?;

    // The seed file has four fields separated by commas, but the fourth field
    // is just the length of the first field. Ignore any fields beyond the first
    // three (so the provisioning service can change the format later without
    // breaking anything).
    let parts = seed_file_str
        .split(',')
        .map(|s| s.trim())
        .collect::<Vec<&str>>();
    if parts.len() < 3 {
        Err(AttestationErrorInner::Provenance(
            ProvenanceError::ParseVmgsidSeedData,
        ))?;
    }

    let seed = hex::decode(parts[0])
        .map_err(ProvenanceError::DecodeVmgsidData)
        .map_err(AttestationErrorInner::Provenance)?;
    let label = hex::decode(parts[1])
        .map_err(ProvenanceError::DecodeVmgsidData)
        .map_err(AttestationErrorInner::Provenance)?;
    let context = hex::decode(parts[2])
        .map_err(ProvenanceError::DecodeVmgsidData)
        .map_err(AttestationErrorInner::Provenance)?;

    let key = crypto::kbkdf::kbkdf_hmac_sha256(&seed, &context, &label, 32)
        .map_err(ProvenanceError::KdfError)
        .map_err(AttestationErrorInner::Provenance)?;

    Ok(Guid::from_slice(&key[0..16].try_into().unwrap()))
}

/// Module that implements the mock [`TeeCall`] for testing purposes
#[cfg(test)]
pub mod test_utils {
    use tee_call::GetAttestationReportResult;
    use tee_call::HW_DERIVED_KEY_LENGTH;
    use tee_call::KeyDerivationPolicy;
    use tee_call::REPORT_DATA_SIZE;
    use tee_call::TeeCall;
    use tee_call::TeeCallGetDerivedKey;
    use tee_call::TeeType;

    /// Mock implementation of [`TeeCall`] with get derived key support for testing purposes
    pub struct MockTeeCall {
        /// Mock measurement data
        pub measurement: [u8; 32],
        /// Mock TCB version returned in attestation reports
        pub tcb_version: u64,
    }

    impl MockTeeCall {
        /// Create a new instance of [`MockTeeCall`].
        pub fn new(measurement: [u8; 32]) -> Self {
            Self {
                measurement,
                tcb_version: 0x1234,
            }
        }

        /// Update the mock measurement data.
        pub fn update_measurement(&mut self, measurement: [u8; 32]) {
            self.measurement = measurement;
        }
    }

    impl TeeCall for MockTeeCall {
        fn get_attestation_report(
            &self,
            report_data: &[u8; REPORT_DATA_SIZE],
        ) -> Result<GetAttestationReportResult, tee_call::Error> {
            let mut report =
                [0x6c; openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE];
            report[..REPORT_DATA_SIZE].copy_from_slice(report_data);

            Ok(GetAttestationReportResult {
                report: report.to_vec(),
                key_derivation_svn: Some(tee_call::KeyDerivationSvn::Snp {
                    tcb_version: self.tcb_version,
                }),
            })
        }

        fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
            Some(self)
        }

        fn tee_type(&self) -> TeeType {
            // Use Snp for testing
            TeeType::Snp
        }
    }

    impl TeeCallGetDerivedKey for MockTeeCall {
        fn get_derived_key(
            &self,
            policy: KeyDerivationPolicy,
        ) -> Result<[u8; 32], tee_call::Error> {
            // Base test key; mix in policy so different policies yield different derived secrets
            let mut key: [u8; HW_DERIVED_KEY_LENGTH] = [0xab; HW_DERIVED_KEY_LENGTH];

            // Mock is SNP; mix in the recorded TCB version.
            let tcb_version = match policy.svn {
                tee_call::KeyDerivationSvn::Snp { tcb_version } => tcb_version,
                tee_call::KeyDerivationSvn::Tdx { .. } => 0,
            };
            let tcb = tcb_version.to_le_bytes();
            for (i, b) in key.iter_mut().enumerate() {
                if policy.mix_measurement {
                    *b ^= self.measurement[i] ^ tcb[i % tcb.len()];
                } else {
                    *b ^= tcb[i % tcb.len()];
                }
            }

            Ok(key)
        }
    }

    /// Mock implementation of [`TeeCall`] without get derived key support for testing purposes
    pub struct MockTeeCallNoGetDerivedKey;

    impl TeeCall for MockTeeCallNoGetDerivedKey {
        fn get_attestation_report(
            &self,
            report_data: &[u8; REPORT_DATA_SIZE],
        ) -> Result<GetAttestationReportResult, tee_call::Error> {
            let mut report =
                [0x6c; openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE];
            report[..REPORT_DATA_SIZE].copy_from_slice(report_data);

            Ok(GetAttestationReportResult {
                report: report.to_vec(),
                key_derivation_svn: None,
            })
        }

        fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
            None
        }

        fn tee_type(&self) -> TeeType {
            // Use Snp for testing
            TeeType::Snp
        }
    }
}

#[cfg(test)]
mod tests {
    mod flush_fault;

    use super::*;
    use crate::test_utils::MockTeeCallNoGetDerivedKey;
    use disk_backend::Disk;
    use disklayer_ram::ram_disk;
    use get_protocol::GSP_CLEARTEXT_MAX;
    use get_protocol::GspExtendedStatusFlags;
    use guest_emulation_device::IgvmAgentAction;
    use guest_emulation_device::IgvmAgentTestPlan;
    use guest_emulation_device::test_utilities::Event;
    use guest_emulation_device::test_utilities::TestGetResponses;
    use guest_emulation_transport::test_utilities::TestGet;
    use key_protector::AES_WRAPPED_AES_KEY_LENGTH;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
    use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationTpmVersion;
    use openhcl_attestation_protocol::vmgs::DEK_BUFFER_SIZE;
    use openhcl_attestation_protocol::vmgs::DekKp;
    use openhcl_attestation_protocol::vmgs::GSP_BUFFER_SIZE;
    use openhcl_attestation_protocol::vmgs::GspKp;
    use openhcl_attestation_protocol::vmgs::NUMBER_KP;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use std::collections::VecDeque;
    use test_utils::MockTeeCall;
    use test_with_tracing::test;
    use vmgs_format::EncryptionAlgorithm;
    use vmgs_format::FileId;

    const ONE_MEGA_BYTE: u64 = 1024 * 1024;

    fn test_attestation_config() -> AttestationVmConfig {
        AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: String::new(),
            console_enabled: false,
            interactive_console_enabled: false,
            ipmi_enabled: false,
            secure_boot: false,
            tpm_enabled: true,
            tpm_version: AttestationTpmVersion::V138,
            tpm_persisted: true,
            filtered_vpci_devices_allowed: false,
            vm_unique_id: String::new(),
            vmgs_provisioner: None,
            hardware_sealing_policy: HardwareSealingPolicy::None,
        }
    }

    /// Models a trusted local SNP report with real ABI offsets, independently
    /// of the deliberately opaque reports used by the older SKR fixtures.
    struct BootReportTee {
        inner: MockTeeCall,
        report_data: parking_lot::Mutex<Vec<[u8; REPORT_DATA_SIZE]>>,
        malformed: bool,
        fail_derivation: bool,
        derivation_calls: parking_lot::Mutex<usize>,
    }

    impl BootReportTee {
        fn new() -> Self {
            Self {
                inner: MockTeeCall::new([0x12; 32]),
                report_data: parking_lot::Mutex::new(Vec::new()),
                malformed: false,
                fail_derivation: false,
                derivation_calls: parking_lot::Mutex::new(0),
            }
        }
    }

    impl TeeCall for BootReportTee {
        fn get_attestation_report(
            &self,
            report_data: &[u8; REPORT_DATA_SIZE],
        ) -> Result<tee_call::GetAttestationReportResult, tee_call::Error> {
            self.report_data.lock().push(*report_data);
            let mut report = vec![0; 1184];
            report[..4].copy_from_slice(&3u32.to_le_bytes());
            report[0x50..0x50 + REPORT_DATA_SIZE].copy_from_slice(report_data);
            report[0x180..0x188].copy_from_slice(&self.inner.tcb_version.to_le_bytes());
            report[0x188] = 0x19;
            if self.malformed {
                report.truncate(1183);
            }
            Ok(tee_call::GetAttestationReportResult {
                report,
                key_derivation_svn: Some(tee_call::KeyDerivationSvn::Snp {
                    tcb_version: self.inner.tcb_version,
                }),
            })
        }

        fn supports_get_derived_key(&self) -> Option<&dyn tee_call::TeeCallGetDerivedKey> {
            Some(self)
        }

        fn tee_type(&self) -> TeeType {
            TeeType::Snp
        }
    }

    impl tee_call::TeeCallGetDerivedKey for BootReportTee {
        fn get_derived_key(
            &self,
            policy: KeyDerivationPolicy,
        ) -> Result<[u8; 32], tee_call::Error> {
            *self.derivation_calls.lock() += 1;
            if self.fail_derivation {
                return Err(tee_call::Error::AllZeroKey);
            }
            self.inner
                .supports_get_derived_key()
                .unwrap()
                .get_derived_key(policy)
        }
    }

    /// Models LM at report return, before the first hardware derivation. The
    /// source supplies the trusted report; all subsequent derivations run on
    /// the destination. This is an ordering model, not a real migration.
    struct FirstDerivationMigrationTee {
        source: BootReportTee,
        destination: BootReportTee,
        derivation_policies: parking_lot::Mutex<Vec<KeyDerivationPolicy>>,
    }

    impl FirstDerivationMigrationTee {
        fn new() -> Self {
            let mut destination = BootReportTee::new();
            // MockTeeCall mixes this context into the actual derived bytes.
            // Use it to model a different hardware secret, not a change to the
            // guest image or attested VM configuration during LM.
            destination.inner.update_measurement([0x34; 32]);
            Self {
                source: BootReportTee::new(),
                destination,
                derivation_policies: parking_lot::Mutex::new(Vec::new()),
            }
        }
    }

    impl TeeCall for FirstDerivationMigrationTee {
        fn get_attestation_report(
            &self,
            report_data: &[u8; REPORT_DATA_SIZE],
        ) -> Result<tee_call::GetAttestationReportResult, tee_call::Error> {
            assert!(
                self.source.report_data.lock().is_empty(),
                "extra boot report"
            );
            assert!(self.derivation_policies.lock().is_empty());
            assert_eq!(*self.source.derivation_calls.lock(), 0);
            assert_eq!(*self.destination.derivation_calls.lock(), 0);
            self.source.get_attestation_report(report_data)
        }

        fn supports_get_derived_key(&self) -> Option<&dyn tee_call::TeeCallGetDerivedKey> {
            Some(self)
        }

        fn tee_type(&self) -> TeeType {
            TeeType::Snp
        }
    }

    impl tee_call::TeeCallGetDerivedKey for FirstDerivationMigrationTee {
        fn get_derived_key(
            &self,
            policy: KeyDerivationPolicy,
        ) -> Result<[u8; 32], tee_call::Error> {
            assert_eq!(&*self.source.report_data.lock(), &[[0; REPORT_DATA_SIZE]]);
            assert!(self.destination.report_data.lock().is_empty());
            self.derivation_policies.lock().push(policy);
            // This destination fixture supports only its own SVN. Inject a
            // TEE error for the unsupported source SVN; MockTeeCall otherwise
            // accepts arbitrary requested SVNs without checking its local TCB.
            if !matches!(policy.svn, tee_call::KeyDerivationSvn::Snp { tcb_version }
                if tcb_version == self.destination.inner.tcb_version)
            {
                return Err(tee_call::Error::AllZeroKey);
            }
            self.destination
                .supports_get_derived_key()
                .unwrap()
                .get_derived_key(policy)
        }
    }

    fn new_test_file() -> Disk {
        ram_disk(4 * ONE_MEGA_BYTE, false).unwrap()
    }

    /// Supplies exactly one trusted report per unlock attempt, while retaining
    /// the existing mock's hardware-key derivation and claims-hash recording.
    struct SequencedBootReportTee {
        inner: BootReportTee,
        report_svns: parking_lot::Mutex<VecDeque<u64>>,
    }

    impl TeeCall for SequencedBootReportTee {
        fn get_attestation_report(
            &self,
            report_data: &[u8; REPORT_DATA_SIZE],
        ) -> Result<tee_call::GetAttestationReportResult, tee_call::Error> {
            let tcb_version = self
                .report_svns
                .lock()
                .pop_front()
                .expect("unexpected extra boot report acquisition");
            let mut result = self.inner.get_attestation_report(report_data)?;
            result.report[0x180..0x188].copy_from_slice(&tcb_version.to_le_bytes());
            result.key_derivation_svn = Some(tee_call::KeyDerivationSvn::Snp { tcb_version });
            Ok(result)
        }

        fn supports_get_derived_key(&self) -> Option<&dyn tee_call::TeeCallGetDerivedKey> {
            self.inner.supports_get_derived_key()
        }

        fn tee_type(&self) -> TeeType {
            self.inner.tee_type()
        }
    }

    async fn new_formatted_vmgs() -> Vmgs {
        let disk = new_test_file();

        let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();

        assert!(
            key_protector_is_empty(&mut vmgs).await,
            "Newly formatted VMGS should have an empty key protector"
        );
        assert!(
            key_protector_by_id_is_empty(&mut vmgs).await,
            "Newly formatted VMGS should have an empty key protector by id"
        );

        vmgs
    }

    async fn key_protector_is_empty(vmgs: &mut Vmgs) -> bool {
        let key_protector = vmgs::read_key_protector(vmgs, AES_WRAPPED_AES_KEY_LENGTH)
            .await
            .unwrap();

        key_protector.as_bytes().iter().all(|&b| b == 0)
    }

    async fn key_protector_by_id_is_empty(vmgs: &mut Vmgs) -> bool {
        vmgs::read_key_protector_by_id(vmgs)
            .await
            .is_err_and(|err| {
                matches!(
                    err,
                    vmgs::ReadFromVmgsError::EntryNotFound(FileId::VM_UNIQUE_ID)
                )
            })
    }

    async fn hardware_key_protector_is_empty(vmgs: &mut Vmgs) -> bool {
        vmgs::read_hardware_key_protector(vmgs)
            .await
            .is_err_and(|err| {
                matches!(
                    err,
                    vmgs::ReadFromVmgsError::EntryNotFound(FileId::HW_KEY_PROTECTOR)
                )
            })
    }

    fn new_key_protector() -> KeyProtector {
        // Ingress and egress KPs are assumed to be the only two KPs, therefore `NUMBER_KP` should be 2
        assert_eq!(NUMBER_KP, 2);

        let ingress_dek = DekKp {
            dek_buffer: [1; DEK_BUFFER_SIZE],
        };
        let egress_dek = DekKp {
            dek_buffer: [2; DEK_BUFFER_SIZE],
        };
        let ingress_gsp = GspKp {
            gsp_length: GSP_BUFFER_SIZE as u32,
            gsp_buffer: [3; GSP_BUFFER_SIZE],
        };
        let egress_gsp = GspKp {
            gsp_length: GSP_BUFFER_SIZE as u32,
            gsp_buffer: [4; GSP_BUFFER_SIZE],
        };
        KeyProtector {
            dek: [ingress_dek, egress_dek],
            gsp: [ingress_gsp, egress_gsp],
            active_kp: 0,
        }
    }

    fn new_key_protector_by_id(
        id_guid: Option<Guid>,
        ported: Option<u8>,
        found_id: bool,
    ) -> KeyProtectorById {
        let key_protector_by_id = openhcl_attestation_protocol::vmgs::KeyProtectorById {
            id_guid: id_guid.unwrap_or_else(Guid::new_random),
            ported: ported.unwrap_or(0),
            pad: [0; 3],
        };

        KeyProtectorById {
            inner: key_protector_by_id,
            found_id,
        }
    }

    async fn new_test_get(
        spawn: impl Spawn,
        enable_igvm_attest: bool,
        plan: Option<IgvmAgentTestPlan>,
    ) -> TestGet {
        if enable_igvm_attest {
            const TEST_DEVICE_MEMORY_SIZE: u64 = 64;
            // Use `DeviceTestMemory` to set up shared memory required by the IGVM_ATTEST GET calls.
            let dev_test_mem = user_driver_emulated_mock::DeviceTestMemory::new(
                TEST_DEVICE_MEMORY_SIZE,
                true,
                "test-attest",
            );

            let mut test_get = guest_emulation_transport::test_utilities::new_transport_pair(
                spawn,
                None,
                get_protocol::ProtocolVersion::NICKEL_REV2,
                Some(dev_test_mem.guest_memory()),
                plan,
            )
            .await;

            test_get.client.set_gpa_allocator(dev_test_mem.dma_client());

            test_get
        } else {
            guest_emulation_transport::test_utilities::new_transport_pair(
                spawn,
                None,
                get_protocol::ProtocolVersion::NICKEL_REV2,
                None,
                None,
            )
            .await
        }
    }

    fn new_attestation_vm_config() -> AttestationVmConfig {
        AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: String::new(),
            console_enabled: false,
            interactive_console_enabled: false,
            ipmi_enabled: false,
            secure_boot: false,
            tpm_enabled: true,
            tpm_version: AttestationTpmVersion::V138,
            tpm_persisted: true,
            hardware_sealing_policy: HardwareSealingPolicy::None,
            filtered_vpci_devices_allowed: false,
            vm_unique_id: String::new(),
            vmgs_provisioner: None,
        }
    }

    #[async_test]
    async fn do_nothing_without_derived_keys() {
        let mut vmgs = new_formatted_vmgs().await;

        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: false,
            use_gsp_by_id: false,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::None,
            encrypt_gsp_type: GspType::None,
        };

        let bios_guid = Guid::new_random();

        unlock_vmgs_data_store(
            &mut vmgs,
            false,
            &mut key_protector,
            &mut key_protector_by_id,
            None,
            None,
            key_protector_settings,
            bios_guid,
        )
        .await
        .unwrap();

        assert!(key_protector_is_empty(&mut vmgs).await);
        assert!(key_protector_by_id_is_empty(&mut vmgs).await);

        // Create another instance as the previous `unlock_vmgs_data_store` took ownership of the last one
        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: false,
            use_gsp_by_id: false,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::None,
            encrypt_gsp_type: GspType::None,
        };

        // Even if the VMGS is encrypted, if no derived keys are provided, nothing should happen
        unlock_vmgs_data_store(
            &mut vmgs,
            true,
            &mut key_protector,
            &mut key_protector_by_id,
            None,
            None,
            key_protector_settings,
            bios_guid,
        )
        .await
        .unwrap();

        assert!(key_protector_is_empty(&mut vmgs).await);
        assert!(key_protector_by_id_is_empty(&mut vmgs).await);
    }

    #[async_test]
    async fn provision_vmgs_and_rotate_keys() {
        let mut vmgs = new_formatted_vmgs().await;

        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

        let ingress = [1; AES_GCM_KEY_LENGTH];
        let egress = [2; AES_GCM_KEY_LENGTH];
        let derived_keys = Keys {
            ingress,
            decrypt_egress: None,
            encrypt_egress: egress,
        };

        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: true,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::GspById,
            encrypt_gsp_type: GspType::GspById,
        };

        let bios_guid = Guid::new_random();

        // Without encryption implies the provision path
        // The VMGS will be locked using the egress key
        unlock_vmgs_data_store(
            &mut vmgs,
            false,
            &mut key_protector,
            &mut key_protector_by_id,
            None,
            Some(derived_keys),
            key_protector_settings,
            bios_guid,
        )
        .await
        .unwrap();

        // The ingress key is essentially ignored since the VMGS wasn't previously encrypted
        vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();

        // The egress key was used to lock the VMGS after provisioning
        vmgs.unlock_with_encryption_key(&egress).await.unwrap();
        // Since this is a new VMGS, the egress key is the first and only key
        assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(0));

        // Since both `should_write_kp` and `use_gsp_by_id` are true, both key protectors should be updated
        assert!(!key_protector_is_empty(&mut vmgs).await);
        assert!(!key_protector_by_id_is_empty(&mut vmgs).await);

        let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
            .await
            .unwrap();
        assert_eq!(found_key_protector.as_bytes(), key_protector.as_bytes());

        let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
        assert_eq!(
            found_key_protector_by_id.as_bytes(),
            key_protector_by_id.inner.as_bytes()
        );

        // Now that the VMGS has been provisioned, simulate the rotation of keys
        let new_egress = [3; AES_GCM_KEY_LENGTH];

        let mut new_key_protector = new_key_protector();
        let mut new_key_protector_by_id = new_key_protector_by_id(None, None, false);

        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: true,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::GspById,
            encrypt_gsp_type: GspType::GspById,
        };

        // Ingress is now the old egress, and we provide a new new egress key
        let derived_keys = Keys {
            ingress: egress,
            decrypt_egress: None,
            encrypt_egress: new_egress,
        };

        unlock_vmgs_data_store(
            &mut vmgs,
            true,
            &mut new_key_protector,
            &mut new_key_protector_by_id,
            None,
            Some(derived_keys),
            key_protector_settings,
            bios_guid,
        )
        .await
        .unwrap();

        // We should still fail to unlock the VMGS with the original ingress key
        vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();
        // The old egress key should no longer be able to unlock the VMGS
        vmgs.unlock_with_encryption_key(&egress).await.unwrap_err();

        // The new egress key should be able to unlock the VMGS
        vmgs.unlock_with_encryption_key(&new_egress).await.unwrap();
        // The old egress key was removed, but not before the new egress key was added in the 1th slot
        assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

        let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
            .await
            .unwrap();
        assert_eq!(found_key_protector.as_bytes(), new_key_protector.as_bytes());

        let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
        assert_eq!(
            found_key_protector_by_id.as_bytes(),
            new_key_protector_by_id.inner.as_bytes()
        );
    }

    #[async_test]
    async fn unlock_previously_encrypted_vmgs_with_ingress_key() {
        let mut vmgs = new_formatted_vmgs().await;

        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

        let ingress = [1; AES_GCM_KEY_LENGTH];
        let egress = [2; AES_GCM_KEY_LENGTH];

        let derived_keys = Keys {
            ingress,
            decrypt_egress: None,
            encrypt_egress: egress,
        };

        vmgs.update_encryption_key(&ingress, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();

        // Initially, the VMGS can be unlocked using the ingress key
        vmgs.unlock_with_encryption_key(&ingress).await.unwrap();
        assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(0));

        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: true,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::GspById,
            encrypt_gsp_type: GspType::GspById,
        };

        let bios_guid = Guid::new_random();

        unlock_vmgs_data_store(
            &mut vmgs,
            true,
            &mut key_protector,
            &mut key_protector_by_id,
            None,
            Some(derived_keys),
            key_protector_settings,
            bios_guid,
        )
        .await
        .unwrap();

        // After the VMGS has been unlocked, the VMGS encryption key should be rotated from ingress to egress
        vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();
        vmgs.unlock_with_encryption_key(&egress).await.unwrap();
        // The ingress key was removed, but not before the egress key was added in the 0th slot
        assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

        // Since both `should_write_kp` and `use_gsp_by_id` are true, both key protectors should be updated
        let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
            .await
            .unwrap();
        assert_eq!(found_key_protector.as_bytes(), key_protector.as_bytes());

        let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
        assert_eq!(
            found_key_protector_by_id.as_bytes(),
            key_protector_by_id.inner.as_bytes()
        );
    }

    #[async_test]
    async fn failed_to_persist_ingress_key_so_use_egress_key_to_unlock_vmgs() {
        let mut vmgs = new_formatted_vmgs().await;

        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

        let ingress = [1; AES_GCM_KEY_LENGTH];
        let decrypt_egress = [2; AES_GCM_KEY_LENGTH];
        let encrypt_egress = [3; AES_GCM_KEY_LENGTH];

        let derived_keys = Keys {
            ingress,
            decrypt_egress: Some(decrypt_egress),
            encrypt_egress,
        };

        // Add only the egress key to the VMGS to simulate a failure to persist the ingress key
        vmgs.test_add_new_encryption_key(&decrypt_egress, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        let egress_key_index = vmgs.test_get_active_datastore_key_index().unwrap();
        assert_eq!(egress_key_index, 0);

        vmgs.unlock_with_encryption_key(&decrypt_egress)
            .await
            .unwrap();
        let found_egress_key_index = vmgs.test_get_active_datastore_key_index().unwrap();
        assert_eq!(found_egress_key_index, egress_key_index);

        // Confirm that the ingress key cannot be used to unlock the VMGS
        vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();

        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: true,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::GspById,
            encrypt_gsp_type: GspType::GspById,
        };

        let bios_guid = Guid::new_random();

        unlock_vmgs_data_store(
            &mut vmgs,
            true,
            &mut key_protector,
            &mut key_protector_by_id,
            None,
            Some(derived_keys),
            key_protector_settings,
            bios_guid,
        )
        .await
        .unwrap();

        // Confirm that the ingress key was not added
        vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();

        // Confirm that the decrypt egress key no longer works
        vmgs.unlock_with_encryption_key(&decrypt_egress)
            .await
            .unwrap_err();

        // The encrypt_egress key can unlock the VMGS and was added as a new key
        vmgs.unlock_with_encryption_key(&encrypt_egress)
            .await
            .unwrap();
        assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

        // Since both `should_write_kp` and `use_gsp_by_id` are true, both key protectors should be updated
        let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
            .await
            .unwrap();
        assert_eq!(found_key_protector.as_bytes(), key_protector.as_bytes());

        let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
        assert_eq!(
            found_key_protector_by_id.as_bytes(),
            key_protector_by_id.inner.as_bytes()
        );
    }

    #[async_test]
    async fn fail_to_unlock_vmgs_with_existing_ingress_key() {
        let mut vmgs = new_formatted_vmgs().await;

        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

        let ingress = [1; AES_GCM_KEY_LENGTH];

        // Ingress and egress keys are the same
        let derived_keys = Keys {
            ingress,
            decrypt_egress: None,
            encrypt_egress: ingress,
        };

        // Add two random keys to the VMGS to simulate unlock failure when ingress and egress keys are the same
        let additional_key = [2; AES_GCM_KEY_LENGTH];
        let yet_another_key = [3; AES_GCM_KEY_LENGTH];

        vmgs.test_add_new_encryption_key(&additional_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(0));

        vmgs.test_add_new_encryption_key(&yet_another_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: true,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::GspById,
            encrypt_gsp_type: GspType::GspById,
        };

        let bios_guid = Guid::new_random();

        let unlock_result = unlock_vmgs_data_store(
            &mut vmgs,
            true,
            &mut key_protector,
            &mut key_protector_by_id,
            None,
            Some(derived_keys),
            key_protector_settings,
            bios_guid,
        )
        .await;
        assert!(unlock_result.is_err());
        assert_eq!(
            unlock_result.unwrap_err().to_string(),
            "failed to unlock vmgs with the existing ingress key".to_string()
        );
    }

    #[async_test]
    async fn fail_to_unlock_vmgs_with_new_ingress_key() {
        let mut vmgs = new_formatted_vmgs().await;

        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

        let derived_keys = Keys {
            ingress: [1; AES_GCM_KEY_LENGTH],
            decrypt_egress: None,
            encrypt_egress: [2; AES_GCM_KEY_LENGTH],
        };

        // Add two random keys to the VMGS to simulate unlock failure when ingress and egress keys are *not* the same
        let additional_key = [3; AES_GCM_KEY_LENGTH];
        let yet_another_key = [4; AES_GCM_KEY_LENGTH];

        vmgs.test_add_new_encryption_key(&additional_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(0));

        vmgs.test_add_new_encryption_key(&yet_another_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: true,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::GspById,
            encrypt_gsp_type: GspType::GspById,
        };

        let bios_guid = Guid::new_random();

        let unlock_result = unlock_vmgs_data_store(
            &mut vmgs,
            true,
            &mut key_protector,
            &mut key_protector_by_id,
            None,
            Some(derived_keys),
            key_protector_settings,
            bios_guid,
        )
        .await;
        assert!(unlock_result.is_err());
        assert_eq!(
            unlock_result.unwrap_err().to_string(),
            "failed to unlock vmgs with the existing ingress key".to_string()
        );
    }

    #[async_test]
    async fn get_derived_keys_using_id() {
        let bios_guid = Guid::new_random();

        let gsp_response_by_id = GuestStateProtectionById {
            seed: guest_emulation_transport::api::GspCleartextContent {
                length: GSP_CLEARTEXT_MAX,
                buffer: [1; GSP_CLEARTEXT_MAX as usize * 2],
            },
            extended_status_flags: GspExtendedStatusFlags::from_bits(0),
        };

        // When the key protector by id inner `id_guid` is all zeroes, the derived ingress and egress keys
        // should be identical.
        let mut key_protector_by_id =
            new_key_protector_by_id(Some(Guid::new_zeroed()), None, false);
        let derived_keys =
            get_derived_keys_by_id(&mut key_protector_by_id, bios_guid, gsp_response_by_id)
                .unwrap();

        assert_eq!(derived_keys.ingress, derived_keys.encrypt_egress);

        // When the key protector by id inner `id_guid` is not all zeroes, the derived ingress and egress keys
        // should be different.
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
        let derived_keys =
            get_derived_keys_by_id(&mut key_protector_by_id, bios_guid, gsp_response_by_id)
                .unwrap();

        assert_ne!(derived_keys.ingress, derived_keys.encrypt_egress);

        // When the `gsp_response_by_id` seed length is 0, deriving a key will fail.
        let gsp_response_by_id_with_0_length_seed = GuestStateProtectionById {
            seed: guest_emulation_transport::api::GspCleartextContent {
                length: 0,
                buffer: [1; GSP_CLEARTEXT_MAX as usize * 2],
            },
            extended_status_flags: GspExtendedStatusFlags::from_bits(0),
        };

        let derived_keys_response = get_derived_keys_by_id(
            &mut key_protector_by_id,
            bios_guid,
            gsp_response_by_id_with_0_length_seed,
        );
        assert!(derived_keys_response.is_err());
        assert_eq!(
            derived_keys_response.unwrap_err().to_string(),
            "failed to derive an egress key based on current vm bios guid".to_string()
        );
    }

    #[async_test]
    async fn pass_through_persist_all_key_protectors() {
        let mut vmgs = new_formatted_vmgs().await;
        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
        let bios_guid = Guid::new_random();

        // Copied/cloned bits used for comparison later
        let kp_copy = key_protector.as_bytes().to_vec();
        let active_kp_copy = key_protector.active_kp;

        // When all key protector settings are true, no actions will be taken on the key protectors or VMGS
        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: true,
            use_hardware_unlock: true,
            decrypt_gsp_type: GspType::GspById,
            encrypt_gsp_type: GspType::GspById,
        };
        persist_all_key_protectors(
            &mut vmgs,
            &mut key_protector,
            &mut key_protector_by_id,
            Some(&HardwareKeyProtectorV3::new_zeroed()),
            bios_guid,
            key_protector_settings,
        )
        .await
        .unwrap();

        assert!(key_protector_is_empty(&mut vmgs).await);
        assert!(key_protector_by_id_is_empty(&mut vmgs).await);

        // The key protector should remain unchanged
        assert_eq!(active_kp_copy, key_protector.active_kp);
        assert_eq!(kp_copy.as_slice(), key_protector.as_bytes());
    }

    #[async_test]
    async fn hardware_sealing_first_boot_creates_hwkp_and_encrypts_vmgs(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;
        // Start with an empty KP to simulate brand-new VMGS with no DEK/GSP present
        let mut key_protector = KeyProtector::new_zeroed();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
        let bios_guid = Guid::new_random();

        // Create a GET client backed by the test host
        let get_pair = guest_emulation_transport::test_utilities::new_transport_pair(
            driver,
            None,
            get_protocol::ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let mock_tee_call = MockTeeCall::new([0x8a; 32]);

        // No KEK, no GSP. Require HardwareSealing and VMGS is not encrypted.
        let derived = get_derived_keys(
            &get_pair.client,
            Some(&mock_tee_call),
            &mut vmgs,
            &mut key_protector,
            &mut key_protector_by_id,
            bios_guid,
            &AttestationVmConfig {
                current_time: None,
                root_cert_thumbprint: String::new(),
                console_enabled: false,
                interactive_console_enabled: false,
                ipmi_enabled: false,
                secure_boot: false,
                tpm_enabled: false,
                tpm_version: AttestationTpmVersion::V138,
                tpm_persisted: false,
                hardware_sealing_policy: HardwareSealingPolicy::Hash,
                filtered_vpci_devices_allowed: true,
                vm_unique_id: String::new(),
                vmgs_provisioner: None,
            },
            false,
            None,
            None,
            Some(KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0x1234,
                },
                mix_measurement: true,
            }),
            GuestStateEncryptionPolicy::HardwareSealing,
            true,
            true,
            false,
        )
        .await
        .unwrap();

        // It must produce an egress key and HWKP
        assert!(derived.derived_keys.is_some());
        assert!(derived.hardware_key_protector.is_some());

        // Capture the egress key before `derived.derived_keys` is consumed below
        let egress_key = derived.derived_keys.as_ref().unwrap().encrypt_egress;

        // Apply to VMGS and verify encryption using egress key
        unlock_vmgs_data_store(
            &mut vmgs,
            false,
            &mut key_protector,
            &mut key_protector_by_id,
            derived.hardware_key_protector,
            derived.derived_keys,
            derived.key_protector_settings,
            bios_guid,
        )
        .await
        .unwrap();

        // VMGS should be unlockable with the egress key
        vmgs.unlock_with_encryption_key(&egress_key).await.unwrap();

        // VMGS should not be unlockable with an all-zero key (ingress was zeroed)
        vmgs.unlock_with_encryption_key(&[0; AES_GCM_KEY_LENGTH])
            .await
            .unwrap_err();
    }

    #[async_test]
    async fn hardware_sealing_recovery_uses_hwkp_v2_when_encrypted(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;

        // Pre-encrypt VMGS to simulate previous boot
        let bootstrap = [0x33; AES_GCM_KEY_LENGTH];
        vmgs.test_add_new_encryption_key(&bootstrap, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();

        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
        let bios_guid = Guid::new_random();

        // Create a HWKP V2 by sealing current key and writing to VMGS
        let mock_tee_call = MockTeeCall::new([0x8a; 32]);

        let hdk = HardwareDerivedKeys::derive_key(
            mock_tee_call.supports_get_derived_key().unwrap(),
            &AttestationVmConfig {
                current_time: None,
                root_cert_thumbprint: String::new(),
                console_enabled: false,
                interactive_console_enabled: false,
                ipmi_enabled: false,
                secure_boot: false,
                tpm_enabled: false,
                tpm_version: AttestationTpmVersion::V138,
                tpm_persisted: false,
                hardware_sealing_policy: HardwareSealingPolicy::Hash,
                filtered_vpci_devices_allowed: true,
                vm_unique_id: String::new(),
                vmgs_provisioner: None,
            },
            KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0x1234,
                },
                mix_measurement: true,
            },
        )
        .unwrap();
        let hwkp = hardware_key_sealing::seal_key(&hdk, &bootstrap).unwrap();
        vmgs::write_hardware_key_protector(&hwkp, &mut vmgs)
            .await
            .unwrap();

        // Now call get_derived_keys with HardwareSealing required and VMGS encrypted
        // Create a GET client backed by the test host
        let get_pair = guest_emulation_transport::test_utilities::new_transport_pair(
            driver,
            None,
            get_protocol::ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let derived = get_derived_keys(
            &get_pair.client,
            Some(&mock_tee_call),
            &mut vmgs,
            &mut key_protector,
            &mut key_protector_by_id,
            bios_guid,
            &AttestationVmConfig {
                current_time: None,
                root_cert_thumbprint: String::new(),
                console_enabled: false,
                interactive_console_enabled: false,
                ipmi_enabled: false,
                secure_boot: false,
                tpm_enabled: false,
                tpm_version: AttestationTpmVersion::V138,
                tpm_persisted: false,
                hardware_sealing_policy: HardwareSealingPolicy::Hash,
                filtered_vpci_devices_allowed: true,
                vm_unique_id: String::new(),
                vmgs_provisioner: None,
            },
            true,
            None,
            None,
            Some(KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0x1234,
                },
                mix_measurement: true,
            }),
            GuestStateEncryptionPolicy::HardwareSealing,
            true,
            true,
            false,
        )
        .await
        .unwrap();

        // Should have recovered ingress from HWKP and rotated egress
        let keys = derived.derived_keys.unwrap();
        assert_eq!(keys.ingress, bootstrap);
        assert_ne!(keys.encrypt_egress, keys.ingress);
    }

    /// In stateless + hardware sealing mode, the GSP / GSP-by-id host callout
    /// must be skipped so that a host-requested `state_refresh` can never
    /// trigger a TPM seed refresh. Here the GSP callout is skipped entirely, so
    /// `get_derived_keys` must report no state refresh.
    ///
    /// See `non_sealing_propagates_gsp_state_refresh` for the contrasting case
    /// that proves a host-provided `state_refresh` is otherwise honored.
    #[async_test]
    async fn hardware_sealing_skips_gsp_state_refresh(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;
        let mut key_protector = KeyProtector::new_zeroed();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
        let bios_guid = Guid::new_random();

        // No scripted GSP responses are needed: hardware sealing mode skips the
        // GSP callout entirely.
        let get_pair = guest_emulation_transport::test_utilities::new_transport_pair(
            driver,
            None,
            get_protocol::ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let mock_tee_call = MockTeeCall::new([0x8a; 32]);

        let derived = get_derived_keys(
            &get_pair.client,
            Some(&mock_tee_call),
            &mut vmgs,
            &mut key_protector,
            &mut key_protector_by_id,
            bios_guid,
            &AttestationVmConfig {
                current_time: None,
                root_cert_thumbprint: String::new(),
                console_enabled: false,
                interactive_console_enabled: false,
                ipmi_enabled: false,
                secure_boot: false,
                tpm_enabled: false,
                tpm_version: AttestationTpmVersion::V138,
                tpm_persisted: false,
                hardware_sealing_policy: HardwareSealingPolicy::Hash,
                filtered_vpci_devices_allowed: true,
                vm_unique_id: String::new(),
                vmgs_provisioner: None,
            },
            false,
            None,
            None,
            Some(KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0x1234,
                },
                mix_measurement: true,
            }),
            GuestStateEncryptionPolicy::HardwareSealing,
            true,
            true,
            false,
        )
        .await
        .unwrap();

        // The GSP callout is skipped in sealing mode, so the host-requested
        // state refresh must not be reported.
        assert!(!derived.gsp_extended_status_flags.state_refresh_request());
    }

    /// Companion to `hardware_sealing_skips_gsp_state_refresh`: in the normal
    /// (non-sealing) flow the GSP callout IS made, so a host-provided
    /// `state_refresh` is propagated. This proves the host actually injects the
    /// flag, making the sealing-mode assertion meaningful.
    #[async_test]
    async fn non_sealing_propagates_gsp_state_refresh(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;
        let mut key_protector = KeyProtector::new_zeroed();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
        let bios_guid = Guid::new_random();

        // Script the host GSP response to report `state_refresh_request`, and a
        // GSP-by-id response with no registry file (no encryption source). The
        // non-sealing flow makes both callouts and captures the GSP flags.
        let gsp_response = TestGetResponses::new(Event::Response(
            get_protocol::GuestStateProtectionResponse {
                message_header: get_protocol::HeaderGeneric::new(
                    get_protocol::HostRequests::GUEST_STATE_PROTECTION,
                ),
                encrypted_gsp: get_protocol::GspCiphertextContent::new_zeroed(),
                decrypted_gsp: [get_protocol::GspCleartextContent::new_zeroed();
                    get_protocol::NUMBER_GSP as usize],
                extended_status_flags: GspExtendedStatusFlags::new()
                    .with_state_refresh_request(true),
            }
            .as_bytes()
            .to_vec(),
        ));
        let gsp_by_id_response = TestGetResponses::new(Event::Response(
            get_protocol::GuestStateProtectionByIdResponse {
                message_header: get_protocol::HeaderGeneric::new(
                    get_protocol::HostRequests::GUEST_STATE_PROTECTION_BY_ID,
                ),
                seed: get_protocol::GspCleartextContent::new_zeroed(),
                extended_status_flags: GspExtendedStatusFlags::new().with_no_registry_file(true),
            }
            .as_bytes()
            .to_vec(),
        ));

        let get_pair = guest_emulation_transport::test_utilities::new_transport_pair(
            driver,
            Some(vec![gsp_response, gsp_by_id_response]),
            get_protocol::ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let derived = get_derived_keys(
            &get_pair.client,
            None,
            &mut vmgs,
            &mut key_protector,
            &mut key_protector_by_id,
            bios_guid,
            &new_attestation_vm_config(),
            false,
            None,
            None,
            None,
            GuestStateEncryptionPolicy::Auto,
            false,
            false,
            false,
        )
        .await
        .unwrap();

        // The GSP callout is made in the normal flow, so the host-requested
        // state refresh is reported.
        assert!(derived.gsp_extended_status_flags.state_refresh_request());
    }

    #[async_test]
    async fn persist_all_key_protectors_write_key_protector_by_id() {
        let mut vmgs = new_formatted_vmgs().await;
        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
        let bios_guid = Guid::new_random();

        // Copied/cloned bits used for comparison later
        let kp_copy = key_protector.as_bytes().to_vec();
        let active_kp_copy = key_protector.active_kp;

        // When `use_gsp_by_id` is true and `should_write_kp` is false, the key protector by id should be written to the VMGS
        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: false,
            use_gsp_by_id: true,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::GspById,
            encrypt_gsp_type: GspType::GspById,
        };
        persist_all_key_protectors(
            &mut vmgs,
            &mut key_protector,
            &mut key_protector_by_id,
            None,
            bios_guid,
            key_protector_settings,
        )
        .await
        .unwrap();

        // The previously empty VMGS now holds the key protector by id but not the key protector
        assert!(key_protector_is_empty(&mut vmgs).await);
        assert!(!key_protector_by_id_is_empty(&mut vmgs).await);

        let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
        assert_eq!(
            found_key_protector_by_id.as_bytes(),
            key_protector_by_id.inner.as_bytes()
        );

        // The key protector should remain unchanged
        assert_eq!(kp_copy.as_slice(), key_protector.as_bytes());
        assert_eq!(active_kp_copy, key_protector.active_kp);
    }

    #[async_test]
    async fn persist_all_key_protectors_remove_ingress_kp() {
        let mut vmgs = new_formatted_vmgs().await;
        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
        let bios_guid = Guid::new_random();

        // Copied active KP for later use
        let active_kp_copy = key_protector.active_kp;

        // When `use_gsp_by_id` is false, `should_write_kp` is true, and `use_hardware_unlock` is false, the active key protector's
        // active kp's dek should be zeroed, the active kp's gsp length should be set to 0, and the active kp should be incremented
        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: false,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::None,
            encrypt_gsp_type: GspType::None,
        };
        persist_all_key_protectors(
            &mut vmgs,
            &mut key_protector,
            &mut key_protector_by_id,
            None,
            bios_guid,
            key_protector_settings,
        )
        .await
        .unwrap();

        assert!(!key_protector_is_empty(&mut vmgs).await);
        assert!(key_protector_by_id_is_empty(&mut vmgs).await);

        // The previously empty VMGS's key protector should now be overwritten
        let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
            .await
            .unwrap();

        assert!(
            found_key_protector.dek[active_kp_copy as usize]
                .dek_buffer
                .iter()
                .all(|&b| b == 0),
        );
        assert_eq!(
            found_key_protector.gsp[active_kp_copy as usize].gsp_length,
            0
        );
        assert_eq!(found_key_protector.active_kp, active_kp_copy + 1);
    }

    #[async_test]
    async fn persist_all_key_protectors_mark_key_protector_by_id_as_not_in_use() {
        let mut vmgs = new_formatted_vmgs().await;
        let mut key_protector = new_key_protector();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, true);
        let bios_guid = Guid::new_random();

        // When `use_gsp_by_id` is false, `should_write_kp` is true, `use_hardware_unlock` is true, and
        // the key protector by id is found and not ported, the key protector by id should be marked as ported
        let key_protector_settings = KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: false,
            use_hardware_unlock: true,
            decrypt_gsp_type: GspType::None,
            encrypt_gsp_type: GspType::None,
        };

        persist_all_key_protectors(
            &mut vmgs,
            &mut key_protector,
            &mut key_protector_by_id,
            Some(&HardwareKeyProtectorV3::new_zeroed()),
            bios_guid,
            key_protector_settings,
        )
        .await
        .unwrap();

        assert!(key_protector_is_empty(&mut vmgs).await);
        assert!(!key_protector_by_id_is_empty(&mut vmgs).await);

        // The previously empty VMGS's key protector by id should now be overwritten
        let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
        assert_eq!(found_key_protector_by_id.ported, 1);
        assert_eq!(
            found_key_protector_by_id.id_guid,
            key_protector_by_id.inner.id_guid
        );
    }

    // --- initialize_platform_security tests ---

    #[async_test]
    async fn init_sec_required_stateless_sealing_enrolls_and_rotates(driver: DefaultDriver) {
        let get_pair = new_test_get(driver, false, None).await;
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
        let mut config = new_attestation_vm_config();
        config.tpm_persisted = false;
        config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
        let tee = BootReportTee::new();
        let bios_guid = Guid::new_random();
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let mut previous_dek = None;

        for boot in 1..=2 {
            let result = initialize_platform_security(
                &get_pair.client,
                bios_guid,
                &config,
                &mut vmgs,
                Some(&tee),
                true,
                ldriver.clone(),
                GuestStateEncryptionPolicy::HardwareSealing,
                true,
            )
            .await
            .unwrap();
            assert!(result.runtime_tcb_floor.is_some());
            assert!(!result.host_attestation_settings.refresh_tpm_seeds);
            assert!(vmgs.encrypted());
            let active_dek = *vmgs.active_encryption_key().unwrap();
            assert_ne!(previous_dek, Some(active_dek));
            assert_eq!(&*tee.report_data.lock(), &vec![[0; REPORT_DATA_SIZE]; boot]);

            // Reopen without an extra test-side flush: enrollment finalized
            // persistence for this active DEK, not just a deferred protector.
            drop(vmgs);
            vmgs = Vmgs::open(disk.clone(), None).await.unwrap();
            assert!(matches!(
                vmgs.active_encryption_key(),
                Err(::vmgs::Error::NeedsUnlock)
            ));
            let protector = vmgs::read_hardware_key_protector(&mut vmgs).await.unwrap();
            let keys = HardwareDerivedKeys::derive_key(
                tee.supports_get_derived_key().unwrap(),
                &config,
                protector.key_derivation_policy().unwrap(),
            )
            .unwrap();
            assert_eq!(protector.unseal_key(&keys).unwrap(), active_dek);
            previous_dek = Some(active_dek);
        }
    }

    #[async_test]
    async fn init_sec_required_stateless_lm_before_first_derivation_seals_on_destination(
        driver: DefaultDriver,
    ) {
        let get_pair = new_test_get(driver, false, None).await;
        let mut config = new_attestation_vm_config();
        config.tpm_persisted = false;
        config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });

        for migrate_after_report in [false, true] {
            let disk = new_test_file();
            let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
            assert!(!vmgs.encrypted());
            assert!(hardware_key_protector_is_empty(&mut vmgs).await);
            let tee = FirstDerivationMigrationTee::new();
            assert_eq!(
                tee.source.inner.tcb_version,
                tee.destination.inner.tcb_version
            );
            let boot_tee: &dyn TeeCall = if migrate_after_report {
                &tee
            } else {
                // LM before report acquisition: boot starts on the destination.
                &tee.destination
            };
            let result = initialize_platform_security(
                &get_pair.client,
                Guid::new_random(),
                &config,
                &mut vmgs,
                Some(boot_tee),
                true,
                ldriver.clone(),
                GuestStateEncryptionPolicy::HardwareSealing,
                true,
            )
            .await
            .unwrap();
            assert!(result.runtime_tcb_floor.is_some());
            assert!(!result.host_attestation_settings.refresh_tpm_seeds);
            assert!(vmgs.encrypted());
            let active_dek = *vmgs.active_encryption_key().unwrap();
            assert_eq!(*tee.source.derivation_calls.lock(), 0);
            assert_eq!(*tee.destination.derivation_calls.lock(), 1);

            // No test-side flush: boot must persist the destination protector
            // and encrypt VMGS with the very DEK that protector contains.
            drop(vmgs);
            let mut reopened = Vmgs::open(disk, None).await.unwrap();
            assert!(reopened.encrypted());
            assert!(matches!(
                reopened.active_encryption_key(),
                Err(::vmgs::Error::NeedsUnlock)
            ));
            let protector = vmgs::read_hardware_key_protector(&mut reopened)
                .await
                .unwrap();
            let policy = KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: tee.source.inner.tcb_version,
                },
                mix_measurement: true,
            };
            let stored_policy = protector.key_derivation_policy().unwrap();
            assert!(stored_policy.mix_measurement);
            assert!(
                matches!(stored_policy.svn, tee_call::KeyDerivationSvn::Snp { tcb_version }
                if tcb_version == tee.source.inner.tcb_version)
            );
            if migrate_after_report {
                let policies = tee.derivation_policies.lock();
                assert_eq!(policies.len(), 1);
                assert!(policies[0].mix_measurement);
                assert!(
                    matches!(policies[0].svn, tee_call::KeyDerivationSvn::Snp { tcb_version }
                    if tcb_version == tee.source.inner.tcb_version)
                );
            } else {
                assert!(tee.derivation_policies.lock().is_empty());
            }

            // Derive independently from both contexts using identical SVN,
            // policy and configuration, not merely different mock identities.
            let source = tee.source.inner.supports_get_derived_key().unwrap();
            let destination = tee.destination.inner.supports_get_derived_key().unwrap();
            assert_ne!(
                source.get_derived_key(policy).unwrap(),
                destination.get_derived_key(policy).unwrap()
            );
            let source_keys = HardwareDerivedKeys::derive_key(source, &config, policy).unwrap();
            assert!(matches!(
                protector.unseal_key(&source_keys),
                Err(HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed)
            ));
            let destination_keys =
                HardwareDerivedKeys::derive_key(destination, &config, policy).unwrap();
            let unsealed_dek = protector.unseal_key(&destination_keys).unwrap();
            assert_eq!(unsealed_dek, active_dek);
            reopened
                .unlock_with_encryption_key(&unsealed_dek)
                .await
                .unwrap();
            assert_eq!(reopened.active_encryption_key().unwrap(), &active_dek);

            // Neither enrollment nor test-side verification may fetch another
            // report. The only report belongs to the chosen side of LM.
            if migrate_after_report {
                assert_eq!(&*tee.source.report_data.lock(), &[[0; REPORT_DATA_SIZE]]);
                assert!(tee.destination.report_data.lock().is_empty());
            } else {
                assert!(tee.source.report_data.lock().is_empty());
                assert_eq!(
                    &*tee.destination.report_data.lock(),
                    &[[0; REPORT_DATA_SIZE]]
                );
            }
        }
    }

    #[async_test]
    async fn init_sec_required_stateless_lm_rejects_source_svn_without_enrollment(
        driver: DefaultDriver,
    ) {
        let get_pair = new_test_get(driver, false, None).await;
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
        assert!(!vmgs.encrypted());
        assert!(hardware_key_protector_is_empty(&mut vmgs).await);
        let mut config = new_attestation_vm_config();
        config.tpm_persisted = false;
        config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
        let mut tee = FirstDerivationMigrationTee::new();
        // Raise one SNP TCB component on the source without changing the
        // destination, which cannot derive at the source report's higher SVN.
        tee.source.inner.tcb_version += 1;
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let result = initialize_platform_security(
            &get_pair.client,
            Guid::new_random(),
            &config,
            &mut vmgs,
            Some(&tee),
            true,
            ldriver,
            GuestStateEncryptionPolicy::HardwareSealing,
            true,
        )
        .await;
        // Failure returns no PlatformAttestationData, hence no runtime floor.
        // Do not assume an automatic reboot/retry with a destination report.
        assert!(matches!(
            result,
            Err(Error(AttestationErrorInner::GetDerivedKeys(
                GetDerivedKeysError::HardwareSealingRequiredButNotSupported
            )))
        ));
        {
            let policies = tee.derivation_policies.lock();
            assert_eq!(policies.len(), 1);
            assert!(policies[0].mix_measurement);
            assert!(
                matches!(policies[0].svn, tee_call::KeyDerivationSvn::Snp { tcb_version }
                if tcb_version == tee.source.inner.tcb_version)
            );
        }
        assert_eq!(&*tee.source.report_data.lock(), &[[0; REPORT_DATA_SIZE]]);
        assert!(tee.destination.report_data.lock().is_empty());
        assert_eq!(*tee.source.derivation_calls.lock(), 0);
        assert_eq!(*tee.destination.derivation_calls.lock(), 0);
        assert!(!vmgs.encrypted());
        assert!(vmgs.active_encryption_key().is_err());
        assert!(hardware_key_protector_is_empty(&mut vmgs).await);
        assert!(key_protector_is_empty(&mut vmgs).await);
        assert!(key_protector_by_id_is_empty(&mut vmgs).await);

        drop(vmgs);
        let mut reopened = Vmgs::open(disk, None).await.unwrap();
        assert!(!reopened.encrypted());
        assert!(reopened.active_encryption_key().is_err());
        assert!(hardware_key_protector_is_empty(&mut reopened).await);
        assert!(key_protector_is_empty(&mut reopened).await);
        assert!(key_protector_by_id_is_empty(&mut reopened).await);
    }

    #[async_test]
    async fn init_sec_required_stateless_derivation_failure_cannot_boot(driver: DefaultDriver) {
        let get_pair = new_test_get(driver, false, None).await;
        let mut config = new_attestation_vm_config();
        config.tpm_persisted = false;
        config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });

        for encrypted in [false, true] {
            let disk = new_test_file();
            let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
            let bios_guid = Guid::new_random();
            let mut tee = BootReportTee::new();
            if encrypted {
                let provisioned = initialize_platform_security(
                    &get_pair.client,
                    bios_guid,
                    &config,
                    &mut vmgs,
                    Some(&tee),
                    true,
                    ldriver.clone(),
                    GuestStateEncryptionPolicy::HardwareSealing,
                    true,
                )
                .await
                .unwrap();
                assert!(provisioned.runtime_tcb_floor.is_some());
                drop(vmgs);
                vmgs = Vmgs::open(disk, None).await.unwrap();
            }
            tee.fail_derivation = true;
            tee.report_data.lock().clear();
            *tee.derivation_calls.lock() = 0;
            let result = initialize_platform_security(
                &get_pair.client,
                bios_guid,
                &config,
                &mut vmgs,
                Some(&tee),
                true,
                ldriver.clone(),
                GuestStateEncryptionPolicy::HardwareSealing,
                true,
            )
            .await;
            assert!(result.is_err());
            assert_eq!(&*tee.report_data.lock(), &[[0; REPORT_DATA_SIZE]]);
            assert!(*tee.derivation_calls.lock() > 0);
            assert_eq!(vmgs.encrypted(), encrypted);
            assert!(vmgs.active_encryption_key().is_err());
        }
    }

    #[async_test]
    async fn init_sec_optional_sealing_requires_this_boot_write(driver: DefaultDriver) {
        let get_pair = new_test_get(driver, true, None).await;
        let mut config = new_attestation_vm_config();
        config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });

        // The default GED has no GSP. Check both automatic policy and
        // explicitly disabled GSP with tenant-key-only encryption.
        for policy in [
            GuestStateEncryptionPolicy::Auto,
            GuestStateEncryptionPolicy::None,
        ] {
            let disk = new_test_file();
            let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
            let bios_guid = Guid::new_random();
            let mut tee = BootReportTee::new();
            let provisioned = initialize_platform_security(
                &get_pair.client,
                bios_guid,
                &config,
                &mut vmgs,
                Some(&tee),
                false,
                ldriver.clone(),
                policy,
                true,
            )
            .await
            .unwrap();
            assert!(provisioned.runtime_tcb_floor.is_some());
            let old_dek = *vmgs.active_encryption_key().unwrap();
            let old_protector = vmgs::read_hardware_key_protector(&mut vmgs).await.unwrap();
            drop(vmgs);
            vmgs = Vmgs::open(disk, None).await.unwrap();

            // Hardware still supports derivation and returns a valid trusted
            // report, but deriving the optional backup key now fails. The
            // preexisting valid protector must not enroll the newly active DEK.
            tee.fail_derivation = true;
            *tee.derivation_calls.lock() = 0;
            let result = initialize_platform_security(
                &get_pair.client,
                bios_guid,
                &config,
                &mut vmgs,
                Some(&tee),
                false,
                ldriver.clone(),
                policy,
                true,
            )
            .await
            .unwrap();
            assert!(result.runtime_tcb_floor.is_none());
            assert!(*tee.derivation_calls.lock() > 0);
            assert_eq!(tee.report_data.lock().len(), 2);
            assert!(vmgs.encrypted());
            assert_ne!(vmgs.active_encryption_key().unwrap(), &old_dek);
            let protector = vmgs::read_hardware_key_protector(&mut vmgs).await.unwrap();
            // Readback is test evidence only, never the enrollment decision.
            tee.fail_derivation = false;
            let keys = HardwareDerivedKeys::derive_key(
                tee.supports_get_derived_key().unwrap(),
                &config,
                old_protector.key_derivation_policy().unwrap(),
            )
            .unwrap();
            assert_eq!(protector.unseal_key(&keys).unwrap(), old_dek);
        }
    }

    #[async_test]
    async fn derived_gsp_tracks_writes_but_gsp_by_id_does_not(driver: DefaultDriver) {
        for use_gsp_by_id in [false, true] {
            for fail_derivation in [false, true] {
                let mut vmgs = new_formatted_vmgs().await;
                let mut config = new_attestation_vm_config();
                config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
                let mut tee = BootReportTee::new();
                tee.fail_derivation = fail_derivation;
                let mut gsp = get_protocol::GuestStateProtectionResponse::new_zeroed();
                gsp.message_header = get_protocol::HeaderGeneric::new(
                    get_protocol::HostRequests::GUEST_STATE_PROTECTION,
                );
                if !use_gsp_by_id {
                    gsp.encrypted_gsp.length = 32;
                    gsp.encrypted_gsp.buffer[..32].fill(0x55);
                }
                let mut responses = vec![TestGetResponses::new(Event::Response(
                    gsp.as_bytes().to_vec(),
                ))];
                if use_gsp_by_id {
                    let mut by_id = get_protocol::GuestStateProtectionByIdResponse::new_zeroed();
                    by_id.message_header = get_protocol::HeaderGeneric::new(
                        get_protocol::HostRequests::GUEST_STATE_PROTECTION_BY_ID,
                    );
                    by_id.seed.length = 32;
                    by_id.seed.buffer[..32].fill(0x66);
                    responses.push(TestGetResponses::new(Event::Response(
                        by_id.as_bytes().to_vec(),
                    )));
                }
                let get_pair = guest_emulation_transport::test_utilities::new_transport_pair(
                    driver.clone(),
                    Some(responses),
                    get_protocol::ProtocolVersion::NICKEL_REV2,
                    None,
                    None,
                )
                .await;
                let mut kp = KeyProtector::new_zeroed();
                let mut kp_by_id = new_key_protector_by_id(Some(Guid::default()), None, false);
                let bios_guid = Guid::new_random();
                let derived = get_derived_keys(
                    &get_pair.client,
                    Some(&tee),
                    &mut vmgs,
                    &mut kp,
                    &mut kp_by_id,
                    bios_guid,
                    &config,
                    false,
                    None,
                    None,
                    Some(KeyDerivationPolicy {
                        svn: tee_call::KeyDerivationSvn::Snp {
                            tcb_version: tee.inner.tcb_version,
                        },
                        mix_measurement: true,
                    }),
                    GuestStateEncryptionPolicy::Auto,
                    true,
                    false,
                    false,
                )
                .await
                .unwrap();
                let eligible = !use_gsp_by_id && !fail_derivation;
                assert_eq!(derived.hardware_key_protector_written, eligible);
                assert!(derived.hardware_key_protector.is_none());
                assert_eq!(derived.key_protector_settings.use_gsp_by_id, use_gsp_by_id);
                let sealed_key = derived
                    .derived_keys
                    .as_ref()
                    .filter(|_| derived.hardware_key_protector_written)
                    .map(|keys| keys.encrypt_egress);
                unlock_vmgs_data_store(
                    &mut vmgs,
                    false,
                    &mut kp,
                    &mut kp_by_id,
                    derived.hardware_key_protector,
                    derived.derived_keys,
                    derived.key_protector_settings,
                    bios_guid,
                )
                .await
                .unwrap();
                assert!(vmgs.encrypted());
                assert_eq!(
                    finalize_hardware_sealing(&mut vmgs, sealed_key, false)
                        .await
                        .unwrap(),
                    eligible
                );
            }
        }
    }

    #[async_test]
    async fn old_egress_unlock_does_not_enroll_different_sealed_key() {
        let mut vmgs = new_formatted_vmgs().await;
        let active_dek = [0x33; AES_GCM_KEY_LENGTH];
        let sealed_dek = [0x44; AES_GCM_KEY_LENGTH];
        vmgs.test_add_new_encryption_key(&active_dek, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        let tee = BootReportTee::new();
        let mut config = new_attestation_vm_config();
        config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
        let hardware_keys = HardwareDerivedKeys::derive_key(
            tee.supports_get_derived_key().unwrap(),
            &config,
            KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: tee.inner.tcb_version,
                },
                mix_measurement: true,
            },
        )
        .unwrap();
        let protector = hardware_key_sealing::seal_key(&hardware_keys, &sealed_dek).unwrap();
        let mut key_protector = KeyProtector::new_zeroed();
        let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
        unlock_vmgs_data_store(
            &mut vmgs,
            true,
            &mut key_protector,
            &mut key_protector_by_id,
            Some(protector),
            Some(Keys {
                ingress: sealed_dek,
                decrypt_egress: Some(active_dek),
                encrypt_egress: sealed_dek,
            }),
            KeyProtectorSettings {
                should_write_kp: false,
                use_gsp_by_id: false,
                use_hardware_unlock: false,
                decrypt_gsp_type: GspType::None,
                encrypt_gsp_type: GspType::None,
            },
            Guid::new_random(),
        )
        .await
        .unwrap();
        assert_eq!(vmgs.active_encryption_key().unwrap(), &active_dek);
        assert!(
            !finalize_hardware_sealing(&mut vmgs, Some(sealed_dek), false)
                .await
                .unwrap()
        );
        assert!(
            !finalize_hardware_sealing(&mut vmgs, None, false)
                .await
                .unwrap()
        );
    }

    fn init_sec_with_retry_reports(
        report_svns: [u64; 2],
    ) -> (
        Option<runtime_sealing::RuntimeTcbFloor>,
        [u8; AES_GCM_KEY_LENGTH],
    ) {
        // GET tasks need their own running executor. Keep the local executor
        // alive around initialization itself so its real one-second retry timer
        // progresses, rather than returning an orphaned LocalDriver.
        let (get_thread, driver) = pal_async::DefaultPool::spawn_on_thread("boot-report-retry-get");
        let result = pal_async::local::block_with_io(async |ldriver| {
            let mut plan = IgvmAgentTestPlan::default();
            plan.insert(
                IgvmAttestRequestType::KEY_RELEASE_REQUEST,
                VecDeque::from([
                    IgvmAgentAction::RespondSuccess,
                    // RespondFailure and RespondFailureSkipHwUnsealing both
                    // explicitly set retry=false. NoResponse completes GET
                    // with an empty response, producing a retryable parse error.
                    IgvmAgentAction::NoResponse,
                    IgvmAgentAction::RespondSuccess,
                ]),
            );
            let get_pair = new_test_get(driver, true, Some(plan)).await;
            let bios_guid = Guid::new_random();
            let mut config = new_attestation_vm_config();
            config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
            let disk = new_test_file();
            let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
            let provision_tee = BootReportTee::new();

            // Provisioning is setup only: neither its report nor its collector
            // participates in the single retried initialization below.
            let provisioned = initialize_platform_security(
                &get_pair.client,
                bios_guid,
                &config,
                &mut vmgs,
                Some(&provision_tee),
                false,
                ldriver.clone(),
                GuestStateEncryptionPolicy::Auto,
                true,
            )
            .await
            .unwrap();
            assert!(provisioned.runtime_tcb_floor.is_some());
            assert_eq!(provision_tee.report_data.lock().len(), 1);
            assert!(vmgs.encrypted());
            assert!(!hardware_key_protector_is_empty(&mut vmgs).await);

            // A working hardware backup would make the first failed SKR
            // attempt succeed without retrying. Remove only that backup and
            // reopen the encrypted store to require an actual SKR unlock.
            vmgs.delete_file(FileId::HW_KEY_PROTECTOR).await.unwrap();
            vmgs.flush().await.unwrap();
            drop(vmgs);
            let mut vmgs = Vmgs::open(disk, None).await.unwrap();
            assert!(vmgs.encrypted());
            assert!(matches!(
                vmgs.active_encryption_key(),
                Err(::vmgs::Error::NeedsUnlock)
            ));
            assert!(hardware_key_protector_is_empty(&mut vmgs).await);

            let tee = SequencedBootReportTee {
                inner: BootReportTee::new(),
                report_svns: parking_lot::Mutex::new(report_svns.into()),
            };
            let recovered = initialize_platform_security(
                &get_pair.client,
                bios_guid,
                &config,
                &mut vmgs,
                Some(&tee),
                false,
                ldriver,
                GuestStateEncryptionPolicy::Auto,
                true,
            )
            .await
            .unwrap();

            // Both attempts belong to the invocation above. No collector or
            // fallback report may be requested, and both keep their SKR hash.
            assert!(tee.report_svns.lock().is_empty());
            {
                let reports = tee.inner.report_data.lock();
                assert_eq!(reports.len(), 2);
                assert!(reports.iter().all(|data| *data != [0; REPORT_DATA_SIZE]));
            }
            assert_eq!(provision_tee.report_data.lock().len(), 1);
            assert!(vmgs.encrypted());
            let active_dek = *vmgs.active_encryption_key().unwrap();

            // Boot sealing still uses the successful report's SVN, including
            // a lower SVN, rather than substituting the runtime collector's
            // high-water mark or making that collector's failure fatal.
            let protector = vmgs::read_hardware_key_protector(&mut vmgs).await.unwrap();
            let policy = protector.key_derivation_policy().unwrap();
            assert!(policy.mix_measurement);
            assert!(matches!(
                policy.svn,
                tee_call::KeyDerivationSvn::Snp { tcb_version } if tcb_version == report_svns[1]
            ));
            let keys = HardwareDerivedKeys::derive_key(
                tee.supports_get_derived_key().unwrap(),
                &config,
                policy,
            )
            .unwrap();
            assert_eq!(protector.unseal_key(&keys).unwrap(), active_dek);
            vmgs.unlock_with_encryption_key(&active_dek).await.unwrap();

            (recovered.runtime_tcb_floor, active_dek)
        });
        get_thread.join().unwrap();
        result
    }

    #[test]
    fn init_sec_retry_high_then_lower_svn_succeeds_without_runtime_floor() {
        let (floor, _) = init_sec_with_retry_reports([0x1235, 0x1234]);
        // Recreating the collector on retry, or observing only successful SKR
        // reports, would incorrectly export the second (lower) report here.
        assert!(floor.is_none());
    }

    #[test]
    fn init_sec_retry_low_then_higher_svn_exports_highest_runtime_floor() {
        let (floor, active_dek) = init_sec_with_retry_reports([0x1234, 0x1235]);
        let mut floor = floor.unwrap();
        let mut config = new_attestation_vm_config();
        config.hardware_sealing_policy = HardwareSealingPolicy::Hash;

        // Use a separate runtime TEE so these intentional fresh reports cannot
        // hide extra boot report acquisitions in the two-attempt assertion.
        let mut tee = BootReportTee::new();
        let err = floor
            .create_protector(&tee, &config, &active_dek)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "local TCB has a component below the runtime floor"
        );
        tee.inner.tcb_version = 0x1235;
        let protector = floor.create_protector(&tee, &config, &active_dek).unwrap();
        assert!(
            runtime_sealing::protector_matches(&tee, &config, &protector, &active_dek).unwrap()
        );
        assert_eq!(&*tee.report_data.lock(), &[[0; REPORT_DATA_SIZE]; 2]);
    }

    #[async_test]
    async fn init_sec_hardware_cached_lower_svn_preserves_boot_policy(driver: DefaultDriver) {
        let get_pair = new_test_get(driver, false, None).await;
        for malformed in [false, true] {
            let mut vmgs = new_formatted_vmgs().await;
            let bootstrap = [0x33; AES_GCM_KEY_LENGTH];
            vmgs.test_add_new_encryption_key(&bootstrap, EncryptionAlgorithm::AES_GCM)
                .await
                .unwrap();
            let mut tee = BootReportTee::new();
            let mut config = new_attestation_vm_config();
            config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
            let cached_svn = tee.inner.tcb_version;
            let keys = HardwareDerivedKeys::derive_key(
                tee.supports_get_derived_key().unwrap(),
                &config,
                KeyDerivationPolicy {
                    svn: tee_call::KeyDerivationSvn::Snp {
                        tcb_version: cached_svn,
                    },
                    mix_measurement: true,
                },
            )
            .unwrap();
            let protector = hardware_key_sealing::seal_key(&keys, &bootstrap).unwrap();
            vmgs::write_hardware_key_protector(&protector, &mut vmgs)
                .await
                .unwrap();

            // Current hardware has advanced, but the boot unseal/rotation path
            // must continue using the older cached policy, not the runtime floor.
            tee.inner.tcb_version += 1;
            tee.malformed = malformed;
            let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
            let result = initialize_platform_security(
                &get_pair.client,
                Guid::new_random(),
                &config,
                &mut vmgs,
                Some(&tee),
                true,
                ldriver,
                GuestStateEncryptionPolicy::HardwareSealing,
                true,
            )
            .await
            .unwrap();
            assert!(vmgs.encrypted());
            assert!(!result.host_attestation_settings.refresh_tpm_seeds);
            // Exactly the original hardware-only report; no collector report.
            assert_eq!(&*tee.report_data.lock(), &[[0; REPORT_DATA_SIZE]]);
            let rotated = vmgs::read_hardware_key_protector(&mut vmgs).await.unwrap();
            assert!(matches!(
                rotated.key_derivation_policy().unwrap().svn,
                tee_call::KeyDerivationSvn::Snp { tcb_version } if tcb_version == cached_svn
            ));
            let active_dek = rotated.unseal_key(&keys).unwrap();
            vmgs.unlock_with_encryption_key(&active_dek).await.unwrap();

            if malformed {
                // Collection failure must not make a previously valid boot fail.
                assert!(result.runtime_tcb_floor.is_none());
            } else {
                let mut floor = result.runtime_tcb_floor.unwrap();
                tee.inner.tcb_version = cached_svn;
                let err = floor
                    .create_protector(&tee, &config, &active_dek)
                    .unwrap_err();
                assert_eq!(
                    err.to_string(),
                    "local TCB has a component below the runtime floor"
                );
            }
        }
    }

    #[async_test]
    async fn init_sec_skr_failure_hardware_fallback_exports_existing_report(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;
        let mut plan = IgvmAgentTestPlan::default();
        plan.insert(
            IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
            VecDeque::from([
                IgvmAgentAction::RespondSuccess,
                IgvmAgentAction::RespondFailure,
            ]),
        );
        let get_pair = new_test_get(driver, true, Some(plan)).await;
        let bios_guid = Guid::new_random();
        let mut config = new_attestation_vm_config();
        config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
        let mut tee = BootReportTee::new();
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let first = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &config,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver.clone(),
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();
        assert!(first.runtime_tcb_floor.is_some());
        assert_eq!(tee.report_data.lock().len(), 1);
        assert!(!hardware_key_protector_is_empty(&mut vmgs).await);

        // SKR fails after report acquisition on the next boot. The fallback
        // unseals at the cached SVN, but must export the newer observed floor.
        let cached_svn = tee.inner.tcb_version;
        tee.inner.tcb_version += 1;
        let recovered = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &config,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver,
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();
        assert!(vmgs.encrypted());
        {
            let reports = tee.report_data.lock();
            assert_eq!(reports.len(), 2);
            // Both reports retain their SKR claims hashes, not zero report_data.
            assert!(reports.iter().all(|data| *data != [0; REPORT_DATA_SIZE]));
        }
        let mut floor = recovered.runtime_tcb_floor.unwrap();
        tee.inner.tcb_version = cached_svn;
        let err = floor.create_protector(&tee, &config, &[0; 32]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "local TCB has a component below the runtime floor"
        );
    }

    #[async_test]
    async fn init_sec_suppressed_eligible_tee_does_not_bootstrap_floor(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;
        let get_pair = new_test_get(driver, false, None).await;
        let tee = BootReportTee::new();
        let mut config = new_attestation_vm_config();
        config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let result = initialize_platform_security(
            &get_pair.client,
            Guid::new_random(),
            &config,
            &mut vmgs,
            Some(&tee),
            true,
            ldriver,
            GuestStateEncryptionPolicy::None,
            true,
        )
        .await
        .unwrap();
        assert!(result.runtime_tcb_floor.is_none());
        assert!(tee.report_data.lock().is_empty());
        assert_eq!(*tee.derivation_calls.lock(), 0);
        assert!(!vmgs.encrypted());
    }

    #[async_test]
    async fn init_sec_suppress_attestation(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;

        // Write non-zero agent data to VMGS so we can verify it is returned.
        let agent = SecurityProfile {
            agent_data: [0xAA; AGENT_DATA_MAX_SIZE],
        };
        vmgs.write_file(FileId::ATTEST, agent.as_bytes())
            .await
            .unwrap();

        // Ensure no IGVM attest call out
        let get_pair = new_test_get(driver, false, None).await;

        let bios_guid = Guid::new_random();
        let att_cfg = test_attestation_config();

        // Ensure VMGS is not encrypted and agent data is empty before the call
        assert!(!vmgs.encrypted());

        // Obtain a LocalDriver briefly, then run the async flow under the pool executor
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let res = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            None, // no TEE when suppressed
            true, // suppress_attestation
            ldriver,
            GuestStateEncryptionPolicy::None,
            true,
        )
        .await
        .unwrap();

        // VMGS remains unencrypted and KP/HWKP not written.
        assert!(!vmgs.encrypted());
        assert!(key_protector_is_empty(&mut vmgs).await);
        assert!(hardware_key_protector_is_empty(&mut vmgs).await);
        // Agent data passed through
        assert_eq!(res.agent_data.unwrap(), agent.agent_data.to_vec());
        // Secure key should be None without pre-provisioning
        assert!(res.guest_secret_key.is_none());
    }

    #[async_test]
    async fn init_sec_secure_key_release_with_wrapped_key_request(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;

        // IGVM attest is required
        let get_pair = new_test_get(driver, true, None).await;

        let bios_guid = Guid::new_random();
        let att_cfg = new_attestation_vm_config();
        let tee = MockTeeCall::new([0x12u8; 32]);

        // Ensure VMGS is not encrypted and agent data is empty before the call
        assert!(!vmgs.encrypted());

        // Obtain a LocalDriver briefly, then run the async flow under the pool executor
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let res = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver.clone(),
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();

        // VMGS is now encrypted and HWKP is updated.
        assert!(vmgs.encrypted());
        assert!(!hardware_key_protector_is_empty(&mut vmgs).await);

        // Agent data should be the same as `key_reference` in the WRAPPED_KEY response.
        // See vm/devices/get/guest_emulation_device/src/test_igvm_agent.rs for the expected response.
        let key_reference = serde_json::json!({
            "key_info": {
                "host": "name"
            },
            "attestation_info": {
                "host": "attestation_name"
            }
        });
        let key_reference = serde_json::to_string(&key_reference).unwrap();
        let key_reference = key_reference.as_bytes();
        let mut expected_agent_data = [0u8; AGENT_DATA_MAX_SIZE];
        expected_agent_data[..key_reference.len()].copy_from_slice(key_reference);
        assert_eq!(res.agent_data.unwrap(), expected_agent_data.to_vec());
        // Secure key should be None without pre-provisioning
        assert!(res.guest_secret_key.is_none());

        // Second call: VMGS unlock via SKR should succeed
        initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver,
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();

        // VMGS should remain encrypted
        assert!(vmgs.encrypted());
    }

    #[async_test]
    async fn init_sec_secure_key_release_without_wrapped_key_request(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;

        // Write non-zero agent data to workaround the WRAPPED_KEY_REQUEST requirement.
        let agent = SecurityProfile {
            agent_data: [0xAA; AGENT_DATA_MAX_SIZE],
        };
        vmgs.write_file(FileId::ATTEST, agent.as_bytes())
            .await
            .unwrap();

        // Skip WRAPPED_KEY_REQUEST for both boots
        let mut plan = IgvmAgentTestPlan::default();
        plan.insert(
            IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
            VecDeque::from([IgvmAgentAction::NoResponse, IgvmAgentAction::NoResponse]),
        );

        // IGVM attest is required
        let get_pair = new_test_get(driver, true, Some(plan)).await;

        let bios_guid = Guid::new_random();
        let att_cfg = new_attestation_vm_config();
        let tee = MockTeeCall::new([0x12u8; 32]);

        // Ensure VMGS is not encrypted and agent data is empty before the call
        assert!(!vmgs.encrypted());

        // Obtain a LocalDriver briefly, then run the async flow under the pool executor
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let res = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver.clone(),
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();

        // VMGS is now encrypted and HWKP is updated.
        assert!(vmgs.encrypted());
        assert!(!hardware_key_protector_is_empty(&mut vmgs).await);
        // Agent data passed through
        assert_eq!(res.agent_data.clone().unwrap(), agent.agent_data.to_vec());
        // Secure key should be None without pre-provisioning
        assert!(res.guest_secret_key.is_none());

        // Second call: VMGS unlock via SKR should succeed
        let res = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver,
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();

        // VMGS should remain encrypted
        assert!(vmgs.encrypted());
        // Agent data passed through
        assert_eq!(res.agent_data.clone().unwrap(), agent.agent_data.to_vec());
        // Secure key should be None without pre-provisioning
        assert!(res.guest_secret_key.is_none());
    }

    #[async_test]
    async fn init_sec_secure_key_release_hw_sealing_backup(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;

        // IGVM attest is required
        let mut plan = IgvmAgentTestPlan::default();
        plan.insert(
            IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
            VecDeque::from([
                IgvmAgentAction::RespondSuccess,
                // initialize_platform_security will attempt SKR/unlock 10 times
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
            ]),
        );

        let get_pair = new_test_get(driver, true, Some(plan)).await;

        let bios_guid = Guid::new_random();
        let att_cfg = test_attestation_config();

        // Ensure VMGS is not encrypted and agent data is empty before the call
        assert!(!vmgs.encrypted());

        // Obtain a LocalDriver briefly, then run the async flow under the pool executor
        let tee = MockTeeCall::new([0x12u8; 32]);
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let res = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver.clone(),
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();

        // VMGS is now encrypted and HWKP is updated.
        assert!(vmgs.encrypted());
        assert!(!hardware_key_protector_is_empty(&mut vmgs).await);
        // Agent data should be the same as `key_reference` in the WRAPPED_KEY response.
        // See vm/devices/get/guest_emulation_device/src/test_igvm_agent.rs for the expected response.
        let key_reference = serde_json::json!({
            "key_info": {
                "host": "name"
            },
            "attestation_info": {
                "host": "attestation_name"
            }
        });
        let key_reference = serde_json::to_string(&key_reference).unwrap();
        let key_reference = key_reference.as_bytes();
        let mut expected_agent_data = [0u8; AGENT_DATA_MAX_SIZE];
        expected_agent_data[..key_reference.len()].copy_from_slice(key_reference);
        assert_eq!(res.agent_data.unwrap(), expected_agent_data.to_vec());
        // Secure key should be None without pre-provisioning
        assert!(res.guest_secret_key.is_none());

        // Second call: VMGS unlock via key recovered with hardware sealing
        // NOTE: The test relies on the test GED to return failing WRAPPED_KEY response
        // with retry recommendation as false to skip the retry loop in
        // secure_key_release::request_vmgs_encryption_keys. Otherwise, the test will stuck
        // on the timer.sleep() as the the driver is not progressed.
        initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver,
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();

        // VMGS should remain encrypted
        assert!(vmgs.encrypted());
    }

    #[async_test]
    async fn init_sec_secure_key_release_skip_hw_unsealing(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;

        // IGVM attest is required
        // KEY_RELEASE succeeds on first boot, fails with skip_hw_unsealing on second boot.
        // WRAPPED_KEY is not in the plan, so it falls back to default (success) every time.
        let mut plan = IgvmAgentTestPlan::default();
        plan.insert(
            IgvmAttestRequestType::KEY_RELEASE_REQUEST,
            VecDeque::from([
                IgvmAgentAction::RespondSuccess,
                IgvmAgentAction::RespondFailureSkipHwUnsealing,
            ]),
        );

        let get_pair = new_test_get(driver, true, Some(plan)).await;

        let bios_guid = Guid::new_random();
        let att_cfg = test_attestation_config();

        // Ensure VMGS is not encrypted and agent data is empty before the call
        assert!(!vmgs.encrypted());

        // Obtain a LocalDriver briefly, then run the async flow under the pool executor
        let tee = MockTeeCall::new([0x12u8; 32]);
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let res = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver.clone(),
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();

        // VMGS is now encrypted and HWKP is updated.
        assert!(vmgs.encrypted());
        assert!(!hardware_key_protector_is_empty(&mut vmgs).await);
        // Agent data should be the same as `key_reference` in the WRAPPED_KEY response.
        let key_reference = serde_json::json!({
            "key_info": {
                "host": "name"
            },
            "attestation_info": {
                "host": "attestation_name"
            }
        });
        let key_reference = serde_json::to_string(&key_reference).unwrap();
        let key_reference = key_reference.as_bytes();
        let mut expected_agent_data = [0u8; AGENT_DATA_MAX_SIZE];
        expected_agent_data[..key_reference.len()].copy_from_slice(key_reference);
        assert_eq!(res.agent_data.unwrap(), expected_agent_data.to_vec());
        // Secure key should be None without pre-provisioning
        assert!(res.guest_secret_key.is_none());

        // Second call: KEY_RELEASE fails with skip_hw_unsealing signal.
        // The skip_hw_unsealing signal causes the hardware unsealing fallback to be
        // skipped, so VMGS unlock should fail.
        // NOTE: The test relies on the test GED to return failing KEY_RELEASE response
        // with retry recommendation as false so the retry loop terminates immediately.
        // Otherwise, the test will get stuck on timer.sleep() as the driver is not
        // progressed.
        let result = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver,
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await;

        assert!(result.is_err());
    }

    #[async_test]
    async fn init_sec_secure_key_release_no_hw_sealing_backup(driver: DefaultDriver) {
        let mut vmgs = new_formatted_vmgs().await;

        // IGVM attest is required
        let mut plan = IgvmAgentTestPlan::default();
        plan.insert(
            IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
            VecDeque::from([
                IgvmAgentAction::RespondSuccess,
                // initialize_platform_security will attempt SKR/unlock 10 times
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
                IgvmAgentAction::RespondFailure,
            ]),
        );

        let get_pair = new_test_get(driver, true, Some(plan)).await;

        let bios_guid = Guid::new_random();
        let att_cfg = test_attestation_config();
        // Without hardware sealing support
        let tee = MockTeeCallNoGetDerivedKey {};

        // Ensure VMGS is not encrypted and agent data is empty before the call
        assert!(!vmgs.encrypted());

        // Obtain a LocalDriver briefly, then run the async flow under the pool executor
        let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
        let res = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver.clone(),
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await
        .unwrap();

        // VMGS is now encrypted but HWKP remains empty.
        assert!(vmgs.encrypted());
        assert!(hardware_key_protector_is_empty(&mut vmgs).await);
        // Agent data should be the same as `key_reference` in the WRAPPED_KEY response.
        // See vm/devices/get/guest_emulation_device/src/test_igvm_agent.rs for the expected response.
        let key_reference = serde_json::json!({
            "key_info": {
                "host": "name"
            },
            "attestation_info": {
                "host": "attestation_name"
            }
        });
        let key_reference = serde_json::to_string(&key_reference).unwrap();
        let key_reference = key_reference.as_bytes();
        let mut expected_agent_data = [0u8; AGENT_DATA_MAX_SIZE];
        expected_agent_data[..key_reference.len()].copy_from_slice(key_reference);
        assert_eq!(res.agent_data.unwrap(), expected_agent_data.to_vec());
        // Secure key should be None without pre-provisioning
        assert!(res.guest_secret_key.is_none());

        // Second call: VMGS unlock should fail without hardware sealing support
        let result = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &att_cfg,
            &mut vmgs,
            Some(&tee),
            false,
            ldriver,
            GuestStateEncryptionPolicy::Auto,
            true,
        )
        .await;

        assert!(result.is_err());
    }

    #[test]
    fn test_get_provenance_claims() {
        // Test JWT: not a valid credential or secret for anything.
        const PROVENANCE_DOC: &str = include_str!("../test_data/valid_jwt");
        let doc = PROVENANCE_DOC.trim().strip_prefix("placeholder_").unwrap();
        let claims = get_provenance_claims(doc.as_bytes()).unwrap();
        assert_eq!(
            claims.id,
            guid::guid!("03020100-0504-0706-0809-0a0b0c0d0e0f")
        );
        assert_eq!(
            claims.signer,
            "did:x509:0:sha256:EA76599D86897382AA519FF2BC0FA6B9C15D60DA2EBE53E72139CD317B0797ED:subject:fican.cvmprovisioningservice.core.azure-test.net"
        );
    }

    #[test]
    fn test_derive_vmgsid() {
        const SEED_DOC_1: &str = "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F,4C6162656C5F435053,436F6E746578745F564D4753,32";
        const SEED_DOC_2: &str = "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F,4C6162656C5F435053,436F6E746578745F564D4753";
        const SEED_DOC_3: &str = "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F,4C6162656C5F435053,436F6E746578745F564D4753,32,ABCDEF";
        const GUID: Guid = guid::guid!("b0587f2d-11e6-9f66-1af4-8b4a619147c8");

        let vmgsid1 = derive_vmgsid(SEED_DOC_1.as_bytes()).unwrap();
        assert_eq!(vmgsid1, GUID);

        let vmgsid2 = derive_vmgsid(SEED_DOC_2.as_bytes()).unwrap();
        assert_eq!(vmgsid2, GUID);

        let vmgsid3 = derive_vmgsid(SEED_DOC_3.as_bytes()).unwrap();
        assert_eq!(vmgsid3, GUID);
    }
}
