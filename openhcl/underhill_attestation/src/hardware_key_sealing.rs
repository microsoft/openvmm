// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of key derivation using hardware secret and the VMGS data encryption key (DEK)
//! sealing using the derived key. The sealed DEK is written to the `FileId::HW_KEY_PROTECTOR`
//! entry of the VMGS file, which can be unsealed later.

use cvm_tracing::CVM_ALLOWED;
use openhcl_attestation_protocol::igvm_attest;
use openhcl_attestation_protocol::vmgs;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtector;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtectorV3;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtectorV4;
use thiserror::Error;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub(crate) enum HardwareDerivedKeysError {
    #[error("invalid key-release context hash")]
    InvalidKeyReleaseContextHash(#[source] igvm_attest::get::InvalidKeyReleaseContextHash),
    #[error("failed to serialize VM configuration for hardware key derivation")]
    SerializeVmConfig(#[source] serde_json::Error),
    #[error("key derivation policy does not match VM configuration")]
    KeyDerivationPolicyMismatch,
    #[error("failed to initialize hardware secret")]
    InitializeHardwareSecret(#[source] tee_call::Error),
    #[error("KDF derivation with hardware secret failed")]
    KdfWithHardwareSecret(#[source] crypto::kbkdf::KbkdfError),
}

#[derive(Debug, Error)]
pub(crate) enum HardwareKeySealingError {
    #[error("failed to generate hardware key protector IV: {0}")]
    Random(getrandom::Error),
    #[error("invalid hardware key protector header")]
    InvalidHeader,
    #[error("failed to encrypt the egress key")]
    EncryptEgressKey(#[source] crypto::aes_256_cbc::Aes256CbcError),
    #[error("invalid egress key encryption size {0}, expected {1}")]
    InvalidEgressKeyEncryptionSize(usize, usize),
    #[error("HMAC-SHA-256 after encryption failed")]
    HmacAfterEncrypt(#[source] crypto::hmac_sha_256::HmacSha256Error),
    #[error("HMAC-SHA-256 before decryption failed")]
    HmacBeforeDecrypt(#[source] crypto::hmac_sha_256::HmacSha256Error),
    #[error("Hardware key protector HMAC verification failed")]
    HardwareKeyProtectorHmacVerificationFailed,
    #[error("failed to decrypt the ingress key")]
    DecryptIngressKey(#[source] crypto::aes_256_cbc::Aes256CbcError),
    #[error("invalid ingress key decryption size {0}, expected {1}")]
    InvalidIngressKeyDecryptionSize(usize, usize),
}

/// Hold the hardware-derived keys.
pub struct HardwareDerivedKeys {
    policy: tee_call::KeyDerivationPolicy,
    key_release_context_hash: Option<[u8; 32]>,
    aes_key: [u8; vmgs::AES_CBC_KEY_LENGTH],
    hmac_key: [u8; vmgs::HMAC_SHA_256_KEY_LENGTH],
}

// Manually implement `Debug` to avoid leaking the secret key material
// (`aes_key`/`hmac_key`) via tracing, panic formatting, etc. Only the
// non-secret `policy` is shown; the keys are redacted.
impl std::fmt::Debug for HardwareDerivedKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HardwareDerivedKeys")
            .field("policy", &self.policy)
            .field("aes_key", &"[redacted]")
            .field("hmac_key", &"[redacted]")
            .finish()
    }
}

impl HardwareDerivedKeys {
    /// Derive an AES and HMAC keys based on the hardware secret, VM configuration, and policy for key sealing.
    pub fn derive_key(
        tee_call: &dyn tee_call::TeeCallGetDerivedKey,
        vm_config: &igvm_attest::get::runtime_claims::AttestationVmConfig,
        policy: tee_call::KeyDerivationPolicy,
    ) -> Result<Self, HardwareDerivedKeysError> {
        // Validate before asking the hardware for a secret. In particular, an
        // empty or malformed string must never silently become a legacy KDF.
        // Accept either hex case, then canonicalize to lowercase for the KDF.
        let key_release_context_hash = vm_config
            .key_release_context_hash
            .as_deref()
            .map(igvm_attest::get::decode_key_release_context_hash)
            .transpose()
            .map_err(HardwareDerivedKeysError::InvalidKeyReleaseContextHash)?;
        let mut canonical_config = vm_config.clone();
        canonical_config.key_release_context_hash = key_release_context_hash
            .as_ref()
            .map(igvm_attest::get::encode_key_release_context_hash);
        // Preserve struct serialization (including field order) and omission
        // of None exactly; changing to a JSON map would break legacy KDF input.
        let vm_config_json = serde_json::to_string(&canonical_config)
            .map_err(HardwareDerivedKeysError::SerializeVmConfig)?;

        let mix_measurement_from_vm_config = matches!(
            vm_config.hardware_sealing_policy,
            igvm_attest::get::runtime_claims::HardwareSealingPolicy::Hash
        );

        // Policy is based on the VM configuration (`hardware_sealing_policy`) on the
        // sealing path and on VMGS file (`HardwareKeyProtector`) on the unsealing path.
        // On both paths, the policy must be consistent with the VM configuration.
        // An inconsistency will cause mismatch in the key derivation function that takes
        // VM configuration as input.
        if policy.mix_measurement != mix_measurement_from_vm_config {
            return Err(HardwareDerivedKeysError::KeyDerivationPolicyMismatch);
        }

        let hardware_secret = tee_call
            .get_derived_key(policy)
            .map_err(HardwareDerivedKeysError::InitializeHardwareSecret)?;
        let label = b"ISOHWKEY";

        let output = crypto::kbkdf::kbkdf_hmac_sha256(
            &hardware_secret,
            vm_config_json.as_bytes(),
            label,
            vmgs::AES_CBC_KEY_LENGTH + vmgs::HMAC_SHA_256_KEY_LENGTH,
        )
        .map_err(HardwareDerivedKeysError::KdfWithHardwareSecret)?;

        let mut aes_key = [0u8; vmgs::AES_CBC_KEY_LENGTH];
        let mut hmac_key = [0u8; vmgs::HMAC_SHA_256_KEY_LENGTH];

        aes_key.copy_from_slice(&output[..vmgs::AES_CBC_KEY_LENGTH]);
        hmac_key.copy_from_slice(&output[vmgs::AES_CBC_KEY_LENGTH..]);

        tracing::info!(
            CVM_ALLOWED,
            svn = ?policy.svn,
            mix_measurement = policy.mix_measurement,
            "derived hardware AES and HMAC keys for VMGS key sealing"
        );

        Ok(Self {
            policy,
            key_release_context_hash,
            aes_key,
            hmac_key,
        })
    }
}

/// Serialize a [`tee_call::KeyDerivationSvn`] into the on-disk `(tee_type, svn)` header
/// representation.
fn key_derivation_svn_to_header(
    svn: tee_call::KeyDerivationSvn,
) -> (u32, [u8; vmgs::HW_KEY_PROTECTOR_SVN_SIZE]) {
    let mut bytes = [0u8; vmgs::HW_KEY_PROTECTOR_SVN_SIZE];
    match svn {
        tee_call::KeyDerivationSvn::Snp { tcb_version } => {
            bytes[..8].copy_from_slice(&tcb_version.to_le_bytes());
            (vmgs::HW_KEY_PROTECTOR_TEE_TYPE_SNP, bytes)
        }
        tee_call::KeyDerivationSvn::Tdx {
            tee_tcb_svn,
            cpu_svn,
        } => {
            bytes[..16].copy_from_slice(&tee_tcb_svn);
            bytes[16..].copy_from_slice(&cpu_svn);
            (vmgs::HW_KEY_PROTECTOR_TEE_TYPE_TDX, bytes)
        }
    }
}

/// Inverse of [`key_derivation_svn_to_header`].
fn key_derivation_svn_from_header(
    tee_type: u32,
    svn: [u8; vmgs::HW_KEY_PROTECTOR_SVN_SIZE],
) -> Option<tee_call::KeyDerivationSvn> {
    match tee_type {
        vmgs::HW_KEY_PROTECTOR_TEE_TYPE_SNP if svn[8..].iter().all(|&byte| byte == 0) => {
            let mut tcb = [0u8; 8];
            tcb.copy_from_slice(&svn[..8]);
            Some(tee_call::KeyDerivationSvn::Snp {
                tcb_version: u64::from_le_bytes(tcb),
            })
        }
        vmgs::HW_KEY_PROTECTOR_TEE_TYPE_TDX => {
            let mut tee_tcb_svn = [0u8; 16];
            let mut cpu_svn = [0u8; 16];
            tee_tcb_svn.copy_from_slice(&svn[..16]);
            cpu_svn.copy_from_slice(&svn[16..]);
            Some(tee_call::KeyDerivationSvn::Tdx {
                tee_tcb_svn,
                cpu_svn,
            })
        }
        _ => None,
    }
}

/// Verify the HMAC over `signed_bytes` and decrypt `iv`/`ciphertext` into the
/// ingress key. Shared by all protector layouts.
fn unseal_key_bytes(
    hardware_derived_keys: &HardwareDerivedKeys,
    signed_bytes: &[u8],
    stored_hmac: &[u8; vmgs::HMAC_SHA_256_KEY_LENGTH],
    iv: &[u8; vmgs::AES_CBC_IV_LENGTH],
    ciphertext: &[u8; vmgs::AES_GCM_KEY_LENGTH],
) -> Result<[u8; vmgs::AES_GCM_KEY_LENGTH], HardwareKeySealingError> {
    let hmac = crypto::hmac_sha_256::hmac_sha_256(&hardware_derived_keys.hmac_key, signed_bytes)
        .map_err(HardwareKeySealingError::HmacBeforeDecrypt)?;

    if !constant_time_eq::constant_time_eq_32(&hmac, stored_hmac) {
        Err(HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed)?
    }

    let mut decrypted_ingress_key = [0u8; vmgs::AES_GCM_KEY_LENGTH];
    let output = crypto::aes_256_cbc::Aes256Cbc::new(&hardware_derived_keys.aes_key)
        .and_then(|aes| aes.decrypt()?.cipher(iv, ciphertext))
        .map_err(HardwareKeySealingError::DecryptIngressKey)?;
    if output.len() != vmgs::AES_GCM_KEY_LENGTH {
        Err(HardwareKeySealingError::InvalidIngressKeyDecryptionSize(
            output.len(),
            vmgs::AES_GCM_KEY_LENGTH,
        ))?
    }
    decrypted_ingress_key.copy_from_slice(&output[..vmgs::AES_GCM_KEY_LENGTH]);

    tracing::info!(
        CVM_ALLOWED,
        "decrypt ingress_key using hardware derived key"
    );

    Ok(decrypted_ingress_key)
}

/// A hardware key protector read from the VMGS, either the legacy v1/v2 layout
/// (SNP), v3, or v4 layout.
#[derive(Debug)]
pub enum HwKeyProtector {
    /// v1/v2 layout ([`HardwareKeyProtector`]); the `version` field distinguishes.
    Legacy(HardwareKeyProtector),
    /// v3 layout ([`HardwareKeyProtectorV3`]).
    V3(HardwareKeyProtectorV3),
    /// v4 layout with an authenticated key-release context hash.
    V4(HardwareKeyProtectorV4),
}

impl HwKeyProtector {
    /// The exact on-disk bytes, without an enum discriminant.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Legacy(p) => p.as_bytes(),
            Self::V3(p) => p.as_bytes(),
            Self::V4(p) => p.as_bytes(),
        }
    }

    /// The stored context hash. This is untrusted until unsealing verifies the HMAC.
    pub fn key_release_context_hash(&self) -> Option<[u8; 32]> {
        match self {
            Self::V4(p) => Some(p.header.key_release_context_hash),
            Self::Legacy(_) | Self::V3(_) => None,
        }
    }

    /// Validate the version-specific wire contract. Legacy v1/v2 metadata keeps
    /// its historical acceptance rules; v1 still has no usable derivation policy.
    pub(crate) fn has_valid_header(&self) -> bool {
        match self {
            Self::Legacy(p) => matches!(
                p.header.version,
                vmgs::HW_KEY_PROTECTOR_VERSION_1 | vmgs::HW_KEY_PROTECTOR_VERSION_2
            ),
            Self::V3(p) => valid_v3_header(&p.header),
            Self::V4(p) => {
                p.header.version == vmgs::HW_KEY_PROTECTOR_VERSION_4
                    && p.header.length as usize == vmgs::HW_KEY_PROTECTOR_V4_SIZE
                    && p.header.mix_measurement <= 1
                    && p.header._reserved == [0; 3]
                    && key_derivation_svn_from_header(p.header.tee_type, p.header.svn).is_some()
            }
        }
    }

    /// The key derivation policy recorded in the protector, or `None` if the
    /// format is not compatible with this OpenHCL (e.g. v1, which always mixes
    /// the measurement, or an unknown version/tee-type).
    pub fn key_derivation_policy(&self) -> Option<tee_call::KeyDerivationPolicy> {
        if !self.has_valid_header() {
            return None;
        }
        match self {
            HwKeyProtector::Legacy(p) => match p.header.version {
                vmgs::HW_KEY_PROTECTOR_VERSION_2 => Some(tee_call::KeyDerivationPolicy {
                    svn: tee_call::KeyDerivationSvn::Snp {
                        tcb_version: p.header.tcb_version,
                    },
                    mix_measurement: p.header.mix_measurement != 0,
                }),
                _ => None,
            },
            HwKeyProtector::V3(p) => {
                key_derivation_svn_from_header(p.header.tee_type, p.header.svn).map(|svn| {
                    tee_call::KeyDerivationPolicy {
                        svn,
                        mix_measurement: p.header.mix_measurement != 0,
                    }
                })
            }
            HwKeyProtector::V4(p) => {
                key_derivation_svn_from_header(p.header.tee_type, p.header.svn).map(|svn| {
                    tee_call::KeyDerivationPolicy {
                        svn,
                        mix_measurement: p.header.mix_measurement != 0,
                    }
                })
            }
        }
    }

    /// Format version of the protector.
    pub fn version(&self) -> u32 {
        match self {
            HwKeyProtector::Legacy(p) => p.header.version,
            HwKeyProtector::V3(p) => p.header.version,
            HwKeyProtector::V4(p) => p.header.version,
        }
    }

    /// Unseal the `ingress_key` with verify-mac-then-decrypt.
    pub fn unseal_key(
        &self,
        hardware_derived_keys: &HardwareDerivedKeys,
    ) -> Result<[u8; vmgs::AES_GCM_KEY_LENGTH], HardwareKeySealingError> {
        if !self.has_valid_header() {
            return Err(HardwareKeySealingError::InvalidHeader);
        }
        match self {
            HwKeyProtector::Legacy(p) => {
                let offset = std::mem::offset_of!(HardwareKeyProtector, hmac);
                unseal_key_bytes(
                    hardware_derived_keys,
                    &p.as_bytes()[..offset],
                    &p.hmac,
                    &p.iv,
                    &p.ciphertext,
                )
            }
            HwKeyProtector::V3(p) => p.unseal_key(hardware_derived_keys),
            HwKeyProtector::V4(p) => unseal_key_bytes(
                hardware_derived_keys,
                &p.as_bytes()[..std::mem::offset_of!(HardwareKeyProtectorV4, hmac)],
                &p.hmac,
                &p.iv,
                &p.ciphertext,
            ),
        }
    }
}

fn valid_v3_header(header: &vmgs::HardwareKeyProtectorHeaderV3) -> bool {
    header.version == vmgs::HW_KEY_PROTECTOR_VERSION_3
        && header.length as usize == vmgs::HW_KEY_PROTECTOR_V3_SIZE
        && header.mix_measurement <= 1
        && header._reserved == [0; 3]
        && key_derivation_svn_from_header(header.tee_type, header.svn).is_some()
}

/// Seal the `egress_key` with encrypt-then-mac, using v4 when the KDF included
/// a context hash and v3 otherwise.
pub fn seal_key(
    hardware_derived_keys: &HardwareDerivedKeys,
    egress_key: &[u8],
) -> Result<HwKeyProtector, HardwareKeySealingError> {
    let (tee_type, svn) = key_derivation_svn_to_header(hardware_derived_keys.policy.svn);

    let mut iv = [0u8; vmgs::AES_CBC_IV_LENGTH];
    getrandom::fill(&mut iv).map_err(HardwareKeySealingError::Random)?;

    let mut encrypted_egress_key = [0u8; vmgs::AES_GCM_KEY_LENGTH];
    let output = crypto::aes_256_cbc::Aes256Cbc::new(&hardware_derived_keys.aes_key)
        .and_then(|aes| aes.encrypt()?.cipher(&iv, egress_key))
        .map_err(HardwareKeySealingError::EncryptEgressKey)?;
    if output.len() != vmgs::AES_GCM_KEY_LENGTH {
        Err(HardwareKeySealingError::InvalidEgressKeyEncryptionSize(
            output.len(),
            vmgs::AES_GCM_KEY_LENGTH,
        ))?
    }
    encrypted_egress_key.copy_from_slice(&output[..vmgs::AES_GCM_KEY_LENGTH]);

    let mut hardware_key_protector = match hardware_derived_keys.key_release_context_hash {
        Some(hash) => HwKeyProtector::V4(HardwareKeyProtectorV4 {
            header: vmgs::HardwareKeyProtectorHeaderV4::new(
                tee_type,
                svn,
                hardware_derived_keys.policy.mix_measurement as u8,
                hash,
            ),
            iv,
            ciphertext: encrypted_egress_key,
            hmac: [0; vmgs::HMAC_SHA_256_KEY_LENGTH],
        }),
        None => HwKeyProtector::V3(HardwareKeyProtectorV3 {
            header: vmgs::HardwareKeyProtectorHeaderV3::new(
                vmgs::HW_KEY_PROTECTOR_V3_SIZE as u32,
                tee_type,
                svn,
                hardware_derived_keys.policy.mix_measurement as u8,
            ),
            iv,
            ciphertext: encrypted_egress_key,
            hmac: [0; vmgs::HMAC_SHA_256_KEY_LENGTH],
        }),
    };
    let bytes = hardware_key_protector.as_bytes();
    let hmac = crypto::hmac_sha_256::hmac_sha_256(
        &hardware_derived_keys.hmac_key,
        &bytes[..bytes.len() - vmgs::HMAC_SHA_256_KEY_LENGTH],
    )
    .map_err(HardwareKeySealingError::HmacAfterEncrypt)?;
    match &mut hardware_key_protector {
        HwKeyProtector::Legacy(p) => p.hmac = hmac,
        HwKeyProtector::V3(p) => p.hmac = hmac,
        HwKeyProtector::V4(p) => p.hmac = hmac,
    }

    tracing::info!(CVM_ALLOWED, "encrypt egress_key using hardware derived key");

    Ok(hardware_key_protector)
}

/// Extension trait to unseal a v3 protector directly.
pub trait HardwareKeyProtectorV3Ext {
    /// Unseal the `ingress_key` with verify-mac-then-decrypt.
    fn unseal_key(
        &self,
        hardware_derived_keys: &HardwareDerivedKeys,
    ) -> Result<[u8; vmgs::AES_GCM_KEY_LENGTH], HardwareKeySealingError>;
}

impl HardwareKeyProtectorV3Ext for HardwareKeyProtectorV3 {
    fn unseal_key(
        &self,
        hardware_derived_keys: &HardwareDerivedKeys,
    ) -> Result<[u8; vmgs::AES_GCM_KEY_LENGTH], HardwareKeySealingError> {
        if !valid_v3_header(&self.header) {
            return Err(HardwareKeySealingError::InvalidHeader);
        }
        let offset = std::mem::offset_of!(HardwareKeyProtectorV3, hmac);
        unseal_key_bytes(
            hardware_derived_keys,
            &self.as_bytes()[..offset],
            &self.hmac,
            &self.iv,
            &self.ciphertext,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockTeeCall;
    use igvm_attest::get::runtime_claims::AttestationTpmVersion;
    use igvm_attest::get::runtime_claims::AttestationVmConfig;
    use igvm_attest::get::runtime_claims::HardwareSealingPolicy;
    use test_with_tracing::test;
    use zerocopy::FromBytes;

    const PLAINTEXT: [u8; 32] = [0xAB; 32];

    fn test_policy() -> tee_call::KeyDerivationPolicy {
        tee_call::KeyDerivationPolicy {
            svn: tee_call::KeyDerivationSvn::Snp { tcb_version: 2 },
            mix_measurement: false,
        }
    }

    #[test]
    fn context_hash_roundtrip_restores_kdf_and_authenticates_every_byte() {
        let mut config = create_test_vm_config(HardwareSealingPolicy::Signer);
        let hash = [0x73; 32];
        config.key_release_context_hash =
            Some(igvm_attest::get::encode_key_release_context_hash(&hash));
        let tee = MockTeeCall::new([0x7a; 32]);
        for svn in [
            test_policy().svn,
            tee_call::KeyDerivationSvn::Tdx {
                tee_tcb_svn: [0x12; 16],
                cpu_svn: [0x34; 16],
            },
        ] {
            let policy = tee_call::KeyDerivationPolicy {
                svn,
                mix_measurement: false,
            };
            let keys = HardwareDerivedKeys::derive_key(&tee, &config, policy).unwrap();
            let protector = seal_key(&keys, &PLAINTEXT).unwrap();
            assert_eq!(protector.version(), vmgs::HW_KEY_PROTECTOR_VERSION_4);
            assert_eq!(protector.as_bytes().len(), 160);
            assert_eq!(protector.key_release_context_hash(), Some(hash));
            let restored = HwKeyProtector::V4(
                HardwareKeyProtectorV4::read_from_bytes(protector.as_bytes()).unwrap(),
            );
            assert_eq!(restored.as_bytes(), protector.as_bytes());
            let restored_policy = restored.key_derivation_policy().unwrap();
            assert_eq!(
                key_derivation_svn_to_header(restored_policy.svn),
                key_derivation_svn_to_header(policy.svn)
            );
            assert_eq!(restored_policy.mix_measurement, policy.mix_measurement);

            let mut restored_config = create_test_vm_config(HardwareSealingPolicy::Signer);
            restored_config.key_release_context_hash = restored
                .key_release_context_hash()
                .as_ref()
                .map(igvm_attest::get::encode_key_release_context_hash);
            let restored_keys = HardwareDerivedKeys::derive_key(
                &tee,
                &restored_config,
                restored.key_derivation_policy().unwrap(),
            )
            .unwrap();
            assert_eq!(keys.aes_key, restored_keys.aes_key);
            assert_eq!(keys.hmac_key, restored_keys.hmac_key);
            assert_eq!(restored.unseal_key(&restored_keys).unwrap(), PLAINTEXT);

            // Exercise the MAC directly so even structurally invalid header
            // mutations prove that every header/IV/ciphertext byte is signed.
            let mac_offset = std::mem::offset_of!(HardwareKeyProtectorV4, hmac);
            for offset in 0..mac_offset {
                let mut bytes = protector.as_bytes().to_vec();
                bytes[offset] ^= 1;
                let tampered = HardwareKeyProtectorV4::read_from_bytes(&bytes).unwrap();
                assert!(matches!(
                    unseal_key_bytes(
                        &keys,
                        &bytes[..mac_offset],
                        &tampered.hmac,
                        &tampered.iv,
                        &tampered.ciphertext,
                    ),
                    Err(HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed)
                ));
            }

            let mut tampered =
                HardwareKeyProtectorV4::read_from_bytes(protector.as_bytes()).unwrap();
            tampered.header.key_release_context_hash[0] ^= 1;
            let tampered = HwKeyProtector::V4(tampered);
            assert!(matches!(
                tampered.unseal_key(&keys),
                Err(HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed)
            ));
            restored_config.key_release_context_hash = tampered
                .key_release_context_hash()
                .as_ref()
                .map(igvm_attest::get::encode_key_release_context_hash);
            let tampered_keys =
                HardwareDerivedKeys::derive_key(&tee, &restored_config, policy).unwrap();
            assert!(matches!(
                tampered.unseal_key(&tampered_keys),
                Err(HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed)
            ));
        }
    }

    #[test]
    fn context_hash_hex_casing_preserves_kdf() {
        let hash = [0xab; 32];
        let canonical = hex::encode(hash);
        let mut config = create_test_vm_config(HardwareSealingPolicy::Signer);
        config.key_release_context_hash = Some(canonical.clone());
        let tee = MockTeeCall::new([0x7a; 32]);
        let keys = HardwareDerivedKeys::derive_key(&tee, &config, test_policy()).unwrap();
        let protector = seal_key(&keys, &PLAINTEXT).unwrap();
        assert_eq!(protector.version(), vmgs::HW_KEY_PROTECTOR_VERSION_4);
        assert_eq!(protector.key_release_context_hash(), Some(hash));

        for encoded in [canonical, "AB".repeat(32), "aB".repeat(32)] {
            config.key_release_context_hash = Some(encoded.clone());
            let cased_keys = HardwareDerivedKeys::derive_key(&tee, &config, test_policy()).unwrap();
            assert_eq!(cased_keys.key_release_context_hash, Some(hash));
            assert_eq!(cased_keys.aes_key, keys.aes_key);
            assert_eq!(cased_keys.hmac_key, keys.hmac_key);
            assert_eq!(protector.unseal_key(&cased_keys).unwrap(), PLAINTEXT);
            assert_eq!(
                seal_key(&cased_keys, &PLAINTEXT)
                    .unwrap()
                    .unseal_key(&keys)
                    .unwrap(),
                PLAINTEXT
            );
            // Canonicalization must not mutate the caller's configuration.
            assert_eq!(
                config.key_release_context_hash.as_deref(),
                Some(encoded.as_str())
            );
        }
    }

    #[test]
    fn context_hash_invalid_before_hardware_derivation() {
        struct MustNotDerive;
        impl tee_call::TeeCall for MustNotDerive {
            fn get_attestation_report(
                &self,
                _: &[u8; tee_call::REPORT_DATA_SIZE],
            ) -> Result<tee_call::GetAttestationReportResult, tee_call::Error> {
                panic!("invalid context hash reached hardware attestation");
            }

            fn supports_get_derived_key(&self) -> Option<&dyn tee_call::TeeCallGetDerivedKey> {
                Some(self)
            }

            fn tee_type(&self) -> tee_call::TeeType {
                tee_call::TeeType::Snp
            }
        }
        impl tee_call::TeeCallGetDerivedKey for MustNotDerive {
            fn get_derived_key(
                &self,
                _: tee_call::KeyDerivationPolicy,
            ) -> Result<[u8; 32], tee_call::Error> {
                panic!("invalid context hash reached hardware derivation");
            }
        }
        let canonical = igvm_attest::get::encode_key_release_context_hash(&[0xff; 32]);
        for invalid in [
            String::new(),
            "not hex".to_owned(),
            "0".repeat(62),
            "0".repeat(63),
            "0".repeat(65),
            "0".repeat(66),
            canonical.replacen('f', "g", 1),
            canonical.replacen('f', " ", 1),
            "é".repeat(32),
            format!("0x{canonical}"),
            format!(" {canonical}"),
            format!("{canonical}\n"),
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".to_owned(),
            "AAAA".to_owned(),
        ] {
            let mut config = create_test_vm_config(HardwareSealingPolicy::Signer);
            config.key_release_context_hash = Some(invalid);
            assert!(matches!(
                HardwareDerivedKeys::derive_key(&MustNotDerive, &config, test_policy()),
                Err(HardwareDerivedKeysError::InvalidKeyReleaseContextHash(_))
            ));
        }
    }

    #[test]
    fn legacy_none_preserves_original_kdf_and_v2_unseal() {
        use tee_call::TeeCallGetDerivedKey;

        let config = create_test_vm_config(HardwareSealingPolicy::Signer);
        // Pin the pre-context-hash JSON, not merely the current serializer's
        // output, to detect field insertion/reordering in the legacy KDF.
        let original_json = r#"{"root-cert-thumbprint":"","console-enabled":false,"interactive-console-enabled":false,"ipmi-enabled":false,"secure-boot":false,"tpm-enabled":false,"tpm-version":"1.38","tpm-persisted":false,"filtered-vpci-devices-allowed":true,"vmUniqueId":"","hardware-sealing-policy":"signer"}"#;
        assert_eq!(serde_json::to_string(&config).unwrap(), original_json);
        let tee = MockTeeCall::new([0x7a; 32]);
        let policy = test_policy();
        let keys = HardwareDerivedKeys::derive_key(&tee, &config, policy).unwrap();
        let original_output = crypto::kbkdf::kbkdf_hmac_sha256(
            &tee.get_derived_key(policy).unwrap(),
            original_json.as_bytes(),
            b"ISOHWKEY",
            64,
        )
        .unwrap();
        assert_eq!(keys.aes_key, original_output[..32]);
        assert_eq!(keys.hmac_key, original_output[32..]);
        let protector = seal_key(&keys, &PLAINTEXT).unwrap();
        assert_eq!(protector.version(), vmgs::HW_KEY_PROTECTOR_VERSION_3);
        assert_eq!(protector.key_release_context_hash(), None);
        let HwKeyProtector::V3(protector) = protector else {
            panic!("no context hash must produce v3");
        };
        assert_eq!(protector.unseal_key(&keys).unwrap(), PLAINTEXT);

        let mut legacy = HardwareKeyProtector {
            header: vmgs::HardwareKeyProtectorHeader::new(2, 104, 2, 0),
            iv: protector.iv,
            ciphertext: protector.ciphertext,
            hmac: [0; 32],
        };
        legacy.hmac = crypto::hmac_sha_256::hmac_sha_256(
            &keys.hmac_key,
            &legacy.as_bytes()[..std::mem::offset_of!(HardwareKeyProtector, hmac)],
        )
        .unwrap();
        let legacy = HwKeyProtector::Legacy(legacy);
        assert_eq!(legacy.key_release_context_hash(), None);
        let legacy_policy = legacy.key_derivation_policy().unwrap();
        assert_eq!(
            key_derivation_svn_to_header(legacy_policy.svn),
            key_derivation_svn_to_header(policy.svn)
        );
        assert_eq!(legacy_policy.mix_measurement, policy.mix_measurement);
        assert_eq!(legacy.unseal_key(&keys).unwrap(), PLAINTEXT);
    }

    #[test]
    fn v4_unseal_rejects_changed_vm_policy_settings() {
        let tee = MockTeeCall::new([0x7a; 32]);
        let mut config = create_test_vm_config(HardwareSealingPolicy::Signer);
        config.key_release_context_hash =
            Some(igvm_attest::get::encode_key_release_context_hash(&[0; 32]));
        let keys = HardwareDerivedKeys::derive_key(&tee, &config, test_policy()).unwrap();
        let protector = seal_key(&keys, &PLAINTEXT).unwrap();
        for setting in 0..5 {
            let mut changed = config.clone();
            let mut policy = test_policy();
            match setting {
                0 => changed.secure_boot = true,
                1 => changed.console_enabled = true,
                2 => changed.key_release_context_hash = None,
                3 => changed.hardware_sealing_policy = HardwareSealingPolicy::None,
                _ => {
                    changed.hardware_sealing_policy = HardwareSealingPolicy::Hash;
                    policy.mix_measurement = true;
                }
            }
            let changed_keys = HardwareDerivedKeys::derive_key(&tee, &changed, policy).unwrap();
            assert!(matches!(
                protector.unseal_key(&changed_keys),
                Err(HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed)
            ));
        }
    }

    fn create_test_vm_config(
        hardware_sealing_policy: HardwareSealingPolicy,
    ) -> AttestationVmConfig {
        AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: "".to_string(),
            console_enabled: false,
            interactive_console_enabled: false,
            ipmi_enabled: false,
            secure_boot: false,
            tpm_enabled: false,
            tpm_version: AttestationTpmVersion::V138,
            tpm_persisted: false,
            key_release_context_hash: None,
            hardware_sealing_policy,
            filtered_vpci_devices_allowed: true,
            vm_unique_id: "".to_string(),
            vmgs_provisioner: None,
        }
    }

    #[test]
    fn hardware_derived_keys_hash_policy() {
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Hash);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let hardware_derived_keys = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0x7308000000000003,
                },
                mix_measurement: true,
            },
        )
        .unwrap();

        let output = seal_key(&hardware_derived_keys, &PLAINTEXT).unwrap();
        let hardware_key_protector = HardwareKeyProtectorV3::read_from_prefix(output.as_bytes())
            .unwrap()
            .0;
        let plaintext = hardware_key_protector
            .unseal_key(&hardware_derived_keys)
            .unwrap();
        assert_eq!(plaintext, PLAINTEXT);
    }

    #[test]
    fn hardware_derived_keys_signer_policy() {
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Signer);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let k1 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0x7308000000000003,
                },
                mix_measurement: false,
            },
        )
        .unwrap();
        let output = seal_key(&k1, &PLAINTEXT).unwrap();
        let hardware_key_protector = HardwareKeyProtectorV3::read_from_prefix(output.as_bytes())
            .unwrap()
            .0;

        // Unseal should succeed with different measurements when using signer policy
        let mock_tee_call = Box::new(MockTeeCall::new([0x8bu8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let k2 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0x7308000000000003,
                },
                mix_measurement: false,
            },
        )
        .unwrap();
        let plaintext = hardware_key_protector.unseal_key(&k2).unwrap();
        assert_eq!(plaintext, PLAINTEXT);
    }

    #[test]
    fn hardware_derived_keys_policy_mismatch() {
        {
            let vm_config = create_test_vm_config(HardwareSealingPolicy::Hash);
            let mock_tee_call =
                Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
            let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();

            let result = HardwareDerivedKeys::derive_key(
                mock_get_derived_key_call,
                &vm_config,
                tee_call::KeyDerivationPolicy {
                    svn: tee_call::KeyDerivationSvn::Snp {
                        tcb_version: 0x7308000000000003,
                    },
                    mix_measurement: false,
                },
            );
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(matches!(
                err,
                HardwareDerivedKeysError::KeyDerivationPolicyMismatch
            ));
        }

        {
            let vm_config = create_test_vm_config(HardwareSealingPolicy::Signer);
            let mock_tee_call =
                Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
            let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();

            let result = HardwareDerivedKeys::derive_key(
                mock_get_derived_key_call,
                &vm_config,
                tee_call::KeyDerivationPolicy {
                    svn: tee_call::KeyDerivationSvn::Snp {
                        tcb_version: 0x7308000000000003,
                    },
                    mix_measurement: true,
                },
            );
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(matches!(
                err,
                HardwareDerivedKeysError::KeyDerivationPolicyMismatch
            ));
        }
    }

    #[test]
    fn hardware_key_protector_header_fields_set() {
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Signer);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let policy = tee_call::KeyDerivationPolicy {
            svn: tee_call::KeyDerivationSvn::Snp {
                tcb_version: 0xDEAD_BEEF,
            },
            mix_measurement: false,
        };
        let k =
            HardwareDerivedKeys::derive_key(mock_get_derived_key_call, &vm_config, policy).unwrap();
        let hwkp = seal_key(&k, &PLAINTEXT).unwrap();

        let HwKeyProtector::V3(hwkp) = hwkp else {
            panic!("no context hash must produce v3");
        };
        let (tee_type, svn) = key_derivation_svn_to_header(policy.svn);
        assert_eq!(hwkp.header.tee_type, tee_type);
        assert_eq!(hwkp.header.svn, svn);
        assert_eq!(hwkp.header.mix_measurement, policy.mix_measurement as u8);
        assert_eq!(hwkp.header.length as usize, vmgs::HW_KEY_PROTECTOR_V3_SIZE);
        assert_eq!(hwkp.header.version, vmgs::HW_KEY_PROTECTOR_VERSION_3);
    }

    #[test]
    fn seal_key_fails_when_plaintext_not_block_aligned() {
        // With CBC and no padding enabled, sealing must fail for non-16-aligned sizes.
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Hash);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let k = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp { tcb_version: 2 },
                mix_measurement: true,
            },
        )
        .unwrap();

        let plaintext = [0x7Au8; 20];
        let err = seal_key(&k, &plaintext)
            .expect_err("expected seal to fail for non-block-multiple length");
        assert!(matches!(err, HardwareKeySealingError::EncryptEgressKey(_)));
    }

    #[test]
    fn hardware_key_protector_hmac_mismatch_detected() {
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Hash);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let hardware_derived_keys = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0x7308000000000003,
                },
                mix_measurement: true,
            },
        )
        .unwrap();

        let mut hwkp = seal_key(&hardware_derived_keys, &PLAINTEXT).unwrap();

        // Corrupt the HMAC to force verification failure
        let HwKeyProtector::V3(p) = &mut hwkp else {
            panic!("no context hash must produce v3");
        };
        p.hmac[0] ^= 0xFF;

        let err = hwkp
            .unseal_key(&hardware_derived_keys)
            .expect_err("expected HMAC verification to fail");

        assert!(matches!(
            err,
            HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed
        ));
    }

    #[test]
    fn unseal_fails_with_different_policy_mix_measurement() {
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Hash);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();

        let k1: HardwareDerivedKeys = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp { tcb_version: 0x1 },
                mix_measurement: true,
            },
        )
        .unwrap();
        let hwkp = seal_key(&k1, &PLAINTEXT).unwrap();

        let vm_config = create_test_vm_config(HardwareSealingPolicy::Signer);
        let k2 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp { tcb_version: 0x1 },
                mix_measurement: false,
            },
        )
        .unwrap();

        let err = hwkp
            .unseal_key(&k2)
            .expect_err("mix_measurement policy change should break unseal");
        assert!(matches!(
            err,
            HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed
        ));
    }

    #[test]
    fn unseal_fails_with_different_tcb_version() {
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Hash);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();

        let k1 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0xAAAAAAAAAAAAAAAA,
                },
                mix_measurement: true,
            },
        )
        .unwrap();
        let hwkp = seal_key(&k1, &PLAINTEXT).unwrap();

        let k2 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0xBBBBBBBBBBBBBBBB,
                },
                mix_measurement: true,
            },
        )
        .unwrap();

        let err = hwkp
            .unseal_key(&k2)
            .expect_err("TCB change should break unseal");
        assert!(matches!(
            err,
            HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed
        ));
    }

    #[test]
    fn unseal_fails_with_different_measurements() {
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Hash);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();

        let k1 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0xAAAAAAAAAAAAAAAA,
                },
                mix_measurement: true,
            },
        )
        .unwrap();
        let hwkp = seal_key(&k1, &PLAINTEXT).unwrap();

        let mock_tee_call = Box::new(MockTeeCall::new([0x8bu8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let k2 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                svn: tee_call::KeyDerivationSvn::Snp {
                    tcb_version: 0xAAAAAAAAAAAAAAAA,
                },
                mix_measurement: true,
            },
        )
        .unwrap();

        let err = hwkp
            .unseal_key(&k2)
            .expect_err("measurement change should break unseal");
        assert!(matches!(
            err,
            HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed
        ));
    }
}
