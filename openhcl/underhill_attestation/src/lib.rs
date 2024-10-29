// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This modules implements attestation protocols for Underhill to support TVM
//! and CVM, including getting a tenant key via secure key release (SKR) for
//! unlocking VMGS and requesting an attestation key (AK) certificate for TPM.
//! The module also implements the VMGS unlocking process based on SKR.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]
#![forbid(unsafe_code)]

mod crypto;
mod hardware_key_sealing;
mod igvm_attest;
mod key_protector;
mod protocol;
mod secure_key_release;
mod vmgs;

pub use igvm_attest::ak_cert::parse_response as parse_ak_cert_response;
pub use igvm_attest::Error as IgvmAttestError;
pub use igvm_attest::IgvmAttestRequestHelper;
pub use protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;

use ::vmgs::EncryptionAlgorithm;
use ::vmgs::Vmgs;
use cvm_tracing::CVM_ALLOWED;
use guest_emulation_transport::api::GspExtendedStatusFlags;
use guest_emulation_transport::api::GuestStateProtection;
use guest_emulation_transport::api::GuestStateProtectionById;
use guest_emulation_transport::GuestEmulationTransportClient;
use guid::Guid;
use hardware_key_sealing::HardwareDerivedKeys;
use hardware_key_sealing::HardwareKeyProtectorExt as _;
use key_protector::GetKeysFromKeyProtectorError;
use key_protector::KeyProtectorExt as _;
use mesh::MeshPayload;
use openssl::pkey::Private;
use openssl::rsa::Rsa;
use pal_async::local::LocalDriver;
use protocol::vmgs::HardwareKeyProtector;
use protocol::vmgs::SecurityProfile;
use protocol::vmgs::AES_GCM_KEY_LENGTH;
use secure_key_release::VmgsEncryptionKeys;
use static_assertions::const_assert_eq;
use std::any::Any;
use std::fmt::Debug;
use tee_call::TeeCall;
use thiserror::Error;
use zerocopy::AsBytes;
use zerocopy::FromZeroes;

/// An attestation error.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(AttestationErrorInner);

impl Error {
    fn log_attestation_error<T: Any + Debug>(filter: tracing::field::Empty, e: &T) {
        let value_any = e as &dyn Any;
        let type_name = std::any::type_name::<T>();

        match value_any.downcast_ref::<String>() {
            Some(as_string) => {
                tracing::error!(filter, "Unexpected string {:?}", as_string);
            }
            None => {
                tracing::error!(filter, "AttestationErrorId::{type_name}::{e:?}");
            }
        }

        match value_any.downcast_ref::<AttestationErrorInner>() {
            Some(attestation_error) => {
                tracing::error!(filter, "Error message: {attestation_error}");
            }
            None => {
                tracing::error!(filter, "Not of type AttestationErrorInner");
            }
        }
    }
}

impl<T: Into<AttestationErrorInner>> From<T> for Error {
    fn from(value: T) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Error)]
