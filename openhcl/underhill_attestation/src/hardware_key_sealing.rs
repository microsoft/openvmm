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
pub struct HardwareDerivedKeys {
    tcb_version: u64,
    aes_key: [u8; vmgs::AES_CBC_KEY_LENGTH],
    hmac_key: [u8; vmgs::HMAC_SHA_256_KEY_LENGTH],
}

impl HardwareDerivedKeys {
    /// Derive an AES and HMAC keys based on the hardware secret for key sealing.
    pub fn derive_key(
        tee_call: &dyn tee_call::TeeCallGetDerivedKey,
        vm_config: &igvm_attest::get::runtime_claims::AttestationVmConfig,
        tcb_version: u64,
    ) -> Result<Self, HardwareDerivedKeysError> {
        let hardware_secret = tee_call
            .get_derived_key(tcb_version)
            .map_err(HardwareDerivedKeysError::InitializeHardwareSecret)?;
        let label = b"ISOHWKEY";

        let vm_config = serde_json::to_string(vm_config).expect("JSON serialization failed");

        let mut kdf = Kbkdf::new(
            openssl::hash::MessageDigest::sha256(),
            label.to_vec(),
            hardware_secret.to_vec(),
        );
        kdf.set_context(vm_config.as_bytes().to_vec());

        let mut output = [0u8; vmgs::AES_CBC_KEY_LENGTH + vmgs::HMAC_SHA_256_KEY_LENGTH];
        openssl_kdf::kdf::derive(kdf, &mut output)
            .map_err(HardwareDerivedKeysError::KdfWithHardwareSecret)?;

        let mut aes_key = [0u8; vmgs::AES_CBC_KEY_LENGTH];
        let mut hmac_key = [0u8; vmgs::HMAC_SHA_256_KEY_LENGTH];

        aes_key.copy_from_slice(&output[..vmgs::AES_CBC_KEY_LENGTH]);
        hmac_key.copy_from_slice(&output[vmgs::AES_CBC_KEY_LENGTH..]);

        Ok(Self {
            tcb_version,
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
            vmgs::HW_KEY_VERSION,
            vmgs::HW_KEY_PROTECTOR_SIZE as u32,
            hardware_derived_keys.tcb_version,
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
    use zerocopy::FromBytes;

    #[test]
    fn hardware_derived_keys() {
        const PLAINTEXT: [u8; 32] = [
            0x5e, 0xd7, 0xf3, 0xd4, 0x9e, 0xcf, 0xb5, 0x6c, 0x05, 0x54, 0x7c, 0x87, 0xe7, 0x30,
            0x59, 0xb1, 0x91, 0xcb, 0xa6, 0xc4, 0x0e, 0x4e, 0x30, 0x77, 0x65, 0x19, 0x71, 0xf5,
            0x20, 0x83, 0x2a, 0xc0,
        ];

        let vm_config = igvm_attest::get::runtime_claims::AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: "".to_string(),
            console_enabled: false,
            secure_boot: false,
            tpm_enabled: false,
            tpm_persisted: false,
            filtered_vpci_devices_allowed: true,
            vm_unique_id: "".to_string(),
        };
        let mock_call = Box::new(MockTeeCall::new(0x1234)) as Box<dyn tee_call::TeeCall>;
        let mock_get_derived_key_call = mock_call.supports_get_derived_key().unwrap();
        let result = HardwareDerivedKeys::derive_key(
            mock_get_derived_key_call,
            &vm_config,
            0x7308000000000003,
        );
        assert!(result.is_ok());
        let hardware_derived_keys = result.unwrap();

        let result = HardwareKeyProtector::seal_key(&hardware_derived_keys, &PLAINTEXT);
        assert!(result.is_ok());
        let output = result.unwrap();

        let result = HardwareKeyProtector::read_from_prefix(output.as_bytes());
        assert!(result.is_ok());
        let hardware_key_protector = result.unwrap().0;

        let result = hardware_key_protector.unseal_key(&hardware_derived_keys);
        assert!(result.is_ok());
        let plaintext = result.unwrap();
        assert_eq!(plaintext, PLAINTEXT);
    }
}
