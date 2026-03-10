// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of key derivation using hardware secret and the VMGS data encryption key (DEK)
//! sealing using the derived key. The sealed DEK is written to the [FileId::HW_KEY_PROTECTOR`]
//! entry of the VMGS file, which can be unsealed later.

use crate::crypto;
use cvm_tracing::CVM_ALLOWED;
use openhcl_attestation_protocol::igvm_attest;
use openhcl_attestation_protocol::vmgs;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtector;
use openssl_kdf::kdf::Kbkdf;
use thiserror::Error;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub(crate) enum HardwareDerivedKeysError {
    #[error("key derivation policy does not match VM configuration")]
    KeyDerivationPolicyMismatch,
    #[error("failed to initialize hardware secret")]
    InitializeHardwareSecret(#[source] tee_call::Error),
    #[error("KDF derivation with hardware secret failed")]
    KdfWithHardwareSecret(#[source] openssl_kdf::kdf::KdfError),
}

#[derive(Debug, Error)]
pub(crate) enum HardwareKeySealingError {
    #[error("failed to encrypt the egress key")]
    EncryptEgressKey(#[source] crypto::Aes256CbcError),
    #[error("invalid egress key encryption size {0}, expected {1}")]
    InvalidEgressKeyEncryptionSize(usize, usize),
    #[error("HMAC-SHA-256 after encryption failed")]
    HmacAfterEncrypt(#[source] crypto::HmacSha256Error),
    #[error("HMAC-SHA-256 before ecryption failed")]
    HmacBeforeDecrypt(#[source] crypto::HmacSha256Error),
    #[error("Hardware key protector HMAC verification failed")]
    HardwareKeyProtectorHmacVerificationFailed,
    #[error("failed to decrypt the ingress key")]
    DecryptIngressKey(#[source] crypto::Aes256CbcError),
    #[error("invalid ingress key decryption size {0}, expected {1}")]
    InvalidIngressKeyDecryptionSize(usize, usize),
}

/// Hold the hardware-derived keys.
#[derive(Debug)]
pub struct HardwareDerivedKeys {
    policy: tee_call::KeyDerivationPolicy,
    aes_key: [u8; vmgs::AES_CBC_KEY_LENGTH],
    hmac_key: [u8; vmgs::HMAC_SHA_256_KEY_LENGTH],
}

impl HardwareDerivedKeys {
    /// Derive an AES and HMAC keys based on the hardware secret, VM configuration, and policy for key sealing.
    pub fn derive_key(
        tee_call: &dyn tee_call::TeeCallGetDerivedKey,
        vm_config: &igvm_attest::get::runtime_claims::AttestationVmConfig,
        policy: tee_call::KeyDerivationPolicy,
    ) -> Result<Self, HardwareDerivedKeysError> {
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

        let vm_config_json = serde_json::to_string(vm_config).expect("JSON serialization failed");

        let mut kdf = Kbkdf::new(
            openssl::hash::MessageDigest::sha256(),
            label.to_vec(),
            hardware_secret.to_vec(),
        );
        kdf.set_context(vm_config_json.as_bytes().to_vec());

        let mut output = [0u8; vmgs::AES_CBC_KEY_LENGTH + vmgs::HMAC_SHA_256_KEY_LENGTH];
        openssl_kdf::kdf::derive(kdf, &mut output)
            .map_err(HardwareDerivedKeysError::KdfWithHardwareSecret)?;

        let mut aes_key = [0u8; vmgs::AES_CBC_KEY_LENGTH];
        let mut hmac_key = [0u8; vmgs::HMAC_SHA_256_KEY_LENGTH];

        aes_key.copy_from_slice(&output[..vmgs::AES_CBC_KEY_LENGTH]);
        hmac_key.copy_from_slice(&output[vmgs::AES_CBC_KEY_LENGTH..]);

        Ok(Self {
            policy,
            aes_key,
            hmac_key,
        })
    }
}

/// Extension trait of [`HardwareKeyProtector`].
pub trait HardwareKeyProtectorExt: Sized {
    /// Seal the `egress_key` with encrypt-then-mac.
    fn seal_key(
        hardware_derived_keys: &HardwareDerivedKeys,
        egress_key: &[u8],
    ) -> Result<Self, HardwareKeySealingError>;

    /// Unseal the `inress_key` with verify-mac-then-decrypt.
    fn unseal_key(
        &self,
        hardware_derived_keys: &HardwareDerivedKeys,
    ) -> Result<[u8; vmgs::AES_CBC_KEY_LENGTH], HardwareKeySealingError>;
}

impl HardwareKeyProtectorExt for HardwareKeyProtector {
    fn seal_key(
        hardware_derived_keys: &HardwareDerivedKeys,
        egress_key: &[u8],
    ) -> Result<Self, HardwareKeySealingError> {
        let header = vmgs::HardwareKeyProtectorHeader::new(
            vmgs::HW_KEY_PROTECTOR_CURRENT_VERSION,
            vmgs::HW_KEY_PROTECTOR_SIZE as u32,
            hardware_derived_keys.policy.tcb_version,
            hardware_derived_keys.policy.mix_measurement as u8,
        );

        let mut iv = [0u8; vmgs::AES_CBC_IV_LENGTH];
        getrandom::fill(&mut iv).expect("rng failure");

        let mut encrypted_egress_key = [0u8; vmgs::AES_GCM_KEY_LENGTH];
        let output = crypto::aes_256_cbc_encrypt(&hardware_derived_keys.aes_key, egress_key, &iv)
            .map_err(HardwareKeySealingError::EncryptEgressKey)?;
        if output.len() != vmgs::AES_GCM_KEY_LENGTH {
            Err(HardwareKeySealingError::InvalidEgressKeyEncryptionSize(
                output.len(),
                vmgs::AES_GCM_KEY_LENGTH,
            ))?
        }
        encrypted_egress_key.copy_from_slice(&output[..vmgs::AES_GCM_KEY_LENGTH]);

        let mut hardware_key_protector = Self {
            header,
            iv,
            ciphertext: encrypted_egress_key,
            hmac: [0u8; vmgs::HMAC_SHA_256_KEY_LENGTH],
        };
        let offset = std::mem::offset_of!(Self, hmac);
        hardware_key_protector.hmac = crypto::hmac_sha_256(
            &hardware_derived_keys.hmac_key,
            &hardware_key_protector.as_bytes()[..offset],
        )
        .map_err(HardwareKeySealingError::HmacAfterEncrypt)?;

        tracing::info!(CVM_ALLOWED, "encrypt egress_key using hardware derived key");

        Ok(hardware_key_protector)
    }

    fn unseal_key(
        &self,
        hardware_derived_keys: &HardwareDerivedKeys,
    ) -> Result<[u8; vmgs::AES_CBC_KEY_LENGTH], HardwareKeySealingError> {
        let offset = std::mem::offset_of!(HardwareKeyProtector, hmac);
        let hmac =
            crypto::hmac_sha_256(&hardware_derived_keys.hmac_key, &self.as_bytes()[..offset])
                .map_err(HardwareKeySealingError::HmacBeforeDecrypt)?;

        if hmac != self.hmac {
            Err(HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed)?
        }

        let mut decrypted_ingress_key = [0u8; vmgs::AES_GCM_KEY_LENGTH];
        let output =
            crypto::aes_256_cbc_decrypt(&hardware_derived_keys.aes_key, &self.ciphertext, &self.iv)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockTeeCall;
    use igvm_attest::get::runtime_claims::AttestationVmConfig;
    use igvm_attest::get::runtime_claims::HardwareSealingPolicy;
    use zerocopy::FromBytes;

    const PLAINTEXT: [u8; 32] = [0xAB; 32];

    fn create_test_vm_config(
        hardware_sealing_policy: HardwareSealingPolicy,
    ) -> AttestationVmConfig {
        AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: "".to_string(),
            console_enabled: false,
            interactive_console_enabled: false,
            secure_boot: false,
            tpm_enabled: false,
            tpm_persisted: false,
            hardware_sealing_policy,
            filtered_vpci_devices_allowed: true,
            vm_unique_id: "".to_string(),
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
                tcb_version: 0x7308000000000003,
                mix_measurement: true,
            },
        )
        .unwrap();

        let output = HardwareKeyProtector::seal_key(&hardware_derived_keys, &PLAINTEXT).unwrap();
        let hardware_key_protector = HardwareKeyProtector::read_from_prefix(output.as_bytes())
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
                tcb_version: 0x7308000000000003,
                mix_measurement: false,
            },
        )
        .unwrap();
        let output = HardwareKeyProtector::seal_key(&k1, &PLAINTEXT).unwrap();
        let hardware_key_protector = HardwareKeyProtector::read_from_prefix(output.as_bytes())
            .unwrap()
            .0;

        // Unseal should succeed with different measurements when using signer policy
        let mock_tee_call = Box::new(MockTeeCall::new([0x8bu8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let k2 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                tcb_version: 0x7308000000000003,
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
                    tcb_version: 0x7308000000000003,
                    mix_measurement: false,
                },
            );
            assert!(result.is_err());
            let err = result.unwrap_err();
            matches!(err, HardwareDerivedKeysError::KeyDerivationPolicyMismatch);
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
                    tcb_version: 0x7308000000000003,
                    mix_measurement: true,
                },
            );
            assert!(result.is_err());
            let err = result.unwrap_err();
            matches!(err, HardwareDerivedKeysError::KeyDerivationPolicyMismatch);
        }
    }

    #[test]
    fn hardware_key_protector_header_fields_set() {
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Signer);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let policy = tee_call::KeyDerivationPolicy {
            tcb_version: 0xDEAD_BEEF,
            mix_measurement: false,
        };
        let k =
            HardwareDerivedKeys::derive_key(mock_get_derived_key_call, &vm_config, policy).unwrap();
        let hwkp = HardwareKeyProtector::seal_key(&k, &PLAINTEXT).unwrap();

        assert_eq!(hwkp.header.tcb_version, policy.tcb_version);
        assert_eq!(hwkp.header.mix_measurement, policy.mix_measurement as u8);
        assert_eq!(hwkp.header.length as usize, vmgs::HW_KEY_PROTECTOR_SIZE);
        assert_eq!(hwkp.header.version, vmgs::HW_KEY_PROTECTOR_CURRENT_VERSION);
    }

    #[test]
    fn unseal_key_fails_when_original_plaintext_not_32() {
        // With CBC and no padding enabled, sealing must fail for non-16-aligned sizes.
        let vm_config = create_test_vm_config(HardwareSealingPolicy::Hash);
        let mock_tee_call = Box::new(MockTeeCall::new([0x7au8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let k = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                tcb_version: 2,
                mix_measurement: true,
            },
        )
        .unwrap();

        let plaintext = [0x7Au8; 20];
        let err = HardwareKeyProtector::seal_key(&k, &plaintext)
            .expect_err("expected seal to fail for non-block-multiple length");
        matches!(err, HardwareKeySealingError::EncryptEgressKey(_));
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
                tcb_version: 0x7308000000000003,
                mix_measurement: true,
            },
        )
        .unwrap();

        let mut hwkp = HardwareKeyProtector::seal_key(&hardware_derived_keys, &PLAINTEXT).unwrap();

        // Corrupt the HMAC to force verification failure
        hwkp.hmac[0] ^= 0xFF;

        let err = hwkp
            .unseal_key(&hardware_derived_keys)
            .expect_err("expected HMAC verification to fail");

        matches!(
            err,
            HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed
        );
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
                tcb_version: 0x1,
                mix_measurement: true,
            },
        )
        .unwrap();
        let hwkp = HardwareKeyProtector::seal_key(&k1, &PLAINTEXT).unwrap();

        let vm_config = create_test_vm_config(HardwareSealingPolicy::Signer);
        let k2 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                tcb_version: 0x1,
                mix_measurement: false,
            },
        )
        .unwrap();

        let err = hwkp
            .unseal_key(&k2)
            .expect_err("mix_measurement policy change should break unseal");
        matches!(
            err,
            HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed
        );
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
                tcb_version: 0xAAAAAAAAAAAAAAAA,
                mix_measurement: true,
            },
        )
        .unwrap();
        let hwkp = HardwareKeyProtector::seal_key(&k1, &PLAINTEXT).unwrap();

        let k2 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                tcb_version: 0xBBBBBBBBBBBBBBBB,
                mix_measurement: true,
            },
        )
        .unwrap();

        let err = hwkp
            .unseal_key(&k2)
            .expect_err("TCB change should break unseal");
        matches!(
            err,
            HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed
        );
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
                tcb_version: 0xAAAAAAAAAAAAAAAA,
                mix_measurement: true,
            },
        )
        .unwrap();
        let hwkp = HardwareKeyProtector::seal_key(&k1, &PLAINTEXT).unwrap();

        let mock_tee_call = Box::new(MockTeeCall::new([0x8bu8; 32])) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_tee_call.supports_get_derived_key().unwrap();
        let k2 = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            tee_call::KeyDerivationPolicy {
                tcb_version: 0xAAAAAAAAAAAAAAAA,
                mix_measurement: true,
            },
        )
        .unwrap();

        let err = hwkp
            .unseal_key(&k2)
            .expect_err("measurement change should break unseal");
        matches!(
            err,
            HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed
        );
    }
}
