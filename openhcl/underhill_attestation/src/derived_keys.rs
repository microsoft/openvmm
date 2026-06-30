// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMGS encryption-key derivation: orchestrates the multi-source key flow
//! (tenant KEK + key-based GSP + ID-based GSP + hardware sealing) that
//! produces ingress/egress data-encryption keys (DEKs) for the VMGS data
//! store.
//!
//! [`get_derived_keys`] is the single entry point; the other items in this
//! module are private helpers that decompose the flow into focused steps.

use crate::DerivedKeyResult;
use crate::GspTypeRecord;
use crate::KeyProtectorActions;
use crate::KeyProtectorById;
use crate::Keys;
use crate::LogOpType;
use crate::hardware_key_sealing::HardwareDerivedKeys;
use crate::hardware_key_sealing::HardwareKeyProtectorExt as _;
use crate::key_protector;
use crate::key_protector::GetKeysFromKeyProtectorError;
use crate::key_protector::KeyProtectorExt as _;
use crate::vmgs;
use ::vmgs::GspType;
use ::vmgs::Vmgs;
use crypto::rsa::RsaKeyPair;
use cvm_tracing::CVM_ALLOWED;
use get_protocol::dps_json::GuestStateEncryptionPolicy;
use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::GspExtendedStatusFlags;
use guest_emulation_transport::api::GuestStateProtection;
use guest_emulation_transport::api::GuestStateProtectionById;
use guid::Guid;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_attestation_protocol::vmgs::AES_GCM_KEY_LENGTH;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtector;
use openhcl_attestation_protocol::vmgs::KeyProtector;
use static_assertions::const_assert_eq;
use tee_call::TeeCall;
use thiserror::Error;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub(crate) enum GetDerivedKeysError {
    #[error("failed to get ingress/egress keys from the key protector")]
    GetKeysFromKeyProtector(#[source] GetKeysFromKeyProtectorError),
    #[error("failed to fetch GSP")]
    FetchGuestStateProtectionById(
        #[source] guest_emulation_transport::error::GuestStateProtectionByIdError,
    ),
    #[error("GSP by id required, but no GSP by id found")]
    GspByIdRequiredButNotFound,
    #[error("failed to unseal the ingress key using hardware derived keys")]
    UnsealIngressKeyUsingHardwareDerivedKeys(
        #[source] crate::hardware_key_sealing::HardwareKeySealingError,
    ),
    #[error("failed to get an ingress key from key protector")]
    GetIngressKeyFromKpFailed,
    #[error("failed to get an ingress key from guest state protection")]
    GetIngressKeyFromGspFailed,
    #[error("failed to get an ingress key from guest state protection by id")]
    GetIngressKeyFromGspByIdFailed,
    #[error("encryption cannot be disabled if VMGS was previously encrypted")]
    DisableVmgsEncryptionFailed,
    #[error("VMGS encryption is required, but no encryption sources were found")]
    EncryptionRequiredButNotFound,
    #[error("failed to seal the egress key using hardware derived keys")]
    SealEgressKeyUsingHardwareDerivedKeys(
        #[source] crate::hardware_key_sealing::HardwareKeySealingError,
    ),
    #[error("failed to write to `FileId::HW_KEY_PROTECTOR` in vmgs")]
    VmgsWriteHardwareKeyProtector(#[source] vmgs::WriteToVmgsError),
    #[error("failed to get derived key by id")]
    GetDerivedKeyById(#[source] GetDerivedKeysByIdError),
    #[error("failed to derive an ingress key")]
    DeriveIngressKey(#[source] crypto::kbkdf::KbkdfError),
    #[error("failed to derive an egress key")]
    DeriveEgressKey(#[source] crypto::kbkdf::KbkdfError),
}

#[derive(Debug, Error)]
pub(crate) enum GetDerivedKeysByIdError {
    #[error("failed to derive an egress key based on current vm bios guid")]
    DeriveEgressKeyUsingCurrentVmId(#[source] crypto::kbkdf::KbkdfError),
    #[error("failed to derive an ingress key based on key protector Id from vmgs")]
    DeriveIngressKeyUsingKeyProtectorId(#[source] crypto::kbkdf::KbkdfError),
}

/// Label used by [`derive_key`].
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

/// Which encryption sources are usable to (un)lock the VMGS in the current
/// boot, after applying both source availability and the active
/// [`GuestStateEncryptionPolicy`].
///
/// Each field is `true` only when the source is both physically available
/// (e.g. RPC server responded, registry file exists) and not disabled by
/// policy.
#[derive(Clone, Copy, Default)]
struct EncryptionSources {
    /// A tenant key (KEK) was released and can unwrap the DEK.
    kek: bool,
    /// Key-based Guest State Protection is usable.
    gsp: bool,
    /// ID-based Guest State Protection (GSP By Id) is usable.
    gsp_by_id: bool,
}

impl EncryptionSources {
    /// Returns `true` when no encryption source is usable.
    fn none(&self) -> bool {
        !self.kek && !self.gsp && !self.gsp_by_id
    }
}

/// State of the VMGS observed at the start of [`get_derived_keys`].
#[derive(Clone, Copy)]
struct InitialVmgsEncryptionState {
    /// VMGS reports itself as encrypted.
    is_encrypted: bool,
    /// Encrypted GSP data is present in the active key protector slot.
    is_gsp: bool,
    /// A non-ported GSP-By-Id key protector entry was found for this VM.
    is_gsp_by_id: bool,
    /// The VMGS was not encrypted and was not provisioned this boot.
    existing_unencrypted: bool,
    /// A non-empty DEK is present in the active key protector slot.
    found_dek: bool,
}

/// Result of [`attempt_gsp`].
struct GspAttempt {
    response: GuestStateProtection,
    /// True when an RPC server responded with non-zero-length GSP data.
    available: bool,
    /// True when GSP is usable under the current policy.
    active: bool,
    /// True when GSP must be used (existing GSP in KP, RPC server requires
    /// it, or strict policy with `GspKey`).
    requires: bool,
    /// True when the VMGS is encrypted but no protector data is found; the
    /// caller should also require GSP By Id.
    force_gsp_by_id: bool,
}

/// Result of [`attempt_gsp_by_id`].
struct GspByIdAttempt {
    response: GuestStateProtectionById,
    /// Source availability; `None` if the source was not queried.
    available: Option<bool>,
    /// True when GSP By Id is usable under the current policy.
    active: bool,
}

/// Tenant keys produced by [`unwrap_kek_keys`].
struct UnwrappedKekKeys {
    ingress_key: [u8; AES_GCM_KEY_LENGTH],
    decrypt_egress_key: Option<[u8; AES_GCM_KEY_LENGTH]>,
    encrypt_egress_key: [u8; AES_GCM_KEY_LENGTH],
    /// `true` when a tenant key was successfully unwrapped.
    kek_active: bool,
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
pub(crate) async fn get_derived_keys(
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
    tcb_version: Option<u64>,
    guest_state_encryption_policy: GuestStateEncryptionPolicy,
    strict_encryption_policy: bool,
    skip_hw_unsealing: bool,
) -> Result<DerivedKeyResult, GetDerivedKeysError> {
    tracing::info!(
        CVM_ALLOWED,
        ?guest_state_encryption_policy,
        strict_encryption_policy,
        "encryption policy"
    );

    // TODO: implement hardware sealing only
    if matches!(
        guest_state_encryption_policy,
        GuestStateEncryptionPolicy::HardwareSealing
    ) {
        todo!("hardware sealing")
    }

    let mut actions = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: false,
        use_hardware_unlock: false,
    };
    let mut gsp_types = GspTypeRecord::default();

    let mut derived_keys = Keys {
        ingress: [0u8; AES_GCM_KEY_LENGTH],
        decrypt_egress: None,
        encrypt_egress: [0u8; AES_GCM_KEY_LENGTH],
    };

    // Ingress / Egress seed values depend on what happened previously to the datastore
    let ingress_idx = (key_protector.active_kp % 2) as usize;
    let egress_idx = ingress_idx ^ 1;

    let found_dek = key_protector::dek_is_present(&key_protector.dek[ingress_idx]);

    // Handle key released via attestation process (tenant key) to get keys from KeyProtector
    let UnwrappedKekKeys {
        ingress_key,
        mut decrypt_egress_key,
        encrypt_egress_key,
        kek_active,
    } = unwrap_kek_keys(
        get,
        key_protector,
        ingress_rsa_kek,
        wrapped_des_key,
        ingress_idx,
        egress_idx,
    )
    .await?;

    // Handle various sources of Guest State Protection
    let state = InitialVmgsEncryptionState {
        is_encrypted,
        is_gsp: key_protector.gsp[ingress_idx].gsp_length != 0,
        is_gsp_by_id: matches!(
            key_protector_by_id,
            KeyProtectorById::Found(inner) if inner.ported != 1,
        ),
        existing_unencrypted: !vmgs.encrypted() && !vmgs.was_provisioned_this_boot(),
        found_dek,
    };
    tracing::info!(
        CVM_ALLOWED,
        is_encrypted = state.is_encrypted,
        is_gsp_by_id = state.is_gsp_by_id,
        is_gsp = state.is_gsp,
        found_dek = state.found_dek,
        "initial vmgs encryption state"
    );
    let mut requires_gsp_by_id = state.is_gsp_by_id;

    // Attempt GSP
    let gsp = attempt_gsp(
        get,
        key_protector,
        ingress_idx,
        guest_state_encryption_policy,
        strict_encryption_policy,
        state,
        requires_gsp_by_id,
    )
    .await;
    if gsp.force_gsp_by_id {
        requires_gsp_by_id = true;
    }

    // Attempt GSP By Id protection if GSP is not available, when changing
    // schemes, or as requested
    let gsp_by_id = if !gsp.active || requires_gsp_by_id {
        attempt_gsp_by_id(
            get,
            guest_state_encryption_policy,
            strict_encryption_policy,
            state.existing_unencrypted,
            requires_gsp_by_id,
        )
        .await?
    } else {
        GspByIdAttempt {
            response: GuestStateProtectionById::new_zeroed(),
            available: None,
            active: false,
        }
    };

    let sources = EncryptionSources {
        kek: kek_active,
        gsp: gsp.active,
        gsp_by_id: gsp_by_id.active,
    };

    // If sources of encryption used last are missing, attempt to unseal VMGS key with hardware key
    if (!sources.kek && found_dek)
        || (!sources.gsp && gsp.requires)
        || (!sources.gsp_by_id && requires_gsp_by_id)
    {
        if let Some(ingress) = try_hardware_unseal_ingress_key(
            get,
            vmgs,
            tee_call,
            attestation_vm_config,
            skip_hw_unsealing,
        )
        .await?
        {
            derived_keys.ingress = ingress;
            derived_keys.decrypt_egress = None;
            derived_keys.encrypt_egress = ingress;

            actions.should_write_kp = false;
            actions.use_hardware_unlock = true;

            tracing::warn!(
                CVM_ALLOWED,
                "Using hardware-derived key to recover VMGS DEK"
            );

            return Ok(DerivedKeyResult {
                derived_keys: Some(derived_keys),
                actions,
                gsp_types,
                gsp_extended_status_flags: gsp.response.extended_status_flags,
            });
        }

        return Err(if !sources.kek && found_dek {
            GetDerivedKeysError::GetIngressKeyFromKpFailed
        } else if !sources.gsp && gsp.requires {
            GetDerivedKeysError::GetIngressKeyFromGspFailed
        } else {
            // !sources.gsp_by_id && requires_gsp_by_id
            GetDerivedKeysError::GetIngressKeyFromGspByIdFailed
        });
    }

    tracing::info!(
        CVM_ALLOWED,
        kek = sources.kek,
        gsp_available = gsp.available,
        gsp = sources.gsp,
        gsp_by_id_available = ?gsp_by_id.available,
        gsp_by_id = sources.gsp_by_id,
        "Encryption sources"
    );

    // Check if sources of encryption are available
    if sources.none() {
        if is_encrypted {
            return Err(GetDerivedKeysError::DisableVmgsEncryptionFailed);
        }
        match guest_state_encryption_policy {
            // fail if some minimum level of encryption was required
            GuestStateEncryptionPolicy::GspById
            | GuestStateEncryptionPolicy::GspKey
            | GuestStateEncryptionPolicy::HardwareSealing => {
                return Err(GetDerivedKeysError::EncryptionRequiredButNotFound);
            }
            GuestStateEncryptionPolicy::Auto | GuestStateEncryptionPolicy::None => {
                tracing::info!(CVM_ALLOWED, "No VMGS encryption used.");

                return Ok(DerivedKeyResult {
                    derived_keys: None,
                    actions,
                    gsp_types,
                    gsp_extended_status_flags: gsp.response.extended_status_flags,
                });
            }
        }
    }

    // Attempt to get hardware derived keys
    let hardware_derived_keys =
        try_derive_hardware_keys(tee_call, attestation_vm_config, tcb_version);

    // Use tenant key (KEK only)
    if !sources.gsp && !sources.gsp_by_id {
        tracing::info!(CVM_ALLOWED, "No GSP used with SKR");

        derived_keys.ingress = ingress_key;
        derived_keys.decrypt_egress = decrypt_egress_key;
        derived_keys.encrypt_egress = encrypt_egress_key;

        if let Some(hardware_derived_keys) = hardware_derived_keys {
            let hardware_key_protector = HardwareKeyProtector::seal_key(
                &hardware_derived_keys,
                &derived_keys.encrypt_egress,
            )
            .map_err(GetDerivedKeysError::SealEgressKeyUsingHardwareDerivedKeys)?;
            vmgs::write_hardware_key_protector(&hardware_key_protector, vmgs)
                .await
                .map_err(GetDerivedKeysError::VmgsWriteHardwareKeyProtector)?;

            tracing::info!(CVM_ALLOWED, "hardware key protector updated (no GSP used)");
        }

        return Ok(DerivedKeyResult {
            derived_keys: Some(derived_keys),
            actions,
            gsp_types,
            gsp_extended_status_flags: gsp.response.extended_status_flags,
        });
    }

    // GSP By Id derives keys differently,
    // because key is shared across VMs different context must be used (Id GUID)
    if (!sources.kek && !sources.gsp) || requires_gsp_by_id {
        let derived_keys_by_id =
            get_derived_keys_by_id(key_protector_by_id, bios_guid, gsp_by_id.response)
                .map_err(GetDerivedKeysError::GetDerivedKeyById)?;

        if !sources.kek && !sources.gsp {
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
            actions.should_write_kp = false;
            actions.use_gsp_by_id = true;
            gsp_types.decrypt = GspType::GspById;
            gsp_types.encrypt = GspType::GspById;

            return Ok(DerivedKeyResult {
                derived_keys: Some(derived_keys_by_id),
                actions,
                gsp_types,
                gsp_extended_status_flags: gsp.response.extended_status_flags,
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

    if requires_gsp_by_id || !sources.gsp {
        // If DEK exists, ingress is either KEK or KEK + GSP By Id
        // If no DEK, then ingress was Gsp By Id (derived above)
        if found_dek {
            if requires_gsp_by_id {
                ingress_seed = Some(
                    gsp_by_id.response.seed.buffer[..gsp_by_id.response.seed.length as usize]
                        .to_vec(),
                );
                gsp_types.decrypt = GspType::GspById;
            } else {
                derived_keys.ingress = ingress_key;
            }
        } else {
            gsp_types.decrypt = GspType::GspById;
        }

        // Choose best available egress seed
        if !sources.gsp {
            egress_seed =
                gsp_by_id.response.seed.buffer[..gsp_by_id.response.seed.length as usize].to_vec();
            actions.use_gsp_by_id = true;
            gsp_types.encrypt = GspType::GspById;
        } else {
            egress_seed =
                gsp.response.new_gsp.buffer[..gsp.response.new_gsp.length as usize].to_vec();
            gsp_types.encrypt = GspType::GspKey;
        }
    } else {
        // `sources.gsp` is true, using `gsp.response`

        if gsp.response.decrypted_gsp[ingress_idx].length == 0
            && gsp.response.decrypted_gsp[egress_idx].length == 0
        {
            tracing::info!(CVM_ALLOWED, "Applying GSP.");

            // VMGS has never had any GSP applied.
            // Leave ingress key untouched, derive egress key with new seed.
            egress_seed =
                gsp.response.new_gsp.buffer[..gsp.response.new_gsp.length as usize].to_vec();

            // Ingress key is either zero or tenant only.
            // Only copy in the case where a tenant key was released.
            if sources.kek {
                derived_keys.ingress = ingress_key;
            }

            gsp_types.encrypt = GspType::GspKey;
        } else {
            tracing::info!(CVM_ALLOWED, "Using existing GSP.");

            ingress_seed = Some(
                gsp.response.decrypted_gsp[ingress_idx].buffer
                    [..gsp.response.decrypted_gsp[ingress_idx].length as usize]
                    .to_vec(),
            );

            if gsp.response.decrypted_gsp[egress_idx].length == 0 {
                // Derive ingress with saved seed, derive egress with new seed.
                egress_seed =
                    gsp.response.new_gsp.buffer[..gsp.response.new_gsp.length as usize].to_vec();
            } else {
                // System failed during data store unlock, and is in indeterminate state.
                // The egress key might have been applied, or the ingress key might be valid.
                // Use saved KP, derive ingress/egress keys to attempt recovery.
                // Do not update the saved KP with new seed value.
                egress_seed = gsp.response.decrypted_gsp[egress_idx].buffer
                    [..gsp.response.decrypted_gsp[egress_idx].length as usize]
                    .to_vec();
                actions.should_write_kp = false;
                decrypt_egress_key = Some(encrypt_egress_key);
            }

            gsp_types.decrypt = GspType::GspKey;
            gsp_types.encrypt = GspType::GspKey;
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

    if actions.should_write_kp {
        // Update with all seeds used, but do not write until data store is unlocked
        key_protector.gsp[egress_idx]
            .gsp_buffer
            .copy_from_slice(&gsp.response.encrypted_gsp.buffer);
        key_protector.gsp[egress_idx].gsp_length = gsp.response.encrypted_gsp.length;

        if let Some(hardware_derived_keys) = hardware_derived_keys {
            let hardware_key_protector = HardwareKeyProtector::seal_key(
                &hardware_derived_keys,
                &derived_keys.encrypt_egress,
            )
            .map_err(GetDerivedKeysError::SealEgressKeyUsingHardwareDerivedKeys)?;

            vmgs::write_hardware_key_protector(&hardware_key_protector, vmgs)
                .await
                .map_err(GetDerivedKeysError::VmgsWriteHardwareKeyProtector)?;

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
        actions,
        gsp_types,
        gsp_extended_status_flags: gsp.response.extended_status_flags,
    })
}

/// Unwrap and rotate the keys stored in `key_protector` using
/// `ingress_rsa_kek`, or return all-zero keys when no tenant key is
/// available. On DEK or DES unwrap failure, emits the corresponding host
/// event before propagating the error.
async fn unwrap_kek_keys(
    get: &GuestEmulationTransportClient,
    key_protector: &mut KeyProtector,
    ingress_rsa_kek: Option<&RsaKeyPair>,
    wrapped_des_key: Option<&[u8]>,
    ingress_idx: usize,
    egress_idx: usize,
) -> Result<UnwrappedKekKeys, GetDerivedKeysError> {
    let Some(ingress_kek) = ingress_rsa_kek else {
        return Ok(UnwrappedKekKeys {
            ingress_key: [0u8; AES_GCM_KEY_LENGTH],
            decrypt_egress_key: None,
            encrypt_egress_key: [0u8; AES_GCM_KEY_LENGTH],
            kek_active: false,
        });
    };

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
            get.event_log_fatal(guest_emulation_transport::api::EventLogId::DEK_DECRYPTION_FAILED)
                .await;
            return Err(GetDerivedKeysError::GetKeysFromKeyProtector(e));
        }
        Err(e) => return Err(GetDerivedKeysError::GetKeysFromKeyProtector(e)),
    };
    Ok(UnwrappedKekKeys {
        ingress_key: keys.ingress,
        decrypt_egress_key: keys.decrypt_egress,
        encrypt_egress_key: keys.encrypt_egress,
        kek_active: true,
    })
}

/// Query the host for key-based Guest State Protection and decide whether
/// it can be used under the current policy.
async fn attempt_gsp(
    get: &GuestEmulationTransportClient,
    key_protector: &mut KeyProtector,
    ingress_idx: usize,
    guest_state_encryption_policy: GuestStateEncryptionPolicy,
    strict_encryption_policy: bool,
    state: InitialVmgsEncryptionState,
    requires_gsp_by_id: bool,
) -> GspAttempt {
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
        ) && (state.is_gsp_by_id || state.existing_unencrypted))
        // disable per encryption policy (first boot only, unless strict)
        || (matches!(
            guest_state_encryption_policy,
            GuestStateEncryptionPolicy::GspById | GuestStateEncryptionPolicy::None
        ) && (!state.is_gsp || strict_encryption_policy));

    let requires_gsp = state.is_gsp
        || response.extended_status_flags.requires_rpc_server()
        || (matches!(
            guest_state_encryption_policy,
            GuestStateEncryptionPolicy::GspKey
        ) && strict_encryption_policy);

    // If the VMGS is encrypted, but no key protection data is found,
    // assume GspById encryption is enabled, but no ID file was written.
    let force_gsp_by_id =
        state.is_encrypted && !requires_gsp_by_id && !requires_gsp && !state.found_dek;

    GspAttempt {
        response,
        available: !no_gsp_available,
        active: !no_gsp,
        requires: requires_gsp,
        force_gsp_by_id,
    }
}

/// Query the host for ID-based Guest State Protection and decide whether
/// it can be used under the current policy.
async fn attempt_gsp_by_id(
    get: &GuestEmulationTransportClient,
    guest_state_encryption_policy: GuestStateEncryptionPolicy,
    strict_encryption_policy: bool,
    existing_unencrypted: bool,
    requires_gsp_by_id: bool,
) -> Result<GspByIdAttempt, GetDerivedKeysError> {
    tracing::info!(CVM_ALLOWED, "attempting GSP By Id");

    let response = get
        .guest_state_protection_data_by_id()
        .await
        .map_err(GetDerivedKeysError::FetchGuestStateProtectionById)?;

    let no_gsp_by_id_available = response.extended_status_flags.no_registry_file();

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
        return Err(GetDerivedKeysError::GspByIdRequiredButNotFound);
    }

    Ok(GspByIdAttempt {
        response,
        available: Some(!no_gsp_by_id_available),
        active: !no_gsp_by_id,
    })
}

/// Try to recover the ingress VMGS DEK using a hardware-sealed key
/// protector when sources of encryption used previously are missing.
///
/// Returns:
/// - `Ok(Some(key))` — hardware unseal succeeded; the returned key can be
///   used directly as the ingress DEK.
/// - `Ok(None)` — no usable hardware sealing material is available; the
///   caller must surface a scheme-specific error.
/// - `Err(_)` — hardware unsealing was attempted but failed.
async fn try_hardware_unseal_ingress_key(
    get: &GuestEmulationTransportClient,
    vmgs: &mut Vmgs,
    tee_call: Option<&dyn TeeCall>,
    attestation_vm_config: &AttestationVmConfig,
    skip_hw_unsealing: bool,
) -> Result<Option<[u8; AES_GCM_KEY_LENGTH]>, GetDerivedKeysError> {
    let Some(tee_call) = tee_call else {
        return Ok(None);
    };

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

    let tcb_version = hardware_key_protector
        .as_ref()
        .map(|kp| kp.header.tcb_version);
    let hardware_derived_keys =
        try_derive_hardware_keys(Some(tee_call), attestation_vm_config, tcb_version);

    // When the IGVM agent signals skip_hw_unsealing, force both
    // hardware_key_protector and hardware_derived_keys to None so the
    // caller falls through to the scheme-specific error. When hardware
    // sealing keys were actually available, emit a warning and a host
    // event that make the skip visible.
    let (hardware_key_protector, hardware_derived_keys) = if skip_hw_unsealing {
        if hardware_key_protector.is_some() && hardware_derived_keys.is_some() {
            tracing::warn!(
                CVM_ALLOWED,
                "Skipping hardware unsealing of VMGS DEK as signaled by IGVM agent"
            );
            get.event_log_fatal(
                guest_emulation_transport::api::EventLogId::DEK_HARDWARE_UNSEALING_SKIPPED,
            )
            .await;
        } else {
            tracing::info!(
                CVM_ALLOWED,
                hardware_key_protector = hardware_key_protector.is_some(),
                hardware_derived_keys = hardware_derived_keys.is_some(),
                "skip_hw_unsealing signaled but hardware key data not available, \
                 falling through to scheme-specific error"
            );
        }
        (None, None)
    } else {
        (hardware_key_protector, hardware_derived_keys)
    };

    let (Some(hardware_key_protector), Some(hardware_derived_keys)) =
        (hardware_key_protector, hardware_derived_keys)
    else {
        return Ok(None);
    };

    let ingress = hardware_key_protector
        .unseal_key(&hardware_derived_keys)
        .map_err(GetDerivedKeysError::UnsealIngressKeyUsingHardwareDerivedKeys)?;
    Ok(Some(ingress))
}

/// Derive hardware-sealing keys via the active [`TeeCall`] if the platform
/// supports it. Returns `None` (and logs a warning) on failure; missing
/// `tee_call` or `tcb_version` returns `None` silently.
fn try_derive_hardware_keys(
    tee_call: Option<&dyn TeeCall>,
    attestation_vm_config: &AttestationVmConfig,
    tcb_version: Option<u64>,
) -> Option<HardwareDerivedKeys> {
    let tee_call = tee_call?.supports_get_derived_key()?;
    let tcb_version = tcb_version?;
    match HardwareDerivedKeys::derive_key(tee_call, attestation_vm_config, tcb_version) {
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
}

/// Update data store keys with key protectors based on VmUniqueId & host seed.
pub(crate) fn get_derived_keys_by_id(
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

    // Ingress values depend on what happened previously to the datastore.
    // If not previously encrypted (no saved Id), then Ingress Key not required.
    let new_ingress_key = if key_protector_by_id.id_guid() != Guid::ZERO {
        // Derive key used to lock data store previously
        derive_key(
            &gsp_response_by_id.seed.buffer[..gsp_response_by_id.seed.length as usize],
            key_protector_by_id.id_guid().as_bytes(),
            VMGS_KEY_DERIVE_LABEL,
        )
        .map_err(GetDerivedKeysByIdError::DeriveIngressKeyUsingKeyProtectorId)?
    } else {
        // If data store is not encrypted, Ingress should equal Egress
        new_egress_key
    };

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

    for (i, gsp) in encrypted_gsp.iter_mut().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::new_key_protector_by_id;
    use get_protocol::GSP_CLEARTEXT_MAX;
    use pal_async::async_test;

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
        let mut key_protector_by_id = new_key_protector_by_id(None, None, true);
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
        assert!(matches!(
            derived_keys_response,
            Err(GetDerivedKeysByIdError::DeriveEgressKeyUsingCurrentVmId(_))
        ));
    }
}
