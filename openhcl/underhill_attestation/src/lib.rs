// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This modules implements attestation protocols for Underhill to support TVM
//! and CVM, including getting a tenant key via secure key release (SKR) for
//! unlocking VMGS and requesting an attestation key (AK) certificate for TPM.
//! The module also implements the VMGS unlocking process based on SKR.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

mod derived_keys;
mod hardware_key_sealing;
mod igvm_attest;
mod jwt;
mod key_protector;
mod secure_key_release;
mod vmgs;

#[cfg(test)]
mod test_helpers;

#[cfg(test)]
mod tests;

pub use igvm_attest::Error as IgvmAttestError;
pub use igvm_attest::IgvmAttestRequestHelper;
pub use igvm_attest::ak_cert::parse_response as parse_ak_cert_response;

use crate::jwt::JwtError;
use crate::jwt::JwtHelper;
use ::vmgs::EncryptionAlgorithm;
use ::vmgs::GspType;
use ::vmgs::Vmgs;
use crypto::sha_256::sha_256;
use cvm_tracing::CVM_ALLOWED;
use derived_keys::GetDerivedKeysError;
use get_protocol::dps_json::GuestStateEncryptionPolicy;
use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::GspExtendedStatusFlags;
use guid::Guid;
use mesh::MeshPayload;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::VmgsProvisioner;
use openhcl_attestation_protocol::vmgs::AES_GCM_KEY_LENGTH;
use openhcl_attestation_protocol::vmgs::AGENT_DATA_MAX_SIZE;
use openhcl_attestation_protocol::vmgs::KeyProtector;
use openhcl_attestation_protocol::vmgs::SecurityProfile;
use pal_async::local::LocalDriver;
use secure_key_release::VmgsEncryptionKeys;
use serde::Deserialize;
use serde::Serialize;
use std::fmt::Debug;
use tee_call::TeeCall;
use thiserror::Error;
use zerocopy::FromZeros;

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
    #[error("failed to read security profile from vmgs")]
    ReadSecurityProfile(#[source] vmgs::ReadFromVmgsError),
    #[error("failed to get derived keys")]
    GetDerivedKeys(#[source] GetDerivedKeysError),
    #[error("failed to read key protector from vmgs")]
    ReadKeyProtector(#[source] vmgs::ReadFromVmgsError),
    #[error("failed to read key protector by id from vmgs")]
    ReadKeyProtectorById(#[source] vmgs::ReadFromVmgsError),
    #[error("failed to unlock vmgs data store")]
    UnlockVmgsDataStore(#[source] UnlockVmgsDataStoreError),
    #[error("failed to read guest secret key from vmgs")]
    ReadGuestSecretKey(#[source] vmgs::ReadFromVmgsError),
    #[error("failed to verify VMGS provenance")]
    Provenance(#[source] ProvenanceError),
}

#[derive(Debug, Error)]
enum UnlockVmgsDataStoreError {
    #[error("failed to unlock vmgs with the existing egress key")]
    VmgsUnlockUsingExistingEgressKey(#[source] ::vmgs::Error),
    #[error("failed to unlock vmgs with the existing ingress key")]
    VmgsUnlockUsingExistingIngressKey(#[source] ::vmgs::Error),
    #[error("failed to write key protector to vmgs")]
    WriteKeyProtector(#[source] vmgs::WriteToVmgsError),
    #[error("failed to read key protector by id to vmgs")]
    WriteKeyProtectorById(#[source] vmgs::WriteToVmgsError),
    #[error("failed to update the vmgs encryption key")]
    UpdateVmgsEncryptionKey(#[source] ::vmgs::Error),
    #[error("failed to persist all key protectors")]
    PersistAllKeyProtectors(#[source] PersistAllKeyProtectorsError),
}

#[derive(Debug, Error)]
enum PersistAllKeyProtectorsError {
    #[error("failed to write key protector to vmgs")]
    WriteKeyProtector(#[source] vmgs::WriteToVmgsError),
    #[error("failed to read key protector by id to vmgs")]
    WriteKeyProtectorById(#[source] vmgs::WriteToVmgsError),
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

#[derive(Debug)]
struct Keys {
    ingress: [u8; AES_GCM_KEY_LENGTH],
    decrypt_egress: Option<[u8; AES_GCM_KEY_LENGTH]>,
    encrypt_egress: [u8; AES_GCM_KEY_LENGTH],
}

/// Actions to apply to the on-disk key protector blobs once the unlock
/// strategy has been chosen by [`get_derived_keys`].
///
/// [`get_derived_keys`]: derived_keys::get_derived_keys
#[derive(Clone, Copy, Default)]
struct KeyProtectorActions {
    /// Write the rotated [`KeyProtector`] back to VMGS.
    should_write_kp: bool,
    /// Update the per-VM-id key protector entry.
    use_gsp_by_id: bool,
    /// True when the VMGS was unlocked using a hardware-sealed key. The
    /// in-memory [`KeyProtector`] must not be altered in this case.
    use_hardware_unlock: bool,
}

/// Records which GSP type was used for each side of the key rotation.
/// Used solely for observability tracing in [`initialize_platform_security`].
#[derive(Clone, Copy)]
struct GspTypeRecord {
    /// GSP type used to decrypt the existing (ingress) VMGS contents.
    decrypt: GspType,
    /// GSP type used to encrypt the new (egress) VMGS contents.
    encrypt: GspType,
}

impl Default for GspTypeRecord {
    fn default() -> Self {
        Self {
            decrypt: GspType::None,
            encrypt: GspType::None,
        }
    }
}

/// In-memory representation of the per-VM-id key protector entry stored in
/// VMGS.
///
/// On first boot (or when the entry has never been written) the entry is
/// absent. The orchestration code distinguishes the two states explicitly
/// rather than relying on "all-zeros means not found" sentinel values.
enum KeyProtectorById {
    /// The entry was loaded from VMGS.
    Found(openhcl_attestation_protocol::vmgs::KeyProtectorById),
    /// No entry was present in VMGS.
    NotFound,
}

impl KeyProtectorById {
    /// Returns the on-disk id, or [`Guid::ZERO`] when the entry was not
    /// found. Used by callers that need to compare against the current
    /// `bios_guid` or detect an unprovisioned slot.
    fn id_guid(&self) -> Guid {
        match self {
            Self::Found(inner) => inner.id_guid,
            Self::NotFound => Guid::ZERO,
        }
    }

    /// Borrow the inner protocol struct mutably, transitioning from
    /// [`Self::NotFound`] to [`Self::Found`] (with a freshly-zeroed inner)
    /// when needed. Used by call sites that are about to write the entry
    /// back to VMGS, since the on-disk write implies the entry now exists.
    fn ensure_found_mut(&mut self) -> &mut openhcl_attestation_protocol::vmgs::KeyProtectorById {
        if matches!(self, Self::NotFound) {
            *self = Self::Found(openhcl_attestation_protocol::vmgs::KeyProtectorById::new_zeroed());
        }
        let Self::Found(inner) = self else {
            unreachable!("transitioned to Found above")
        };
        inner
    }

    /// Test helper that extracts the inner protocol struct, panicking on
    /// [`Self::NotFound`]. Used by tests that assert the in-memory entry
    /// matches what was just written to VMGS.
    #[cfg(test)]
    fn inner_for_test(&self) -> &openhcl_attestation_protocol::vmgs::KeyProtectorById {
        let Self::Found(inner) = self else {
            panic!("expected KeyProtectorById::Found");
        };
        inner
    }
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
    /// Actions for the orchestration code to apply to the on-disk key
    /// protector blobs.
    actions: KeyProtectorActions,
    /// Observability record of which GSP types were used (logging only).
    gsp_types: GspTypeRecord,
    /// The instance of [`GspExtendedStatusFlags`] returned by GSP.
    gsp_extended_status_flags: GspExtendedStatusFlags,
}

/// The return values of [`initialize_platform_security`].
pub struct PlatformAttestationData {
    /// The instance of [`HostAttestationSettings`].
    pub host_attestation_settings: HostAttestationSettings,
    /// The agent data used by an attestation request.
    pub agent_data: Option<Vec<u8>>,
    /// The guest secret key.
    pub guest_secret_key: Option<Vec<u8>>,
}

/// An error paired with a retry hint.
///
/// Used by attestation flows where some failures are transient (host service
/// unavailable, TDX service VM not yet ready, dynamic firmware update in
/// flight) and the caller should retry, while other failures are fatal and
/// must be propagated immediately.
#[derive(Debug)]
pub(crate) struct Retryable<E> {
    /// The underlying error.
    pub error: E,
    /// Whether the operation can be retried.
    pub can_retry: bool,
}

impl<E> Retryable<E> {
    /// Wraps a fatal error that should not be retried.
    pub(crate) fn fatal(error: E) -> Self {
        Self {
            error,
            can_retry: false,
        }
    }

    /// Wraps an error with the given retry hint.
    pub(crate) fn with_retry(error: E, can_retry: bool) -> Self {
        Self { error, can_retry }
    }
}

/// The attestation type to use.
// TODO: Support VBS
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
///
/// On success, returns a bool indicating whether igvmagent requested a
/// state refresh. On failure, returns a [`Retryable`] carrying the error
/// and whether the caller should retry.
async fn try_unlock_vmgs(
    get: &GuestEmulationTransportClient,
    bios_guid: Guid,
    attestation_vm_config: &AttestationVmConfig,
    vmgs: &mut Vmgs,
    tee_call: Option<&dyn TeeCall>,
    guest_state_encryption_policy: GuestStateEncryptionPolicy,
    strict_encryption_policy: bool,
    agent_data: &mut [u8; AGENT_DATA_MAX_SIZE],
    key_protector_by_id: &mut KeyProtectorById,
) -> Result<bool, Retryable<AttestationErrorInner>> {
    let skr_response = if let Some(tee_call) = tee_call {
        tracing::info!(CVM_ALLOWED, "Retrieving key-encryption key");

        // Retrieve the tenant key via attestation
        secure_key_release::request_vmgs_encryption_keys(
            get,
            tee_call,
            vmgs,
            attestation_vm_config,
            agent_data,
        )
        .await
    } else {
        tracing::info!(CVM_ALLOWED, "Key-encryption key retrieval not required");

        // Attestation is unavailable, assume no tenant key
        Ok(VmgsEncryptionKeys::default())
    };

    let retry = match &skr_response {
        Ok(_) => false,
        Err(r) => r.can_retry,
    };

    let skip_hw_unsealing = matches!(
        &skr_response,
        Err(Retryable {
            error: secure_key_release::RequestVmgsEncryptionKeysError::ParseIgvmAttestKeyReleaseResponse(
                igvm_attest::key_release::KeyReleaseError::ParseHeader(
                    igvm_attest::Error::Attestation {
                        skip_hw_unsealing_signal: true,
                        ..
                    },
                ),
            ),
            ..
        })
    );

    let VmgsEncryptionKeys {
        ingress_rsa_kek,
        wrapped_des_key,
        tcb_version,
    } = match skr_response {
        Ok(k) => {
            tracing::info!(CVM_ALLOWED, "Successfully retrieved key-encryption key");
            k
        }
        Err(Retryable { error, .. }) => {
            // Non-fatal, allowing for hardware-based recovery
            tracing::error!(
                CVM_ALLOWED,
                error = &error as &dyn std::error::Error,
                "Failed to retrieve key-encryption key"
            );

            VmgsEncryptionKeys::default()
        }
    };

    // Determine the minimal size of a DEK entry based on whether `wrapped_des_key` presents
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
    let mut key_protector = vmgs::read_key_protector(vmgs, dek_minimal_size)
        .await
        .map_err(|e| Retryable::fatal(AttestationErrorInner::ReadKeyProtector(e)))?;

    let start_time = std::time::SystemTime::now();
    let vmgs_encrypted = vmgs.encrypted();
    tracing::info!(
        ?tcb_version,
        vmgs_encrypted,
        op_type = ?LogOpType::BeginDecryptVmgs,
        "Deriving keys"
    );

    let derived_keys_result = derived_keys::get_derived_keys(
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
        tcb_version,
        guest_state_encryption_policy,
        strict_encryption_policy,
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
        Retryable::with_retry(AttestationErrorInner::GetDerivedKeys(e), retry)
    })?;

    // All Underhill VMs use VMGS encryption
    tracing::info!("Unlocking VMGS");
    if let Err(e) = unlock_vmgs_data_store(
        vmgs,
        vmgs_encrypted,
        &mut key_protector,
        key_protector_by_id,
        derived_keys_result.derived_keys,
        derived_keys_result.actions,
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

        return Err(Retryable::with_retry(
            AttestationErrorInner::UnlockVmgsDataStore(e),
            retry,
        ));
    }

    tracing::info!(
        CVM_ALLOWED,
        op_type = ?LogOpType::DecryptVmgs,
        success = true,
        decrypt_gsp_type = ?derived_keys_result.gsp_types.decrypt,
        encrypt_gsp_type = ?derived_keys_result.gsp_types.encrypt,
        latency = std::time::SystemTime::now().duration_since(start_time).map_or(0, |d| d.as_millis()),
        "Unlocked datastore"
    );

    Ok(derived_keys_result
        .gsp_extended_status_flags
        .state_refresh_request())
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
    // Maximum number of attempts when the VMGS is encrypted and the
    // attestation call-out may transiently fail (IGVm agent down for
    // servicing, TDX service VM not ready, or a dynamic firmware update
    // means the report is not verifiable yet).
    const ENCRYPTED_VMGS_MAX_ATTEMPTS: usize = 10;
    // When the VMGS is not encrypted there is no benefit to retrying;
    // make a single attempt and surface the error.
    const UNENCRYPTED_VMGS_MAX_ATTEMPTS: usize = 1;

    tracing::info!(CVM_ALLOWED,
        tee_type=?tee_call.map(|tee| tee.tee_type()),
        secure_boot=attestation_vm_config.secure_boot,
        tpm_enabled=attestation_vm_config.tpm_enabled,
        tpm_persisted=attestation_vm_config.tpm_persisted,
        "Reading security profile");

    // Read Security Profile from VMGS
    // Currently this only includes "Key Reference" data, which is not attested data, is opaque to the
    // OpenHCL, and is passed to the IGVMm agent outside of the report contents.
    let SecurityProfile { mut agent_data } = vmgs::read_security_profile(vmgs)
        .await
        .map_err(AttestationErrorInner::ReadSecurityProfile)?;

    // If attestation is suppressed, return the `agent_data` that is required by
    // TPM AK cert request.
    if suppress_attestation {
        tracing::info!(CVM_ALLOWED, "Suppressing attestation");

        return Ok(PlatformAttestationData {
            host_attestation_settings: HostAttestationSettings {
                refresh_tpm_seeds: false,
            },
            agent_data: Some(agent_data.to_vec()),
            guest_secret_key: None,
        });
    }

    // Read VM id from VMGS
    tracing::info!(CVM_ALLOWED, "Reading VM ID from VMGS");
    let mut key_protector_by_id = match vmgs::read_key_protector_by_id(vmgs).await {
        Ok(inner) => KeyProtectorById::Found(inner),
        Err(vmgs::ReadFromVmgsError::EntryNotFound(_)) => KeyProtectorById::NotFound,
        Err(e) => return Err(AttestationErrorInner::ReadKeyProtectorById(e).into()),
    };

    // Check if the VM id has been changed since last boot with KP write
    let vm_id_changed = if let KeyProtectorById::Found(inner) = &key_protector_by_id {
        let changed = inner.id_guid != bios_guid;
        if changed {
            tracing::info!(CVM_ALLOWED, "VM Id has changed since last boot");
        };
        changed
    } else {
        // Previous id in KP not found means this is the first boot or the GspById
        // is not provisioned, treat id as unchanged for this case.
        false
    };

    // Retry attestation call-out if necessary (if VMGS encrypted).
    // The IGVm Agent could be down for servicing, or the TDX service VM might not be ready, or a dynamic firmware
    // update could mean that the report was not verifiable.
    let vmgs_encrypted: bool = vmgs.encrypted();
    let max_retry = if vmgs_encrypted {
        ENCRYPTED_VMGS_MAX_ATTEMPTS
    } else {
        UNENCRYPTED_VMGS_MAX_ATTEMPTS
    };

    let mut timer = pal_async::timer::PolledTimer::new(&driver);
    let mut i = 0;

    let state_refresh_request_from_gsp = loop {
        tracing::info!(CVM_ALLOWED, attempt = i, "attempt to unlock VMGS file");

        let response = try_unlock_vmgs(
            get,
            bios_guid,
            attestation_vm_config,
            vmgs,
            tee_call,
            guest_state_encryption_policy,
            strict_encryption_policy,
            &mut agent_data,
            &mut key_protector_by_id,
        )
        .await;

        match response {
            Ok(b) => break b,
            Err(Retryable {
                error,
                can_retry: false,
            }) => return Err(error.into()),
            Err(Retryable {
                error,
                can_retry: true,
            }) => {
                if i >= max_retry - 1 {
                    return Err(error.into());
                }
            }
        }

        // Stall on retries
        timer.sleep(std::time::Duration::new(1, 0)).await;
        i += 1;
    };

    let host_attestation_settings = HostAttestationSettings {
        refresh_tpm_seeds: state_refresh_request_from_gsp || vm_id_changed,
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
    derived_keys: Option<Keys>,
    actions: KeyProtectorActions,
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
                return Err(UnlockVmgsDataStoreError::VmgsUnlockUsingExistingIngressKey(
                    e,
                ));
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
        should_write_kp = actions.should_write_kp,
        use_gsp_by_id = actions.use_gsp_by_id,
        use_hardware_unlock = actions.use_hardware_unlock,
        "key protector settings"
    );

    if actions.should_write_kp {
        // Update on disk KP with all seeds used, to allow for disaster recovery
        vmgs::write_key_protector(key_protector, vmgs)
            .await
            .map_err(UnlockVmgsDataStoreError::WriteKeyProtector)?;

        if actions.use_gsp_by_id {
            vmgs::write_key_protector_by_id(
                key_protector_by_id.ensure_found_mut(),
                vmgs,
                false,
                bios_guid,
            )
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
    persist_all_key_protectors(vmgs, key_protector, key_protector_by_id, bios_guid, actions)
        .await
        .map_err(UnlockVmgsDataStoreError::PersistAllKeyProtectors)
}

/// Update Key Protector to remove 2nd protector, and write to VMGS
async fn persist_all_key_protectors(
    vmgs: &mut Vmgs,
    key_protector: &mut KeyProtector,
    key_protector_by_id: &mut KeyProtectorById,
    bios_guid: Guid,
    actions: KeyProtectorActions,
) -> Result<(), PersistAllKeyProtectorsError> {
    use openhcl_attestation_protocol::vmgs::NUMBER_KP;

    if actions.use_gsp_by_id && !actions.should_write_kp {
        vmgs::write_key_protector_by_id(
            key_protector_by_id.ensure_found_mut(),
            vmgs,
            false,
            bios_guid,
        )
        .await
        .map_err(PersistAllKeyProtectorsError::WriteKeyProtectorById)?;
    } else {
        // If HW Key unlocked VMGS, do not alter KP
        if !actions.use_hardware_unlock {
            // Remove ingress KP & DEK, no longer applies to data store
            key_protector.dek[key_protector.active_kp as usize % NUMBER_KP]
                .dek_buffer
                .fill(0);
            key_protector.gsp[key_protector.active_kp as usize % NUMBER_KP].gsp_length = 0;
            key_protector.active_kp += 1;

            vmgs::write_key_protector(key_protector, vmgs)
                .await
                .map_err(PersistAllKeyProtectorsError::WriteKeyProtector)?;
        }

        // Update Id data to indicate this scheme is no longer in use
        if !actions.use_gsp_by_id
            && let KeyProtectorById::Found(inner) = key_protector_by_id
            && inner.ported == 0
        {
            inner.ported = 1;
            vmgs::write_key_protector_by_id(inner, vmgs, true, bios_guid)
                .await
                .map_err(PersistAllKeyProtectorsError::WriteKeyProtectorById)?;
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
    let signer = format!("did:x509:0:sha256:{}:subject:{}", hex::encode(digest), sn);
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