enum AttestationErrorInner {
    #[error("read security profile from vmgs")]
    ReadSecurityProfile(#[source] vmgs::ReadFromVmgsError),
    #[error("failed to request vmgs encryption keys")]
    RequestVmgsEncryptionKeys(#[source] secure_key_release::RequestVmgsEncryptionKeysError),
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
}

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
enum GetDerivedKeysError {
    #[error("failed to get ingress/egress keys from the the key protector")]
    GetKeysFromKeyProtector(#[source] GetKeysFromKeyProtectorError),
    #[error("failed to fetch GSP")]
    FetchGuestStateProtectionById(
        #[source] guest_emulation_transport::error::GuestStateProtectionByIdError,
    ),
    #[error("GSP By Id required, but no GSP By Id found")]
    GspByIdRequiredButNotFound,
    #[error("failed to unseal the ingress key using hardware derived keys")]
    UnsealIngressKeyUsingHardwareDerivedKeys(
        #[source] hardware_key_sealing::HardwareKeySealingError,
    ),
    #[error("failed to get an ingress key from key protector")]
    GetIngressKeyFromKpFailed,
    #[error("failed to get an ingress key from guest state protection")]
    GetIngressKeyFromKGspFailed,
    #[error("failed to get an ingress key from guest state protection by id")]
    GetIngressKeyFromKGspByIdFailed,
    #[error("Encryption cannot be disabled if VMGS was previously encrypted")]
    DisableVmgsEncryptionFailed,
    #[error("failed to seal the egress key using hardware derived keys")]
    SealEgressKeyUsingHardwareDerivedKeys(#[source] hardware_key_sealing::HardwareKeySealingError),
    #[error("failed to write to `FileId::HW_KEY_PROTECTOR` in vmgs")]
    VmgsWriteHardwareKeyProtector(#[source] vmgs::WriteToVmgsError),
    #[error("failed to get derived key by id")]
    GetDerivedKeyById(#[source] GetDerivedKeysByIdError),
    #[error("failed to derive an ingress key")]
    DeriveIngressKey(#[source] crypto::KbkdfError),
    #[error("failed to derive an egress key")]
    DeriveEgressKey(#[source] crypto::KbkdfError),
}

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
enum GetDerivedKeysByIdError {
    #[error("failed to derive an egress key based on current vm bios guid")]
    DeriveEgressKeyUsingCurrentVmId(#[source] crypto::KbkdfError),
    #[error("invalid derived egress key size {key_size}, expected {expected_size}")]
    InvalidDerivedEgressKeySize {
        key_size: usize,
        expected_size: usize,
    },
    #[error("failed to derive an ingress key based on key protector Id from vmgs")]
    DeriveIngressKeyUsingKeyProtectorId(#[source] crypto::KbkdfError),
    #[error("invalid derived egress key size {key_size}, expected {expected_size}")]
    InvalidDerivedIngressKeySize {
        key_size: usize,
        expected_size: usize,
    },
}

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
enum UnlockVmgsDataStoreError {
    #[error("failed to unlock vmgs with the new ingress key")]
    VmgsUnlockUsingNewIngressKey(#[source] ::vmgs::Error),
    #[error("failed to unlock vmgs with the existing ingress key")]
    VmgsUnlockUsingExistingIngressKey(#[source] ::vmgs::Error),
    #[error("failed to write key protector to vmgs")]
    WriteKeyProtector(#[source] vmgs::WriteToVmgsError),
    #[error("failed to read key protector by id to vmgs")]
    WriteKeyProtectorById(#[source] vmgs::WriteToVmgsError),
    #[error("failed to remove the old vmgs encryption key")]
    RemoveOldVmgsEncryptionKey(#[source] ::vmgs::Error),
    #[error("failed to add a new vmgs encryption key after removing the old key")]
    AddNewVmgsEncryptionKeyAfterRemoval(#[source] ::vmgs::Error),
    #[error("failed to add a new vmgs encryption key")]
    AddNewVmgsEncryptionKey(#[source] ::vmgs::Error),
    #[error("failed to persist all key protectors")]
    PersistAllKeyProtectors(#[source] PersistAllKeyProtectorsError),
}

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
enum PersistAllKeyProtectorsError {
    #[error("failed to write key protector to vmgs")]
    WriteKeyProtector(#[source] vmgs::WriteToVmgsError),
    #[error("failed to read key protector by id to vmgs")]
    WriteKeyProtectorById(#[source] vmgs::WriteToVmgsError),
}

/// Label used by `derive_key`
const VMGS_KEY_DERIVE_LABEL: &[u8; 7] = b"VMGSKEY";

#[derive(Debug)]
struct Keys {
    ingress: [u8; AES_GCM_KEY_LENGTH],
    egress: [u8; AES_GCM_KEY_LENGTH],
}

/// Key protector settings
struct KeyProtectorSettings {
    /// Whether to update key protector
    should_write_kp: bool,
    /// Whether GSP by id is used
    use_gsp_by_id: bool,
    /// Whether hardware key sealing is used
    use_hardware_unlock: bool,
}

/// Helper struct for [`protocol::vmgs::KeyProtectorById`]
struct KeyProtectorById {
    /// The instance of [`protocol::vmgs::KeyProtectorById`].
    pub inner: protocol::vmgs::KeyProtectorById,
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

/// The attestation type to use.
#[derive(Debug, MeshPayload, Copy, Clone, PartialEq, Eq)]
pub enum AttestationType {
    /// Use the SEV-SNP TEE for attestation.
    Snp,
    /// Use the TDX TEE for attestation.
    Tdx,
    /// Use trusted host-based attestation.
    Host,
    /// CVM without supported attestation.
    Unsupported,
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
    attestation_type: AttestationType,
    suppress_attestation: bool,
    driver: LocalDriver,
) -> Result<PlatformAttestationData, Error> {
    // Read Security Profile from VMGS
    // Currently this only includes "Key Reference" data, which is not attested data, is opaque to the
    // Underhill, and is passed to the IGVMm agent outside of the report contents.
    let SecurityProfile { mut agent_data } =
        vmgs::read_security_profile(vmgs).await.map_err(|e| {
            Error::log_attestation_error(CVM_ALLOWED, &e);
            tracing::error!("failed to read security profile");
            AttestationErrorInner::ReadSecurityProfile(e)
        })?;

    // If attestation is suppressed, return the `agent_data` that is required by
    // TPM AK cert request.
    if suppress_attestation {
        tracing::info!(
            CVM_ALLOWED,
            "Suppressing attestation, assuming unlocked vmgs and stateless tpm"
        );

        return Ok(PlatformAttestationData {
            host_attestation_settings: HostAttestationSettings {
                refresh_tpm_seeds: false,
            },
            agent_data: Some(agent_data.to_vec()),
            guest_secret_key: None,
        });
    }

    let tee_call: Option<Box<dyn TeeCall>> = match attestation_type {
        AttestationType::Snp => Some(Box::new(tee_call::SnpCall)),
        AttestationType::Tdx => Some(Box::new(tee_call::TdxCall)),
        AttestationType::Host | AttestationType::Unsupported => None,
    };

    tracing::info!(
        CVM_ALLOWED,
        "Initializing attestation config for platform type {:?}",
        attestation_type
    );

    let VmgsEncryptionKeys {
        ingress_rsa_kek,
        wrapped_des_key,
        tcb_version,
    } = if let Some(tee_call) = tee_call.as_ref() {
        tracing::info!(CVM_ALLOWED, "Retrieving key-encryption key");

        // Retrieve the tenant key via attestation
        secure_key_release::request_vmgs_encryption_keys(
            get,
            tee_call.as_ref(),
            vmgs,
            attestation_vm_config,
            &mut agent_data,
            driver,
        )
        .await
        .map_err(|e| {
            Error::log_attestation_error(CVM_ALLOWED, &e);
            tracing::error!("failed to retrieve key-encryption key");
            AttestationErrorInner::RequestVmgsEncryptionKeys(e)
        })?
    } else {
        tracing::info!(CVM_ALLOWED, "Key-encryption key retrieval not required");

        // Attestation is unavailable, assume no tenant key
        VmgsEncryptionKeys::default()
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
        dek_minimal_size,
        "Reading key protector from VMGS"
    );
    let mut key_protector = vmgs::read_key_protector(vmgs, dek_minimal_size)
        .await
        .map_err(|e| {
            Error::log_attestation_error(CVM_ALLOWED, &e);
            tracing::error!("failed to read key protector blob from VMGS");
            AttestationErrorInner::ReadKeyProtector(e)
        })?;

    // Read VM id from VMGS
    tracing::info!(CVM_ALLOWED, "Reading VM ID from VMGS");
    let mut key_protector_by_id = match vmgs::read_key_protector_by_id(vmgs).await {
        Ok(key_protector_by_id) => KeyProtectorById {
            inner: key_protector_by_id,
            found_id: true,
        },
        Err(vmgs::ReadFromVmgsError::EntryNotFound(_)) => KeyProtectorById {
            inner: protocol::vmgs::KeyProtectorById::new_zeroed(),
            found_id: false,
        },
        Err(e) => {
            Error::log_attestation_error(CVM_ALLOWED, &e);
            tracing::error!("failed to read VM Id from VMGS");
            Err(AttestationErrorInner::ReadKeyProtectorById(e))
        }?,
    };

    // Check if the VM id has been changed since last boot with KP write
    let vm_id_changed = if key_protector_by_id.found_id {
        key_protector_by_id.inner.id_guid != bios_guid
    } else {
        // Previous id in KP not found means this is the first boot,
        // treat id as unchanged for this case.
        false
    };

    let vmgs_encrypted: bool = vmgs.get_encryption_algorithm() != EncryptionAlgorithm::NONE;

    tracing::info!("Deriving keys. Tcb version is {:X?}", tcb_version);
    let derived_keys_result = get_derived_keys(
        get,
        tee_call.as_deref(),
        vmgs,
        &mut key_protector,
        &mut key_protector_by_id,
        bios_guid,
        attestation_vm_config,
        vmgs_encrypted,
        ingress_rsa_kek.as_ref(),
        wrapped_des_key.as_deref(),
        tcb_version,
    )
    .await
    .map_err(|e| {
        Error::log_attestation_error(CVM_ALLOWED, &e);
        tracing::error!("failed to derive keys");
        AttestationErrorInner::GetDerivedKeys(e)
    })?;

    // All Underhill VMs use VMGS encryption
    tracing::info!("Unlocking VMGS");
    if let Err(e) = unlock_vmgs_data_store(
        vmgs,
        vmgs_encrypted,
        &mut key_protector,
        &mut key_protector_by_id,
        derived_keys_result.derived_keys,
        derived_keys_result.key_protector_settings,
        bios_guid,
    )
    .await
    {
        get.event_log_fatal(guest_emulation_transport::api::EventLogId::ATTESTATION_FAILED)
            .await;

        Error::log_attestation_error(CVM_ALLOWED, &e);
        tracing::error!("failed to unlock VMGS");
        Err(AttestationErrorInner::UnlockVmgsDataStore(e))?
    }

    let state_refresh_request_from_gsp = derived_keys_result
        .gsp_extended_status_flags
        .state_refresh_request();

    let host_attestation_settings = HostAttestationSettings {
        refresh_tpm_seeds: { state_refresh_request_from_gsp | vm_id_changed },
    };

    tracing::info!(
        CVM_ALLOWED,
        state_refresh_request_from_gsp,
        vm_id_changed,
        "determine if refreshing tpm seeds is needed"
    );

    // Read guest secret key from unlocked VMGS
    let guest_secret_key = match vmgs::read_guest_secret_key(vmgs).await {
        Ok(data) => Some(data.guest_secret_key.to_vec()),
        Err(vmgs::ReadFromVmgsError::EntryNotFound(_)) => None,
        Err(e) => {
            let return_e = AttestationErrorInner::ReadGuestSecretKey(e).into();
            Error::log_attestation_error(CVM_ALLOWED, &return_e);
            tracing::error!("failed to read secret key from unlocked VMGS");
            return Err(return_e);
        }
    };

    Ok(PlatformAttestationData {
        host_attestation_settings,
        agent_data: Some(agent_data.to_vec()),
        guest_secret_key,
    })
}

/// Get ingress and egress keys for the VMGS, unlock VMGS,
/// remove old key if necessary, and update KP.
async fn unlock_vmgs_data_store(
    vmgs: &mut Vmgs,
    vmgs_encrypted: bool,
    key_protector: &mut protocol::vmgs::KeyProtector,
    key_protector_by_id: &mut KeyProtectorById,
    derived_keys: Option<Keys>,
    key_protector_settings: KeyProtectorSettings,
    bios_guid: Guid,
) -> Result<(), UnlockVmgsDataStoreError> {
    let mut new_key = false; // Indicate if we need to add a new key after unlock

    let Some(Keys {
        ingress: new_ingress_key,
        egress: new_egress_key,
    }) = derived_keys
    else {
        tracing::info!(
            CVM_ALLOWED,
            "Encryption disabled, skipping unlock vmgs data store"
        );
        return Ok(());
    };

    if new_ingress_key != new_egress_key {
        tracing::trace!(CVM_ALLOWED, "EgressKey is different than IngressKey");
        new_key = true;
    }

    // Call unlock_with_encryption_key using ingress_key if datastore is encrypted
    let mut old_index = 2;
    let mut provision = false;
    if vmgs_encrypted {
        tracing::info!(CVM_ALLOWED, "Decrypting vmgs file...");
        match vmgs.unlock_with_encryption_key(&new_ingress_key).await {
            Ok(index) => old_index = index,
            Err(e) if new_key => {
                // If last time is provisioning and we failed to persist KP then we'll come here.
                tracing::trace!(
                    CVM_ALLOWED,
                    error = &e as &dyn std::error::Error,
                    "Unlock with ingress key error"
                );
                // The datastore can be unlocked using EgressKey
                old_index = vmgs
                    .unlock_with_encryption_key(&new_egress_key)
                    .await
                    .map_err(UnlockVmgsDataStoreError::VmgsUnlockUsingNewIngressKey)?;
                new_key = false;
            }
            Err(e) => Err(UnlockVmgsDataStoreError::VmgsUnlockUsingExistingIngressKey(
                e,
            ))?,
        }
    } else {
        // The datastore is not encrypted which means it's during provision.
        tracing::info!(
            CVM_ALLOWED,
            "vmgs data store is not encrypted, provisioning."
        );
        provision = true;
    }

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

    // Call add_new_encryption_key adding egress_key if different with ingress_key or during provision
    if provision || new_key {
        let result = vmgs
            .add_new_encryption_key(&new_egress_key, EncryptionAlgorithm::AES_GCM)
            .await;

        match result {
            Ok(_new_index) => (),
            Err(_) if old_index != 2 => {
                // If last time we failed to remove old key then we'll come here.
                // We have to remove old key before adding egress_key.
                let key_index = if old_index == 0 { 1 } else { 0 };
                tracing::trace!(CVM_ALLOWED, key_index, "Remove old key...");
                vmgs.remove_encryption_key(key_index)
                    .await
                    .map_err(UnlockVmgsDataStoreError::RemoveOldVmgsEncryptionKey)?;

                tracing::trace!(CVM_ALLOWED, "Add egress_key again...");
                vmgs.add_new_encryption_key(&new_egress_key, EncryptionAlgorithm::AES_GCM)
                    .await
                    .map_err(UnlockVmgsDataStoreError::AddNewVmgsEncryptionKeyAfterRemoval)?;
            }
            Err(e) => Err(UnlockVmgsDataStoreError::AddNewVmgsEncryptionKey(e))?,
        }
    }

    // Remove ingress_key if different with egress_key
    if !provision && new_key {
        vmgs.remove_encryption_key(old_index)
            .await
            .map_err(UnlockVmgsDataStoreError::RemoveOldVmgsEncryptionKey)?;
    }

    // Persist KP to VMGS
    persist_all_key_protectors(
        vmgs,
        key_protector,
        key_protector_by_id,
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
async fn get_derived_keys(
    get: &GuestEmulationTransportClient,
    tee_call: Option<&dyn TeeCall>,
    vmgs: &mut Vmgs,
    key_protector: &mut protocol::vmgs::KeyProtector,
    key_protector_by_id: &mut KeyProtectorById,
    bios_guid: Guid,
    attestation_vm_config: &AttestationVmConfig,
    is_encrypted: bool,
    ingress_rsa_kek: Option<&Rsa<Private>>,
    wrapped_des_key: Option<&[u8]>,
    tcb_version: Option<u64>,
) -> Result<DerivedKeyResult, GetDerivedKeysError> {
    let mut key_protector_settings = KeyProtectorSettings {
        should_write_kp: true,
        use_gsp_by_id: false,
        use_hardware_unlock: false,
    };

    let mut derived_keys = Keys {
        ingress: [0u8; AES_GCM_KEY_LENGTH],
        egress: [0u8; AES_GCM_KEY_LENGTH],
    };

    // Ingress / Egress seed values depend on what happened previously to the datastore
    let ingress_idx = (key_protector.active_kp % 2) as usize;
    let egress_idx = if ingress_idx == 0 { 1 } else { 0 } as usize;

    let found_dek = !key_protector.dek[ingress_idx]
        .dek_buffer
        .iter()
        .all(|&x| x == 0);

    // Handle key released via attestation process (tenant key) to get keys from KeyProtector
    let (ingress_key, egress_key, no_kek) = if let Some(ingress_kek) = ingress_rsa_kek {
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
        (keys.ingress, keys.egress, false)
    } else {
        ([0u8; AES_GCM_KEY_LENGTH], [0u8; AES_GCM_KEY_LENGTH], true)
    };

    // Handle various sources of Guest State Protection
    let mut requires_gsp_by_id =
        key_protector_by_id.found_id && key_protector_by_id.inner.ported != 1;

    // Attempt GSP
    let (gsp_response, no_gsp, requires_gsp) = {
        let found_kp = key_protector.gsp[ingress_idx].gsp_length != 0;

        let response = get_gsp_data(get, key_protector).await;

        let no_gsp =
            response.extended_status_flags.no_rpc_server() || response.encrypted_gsp.length == 0;

        let requires_gsp = found_kp || response.extended_status_flags.requires_rpc_server();

        // If the VMGS is encrypted, but no key protection data is found,
        // assume GspById encryption is enabled, but no ID file was written.
        if is_encrypted && !requires_gsp_by_id && !requires_gsp && !found_dek {
            requires_gsp_by_id = true;
        }

        (response, no_gsp, requires_gsp)
    };

    // Attempt GSP By Id protection if GSP is not available, or when changing schemes.
    let (gsp_response_by_id, no_gsp_by_id) = if no_gsp || requires_gsp_by_id {
        let gsp_response_by_id = get
            .guest_state_protection_data_by_id()
            .await
            .map_err(GetDerivedKeysError::FetchGuestStateProtectionById)?;

        let no_gsp_by_id = gsp_response_by_id.extended_status_flags.no_registry_file();

        if no_gsp_by_id && requires_gsp_by_id {
            Err(GetDerivedKeysError::GspByIdRequiredButNotFound)?
        }

        (gsp_response_by_id, no_gsp_by_id)
    } else {
        (GuestStateProtectionById::new_zeroed(), true)
    };

    // If sources of encryption used last are missing, attempt to unseal VMGS key with hardware key
    if (no_kek && found_dek) || (no_gsp && requires_gsp) || (no_gsp_by_id && requires_gsp_by_id) {
        tracing::info!("Unseal VMGS key-encryption key with hardware key");
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
                    match HardwareDerivedKeys::derive_key(
                        tee_call,
                        attestation_vm_config,
                        hardware_key_protector.header.tcb_version,
                    ) {
                        Ok(hardware_derived_key) => Some(hardware_derived_key),
                        Err(e) => {
                            // non-fatal
                            tracing::warn!(
                                CVM_ALLOWED,
                                error = &e as &dyn std::error::Error,
                                "failed to derive hardware keys using HW_KEY_PROTECTOR",
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            });

            (hardware_key_protector, hardware_derived_keys)
        } else {
            (None, None)
        };

        if let (Some(hardware_key_protector), Some(hardware_derived_keys)) =
            (hardware_key_protector, hardware_derived_keys)
        {
            derived_keys.ingress = hardware_key_protector
                .unseal_key(&hardware_derived_keys)
                .map_err(GetDerivedKeysError::UnsealIngressKeyUsingHardwareDerivedKeys)?;
            derived_keys.egress = derived_keys.ingress;

            key_protector_settings.should_write_kp = false;
            key_protector_settings.use_hardware_unlock = true;

            tracing::warn!(CVM_ALLOWED, "Using hardware based key derivation");

            return Ok(DerivedKeyResult {
                derived_keys: Some(derived_keys),
                key_protector_settings,
                gsp_extended_status_flags: gsp_response.extended_status_flags,
            });
        } else {
            if no_kek && found_dek {
                Err(GetDerivedKeysError::GetIngressKeyFromKpFailed)?
            } else if no_gsp && requires_gsp {
                Err(GetDerivedKeysError::GetIngressKeyFromKGspFailed)?
            } else {
                // no_gsp_by_id && requires_gsp_by_id
                Err(GetDerivedKeysError::GetIngressKeyFromKGspByIdFailed)?
            }
        }
    }

    // Check if sources of encryption are available
    if no_kek && no_gsp && no_gsp_by_id {
        if is_encrypted {
            Err(GetDerivedKeysError::DisableVmgsEncryptionFailed)?
        }

        tracing::trace!(CVM_ALLOWED, "No VMGS encryption used.");

        return Ok(DerivedKeyResult {
            derived_keys: None,
            key_protector_settings,
            gsp_extended_status_flags: gsp_response.extended_status_flags,
        });
    }

    // Attempt to get hardware derived keys
    let hardware_derived_keys = tee_call
        .and_then(|tee_call| tee_call.supports_get_derived_key())
        .and_then(|tee_call| {
            if let Some(tcb_version) = tcb_version {
                match HardwareDerivedKeys::derive_key(tee_call, attestation_vm_config, tcb_version)
                {
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

    // Use tenant key (KEK only)
    if no_gsp && no_gsp_by_id {
        tracing::trace!(CVM_ALLOWED, "No GSP used with SKR");

        derived_keys.ingress = ingress_key;
        derived_keys.egress = egress_key;

        if let Some(hardware_derived_keys) = hardware_derived_keys {
            let hardware_key_protector =
                HardwareKeyProtector::seal_key(&hardware_derived_keys, &derived_keys.egress)
                    .map_err(GetDerivedKeysError::SealEgressKeyUsingHardwareDerivedKeys)?;
            vmgs::write_hardware_key_protector(&hardware_key_protector, vmgs)
                .await
                .map_err(GetDerivedKeysError::VmgsWriteHardwareKeyProtector)?;

            tracing::info!(CVM_ALLOWED, "hardware key protector updated (no GSP used)");
        }

        return Ok(DerivedKeyResult {
            derived_keys: Some(derived_keys),
            key_protector_settings,
            gsp_extended_status_flags: gsp_response.extended_status_flags,
        });
    }

    // GSP By Id derives keys differently,
    // because key is shared across VMs different context must be used (Id GUID)
    if (no_kek && no_gsp) || requires_gsp_by_id {
        let derived_keys_by_id =
            get_derived_keys_by_id(key_protector_by_id, bios_guid, gsp_response_by_id)
                .map_err(GetDerivedKeysError::GetDerivedKeyById)?;

        if no_kek && no_gsp {
            tracing::trace!(CVM_ALLOWED, "Using GSP with ID.");

            // Not required for Id protection
            key_protector_settings.should_write_kp = false;
            key_protector_settings.use_gsp_by_id = true;

            return Ok(DerivedKeyResult {
                derived_keys: Some(derived_keys_by_id),
                key_protector_settings,
                gsp_extended_status_flags: gsp_response.extended_status_flags,
            });
        }

        derived_keys.ingress = derived_keys_by_id.ingress;

        tracing::trace!(CVM_ALLOWED, "Converting GSP method.");
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
            } else {
                derived_keys.ingress = ingress_key;
            }
        }

        // Choose best available egress seed
        if no_gsp {
            egress_seed =
                gsp_response_by_id.seed.buffer[..gsp_response_by_id.seed.length as usize].to_vec();
            key_protector_settings.use_gsp_by_id = true;
        } else {
            egress_seed =
                gsp_response.new_gsp.buffer[..gsp_response.new_gsp.length as usize].to_vec();
        }
    } else {
        // `no_gsp` is false, using `gsp_response`

        if gsp_response.decrypted_gsp[ingress_idx].length == 0
            && gsp_response.decrypted_gsp[egress_idx].length == 0
        {
            tracing::trace!(CVM_ALLOWED, "Applying GSP.");

            // VMGS has never had any GSP applied.
            // Leave ingress key untouched, derive egress key with new seed.
            egress_seed =
                gsp_response.new_gsp.buffer[..gsp_response.new_gsp.length as usize].to_vec();

            // Ingress key is either zero or tenant only.
            // Only copy in the case where a tenant key was released.
            if !no_kek {
                derived_keys.ingress = ingress_key;
            }
        } else {
            tracing::trace!(CVM_ALLOWED, "Using GSP.");

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
            }
        }
    }

    // Derive key used to lock data store previously
    if let Some(seed) = ingress_seed {
        derived_keys.ingress = crypto::derive_key(&ingress_key, &seed, VMGS_KEY_DERIVE_LABEL)
            .map_err(GetDerivedKeysError::DeriveIngressKey)?;
    }

    // Always derive a new egress key using best available seed
    derived_keys.egress = crypto::derive_key(&egress_key, &egress_seed, VMGS_KEY_DERIVE_LABEL)
        .map_err(GetDerivedKeysError::DeriveEgressKey)?;

    if key_protector_settings.should_write_kp {
        // Update with all seeds used, but do not write until data store is unlocked
        key_protector.gsp[egress_idx]
            .gsp_buffer
            .copy_from_slice(&gsp_response.encrypted_gsp.buffer);
        key_protector.gsp[egress_idx].gsp_length = gsp_response.encrypted_gsp.length;

        if let Some(hardware_derived_keys) = hardware_derived_keys {
            let hardware_key_protector =
                HardwareKeyProtector::seal_key(&hardware_derived_keys, &derived_keys.egress)
                    .map_err(GetDerivedKeysError::SealEgressKeyUsingHardwareDerivedKeys)?;

            vmgs::write_hardware_key_protector(&hardware_key_protector, vmgs)
                .await
                .map_err(GetDerivedKeysError::VmgsWriteHardwareKeyProtector)?;

            tracing::info!(CVM_ALLOWED, "hardware key protector updated");
        }
    }

    Ok(DerivedKeyResult {
        derived_keys: Some(derived_keys),
        key_protector_settings,
        gsp_extended_status_flags: gsp_response.extended_status_flags,
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
    let new_egress_key = crypto::derive_key(
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
        crypto::derive_key(
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
        egress: new_egress_key,
    })
}

/// Prepare the request payload and request GSP from the host via GET.
async fn get_gsp_data(
    get: &GuestEmulationTransportClient,
    key_protector: &mut protocol::vmgs::KeyProtector,
) -> GuestStateProtection {
    use protocol::vmgs::GSP_BUFFER_SIZE;
    use protocol::vmgs::NUMBER_KP;

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
    key_protector: &mut protocol::vmgs::KeyProtector,
    key_protector_by_id: &mut KeyProtectorById,
    bios_guid: Guid,
    key_protector_settings: KeyProtectorSettings,
) -> Result<(), PersistAllKeyProtectorsError> {
    use protocol::vmgs::NUMBER_KP;

    if key_protector_settings.use_gsp_by_id && !key_protector_settings.should_write_kp {
        vmgs::write_key_protector_by_id(&mut key_protector_by_id.inner, vmgs, false, bios_guid)
            .await
            .map_err(PersistAllKeyProtectorsError::WriteKeyProtectorById)?;
    } else {
        // If HW Key unlocked VMGS, do not alter KP
        if !key_protector_settings.use_hardware_unlock {
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
        if !key_protector_settings.use_gsp_by_id
            && key_protector_by_id.found_id
            && key_protector_by_id.inner.ported == 0
        {
            key_protector_by_id.inner.ported = 1;
            vmgs::write_key_protector_by_id(&mut key_protector_by_id.inner, vmgs, true, bios_guid)
                .await
                .map_err(PersistAllKeyProtectorsError::WriteKeyProtectorById)?;
        }
    }

    Ok(())
}
