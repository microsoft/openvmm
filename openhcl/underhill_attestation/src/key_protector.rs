// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the key retrieval logic for the [`KeyProtector`].

use crate::Keys;
use crate::crypto;
use cvm_tracing::CVM_ALLOWED;
use cvm_tracing::CVM_CONFIDENTIAL;
use openhcl_attestation_protocol::vmgs::AES_GCM_KEY_LENGTH;
use openhcl_attestation_protocol::vmgs::KeyProtector;
use openssl::pkey::Private;
use openssl::rsa::Rsa;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum GetKeysFromKeyProtectorError {
    #[error(
        "The DEK format expects to hold an RSA-WRAPPED AES key, but found an AES-WRAPPED AES key"
    )]
    InvalidDekFormat,
    #[error("Ingress RSA KEK size {key_size} was larger than expected {expected_size}")]
    InvalidIngressRsaKekSize {
        key_size: usize,
        expected_size: usize,
    },
    #[error(
        "Wrapped DiskEncryptionSettings key size {key_size} was smaller than expected {expected_size}"
    )]
    InvalidWrappedDesKeySize {
        key_size: usize,
        expected_size: usize,
    },
    #[error("Invalid RSA unwrap output size {output_size}, expected {expected_size}")]
    InvalidRsaUnwrapOutputSize {
        output_size: usize,
        expected_size: usize,
    },
    #[error("Invalid AES unwrap output size {output_size}, expected {expected_size}")]
    InvalidAesUnwrapOutputSize {
        output_size: usize,
        expected_size: usize,
    },
    #[error("Wrapped egress key too large - {key_size} > {expected_size}")]
    InvalidWrappedEgressKeySize {
        key_size: usize,
        expected_size: usize,
    },
    #[error("failed to unwrap the DiskEncryptionSettings key")]
    DesKeyRsaUnwrap(#[source] crypto::RsaOaepError),
    #[error("failed to unwrap the ingress DEK entry with RSA-OAEP in KeyProtector")]
    IngressDekRsaUnwrap(#[source] crypto::RsaOaepError),
    #[error("failed to unwrap the ingress DEK entry with AES-WRAP-WITH-PADDING in KeyProtector")]
    IngressDekAesUnwrap(#[source] crypto::AesKeyWrapWithPaddingError),
    #[error("failed to unwrap the egress DEK entry with RSA-OAEP in KeyProtector")]
    EgressDekRsaUnwrap(#[source] crypto::RsaOaepError),
    #[error("failed to unwrap the egress DEK entry with AES-WRAP-WITH-PADDING in KeyProtector")]
    EgressDekAesUnwrap(#[source] crypto::AesKeyWrapWithPaddingError),
    #[error("failed to wrap the egress key with RSA-OAEP")]
    EgressKeyRsaWrap(#[source] crypto::RsaOaepError),
    #[error("failed to wrap the egress key with AES-WRAP-WITH-PADDING")]
    EgressKeyAesWrap(#[source] crypto::AesKeyWrapWithPaddingError),
}

/// AES-Wrapped AES key size (32-byte with 8-byte padding)
pub const AES_WRAPPED_AES_KEY_LENGTH: usize = 40;

/// AES-Wrapped RSA key size (must be at least RSA 2k)
pub const RSA_WRAPPED_AES_KEY_LENGTH: usize = 256;

/// Extension trait of [`KeyProtector`].
pub trait KeyProtectorExt {
    /// Unwrap the ingress key for decrypting VMGS (if present) in the Key Protector
    /// and generate a new egress key for (re)encrypting VMGS.
    fn unwrap_and_rotate_keys(
        &mut self,
        ingress_kek: &Rsa<Private>,
        wrapped_des_key: Option<&[u8]>,
        ingress_idx: usize,
        egress_idx: usize,
    ) -> Result<Keys, GetKeysFromKeyProtectorError>;
}

impl KeyProtectorExt for KeyProtector {
    fn unwrap_and_rotate_keys(
        &mut self,
        ingress_kek: &Rsa<Private>,
        wrapped_des_key: Option<&[u8]>,
        ingress_idx: usize,
        egress_idx: usize,
    ) -> Result<Keys, GetKeysFromKeyProtectorError> {
        use openhcl_attestation_protocol::vmgs::DEK_BUFFER_SIZE;

        let found_ingress_dek = !self.dek[ingress_idx].dek_buffer.iter().all(|&x| x == 0);
        let found_egress_dek = !self.dek[egress_idx].dek_buffer.iter().all(|&x| x == 0);
        let mut ingress_key = [0u8; AES_GCM_KEY_LENGTH];
        let mut encrypt_egress_key = [0u8; AES_GCM_KEY_LENGTH];
        let use_des_key = wrapped_des_key.is_some(); // whether the wrapped key from DiskEncryptionSettings payload is used
        let modulus_size = ingress_kek.size() as usize;

        // If the `dek` entry is not empty or `wrapped_des_key` (RSA-wrapped) is present, decrypt the ingress key.
        // The use of `wrapped_des_key` from DiskEncryptionSettings implies that VMGS structure is new (3-blob) where
        // the `dek` entry contains an AES-wrapped key. The AES-wrapped key can be unwrapped by the
        // decrypted `wrapped_key` (using `ingress_kek`). Otherwise, VMGS structure should be old (2-blob)
        // where the `dek` is an RSA-wrapped key. The RSA-wrapped key can be unwrapped by the `ingress_kek`.
        let des_key = if found_ingress_dek || use_des_key {
            if found_ingress_dek && use_des_key {
                // Validate the DEK format, which is expected to hold an AES-wrapped key
                // when `wrapped_des_key` is `Some`.
                if !self.dek[ingress_idx].dek_buffer[AES_WRAPPED_AES_KEY_LENGTH..]
                    .iter()
                    .all(|&x| x == 0)
                {
                    Err(GetKeysFromKeyProtectorError::InvalidDekFormat)?
                }
            }

            if modulus_size > DEK_BUFFER_SIZE {
                Err(GetKeysFromKeyProtectorError::InvalidIngressRsaKekSize {
                    key_size: modulus_size,
                    expected_size: DEK_BUFFER_SIZE,
                })?
            }

            let rsa_unwrapped_key = if let Some(wrapped_des_key) = wrapped_des_key {
                tracing::info!(CVM_ALLOWED, "wrapped key is present");

                if wrapped_des_key.len() < modulus_size {
                    Err(GetKeysFromKeyProtectorError::InvalidWrappedDesKeySize {
                        key_size: wrapped_des_key.len(),
                        expected_size: modulus_size,
                    })?
                }

                crypto::rsa_oaep_decrypt(
                    ingress_kek,
                    &wrapped_des_key[..modulus_size],
                    crypto::RsaOaepHashAlgorithm::Sha256,
                )
                .map_err(GetKeysFromKeyProtectorError::DesKeyRsaUnwrap)?
            } else {
                // The DEK buffer should contain an RSA-wrapped key.
                tracing::info!(CVM_CONFIDENTIAL, "found dek, index {}", ingress_idx);

                crypto::rsa_oaep_decrypt(
                    ingress_kek,
                    &self.dek[ingress_idx].dek_buffer[..modulus_size],
                    crypto::RsaOaepHashAlgorithm::Sha256,
                )
                .map_err(GetKeysFromKeyProtectorError::IngressDekRsaUnwrap)?
            };

            if rsa_unwrapped_key.len() != AES_GCM_KEY_LENGTH {
                Err(GetKeysFromKeyProtectorError::InvalidRsaUnwrapOutputSize {
                    output_size: rsa_unwrapped_key.len(),
                    expected_size: AES_GCM_KEY_LENGTH,
                })?
            }

            if found_ingress_dek {
                if use_des_key {
                    tracing::info!(
                        CVM_CONFIDENTIAL,
                        "dek[{}] hold an AES-wrapped key",
                        ingress_idx
                    );

                    // The DEK buffer should contain an AES-wrapped key.
                    let dek_buffer = &self.dek[ingress_idx].dek_buffer;
                    let aes_unwrapped_key = crypto::aes_key_unwrap_with_padding(
                        &rsa_unwrapped_key,
                        &dek_buffer[..AES_WRAPPED_AES_KEY_LENGTH],
                    )
                    .map_err(GetKeysFromKeyProtectorError::IngressDekAesUnwrap)?;

                    if aes_unwrapped_key.len() != AES_GCM_KEY_LENGTH {
                        Err(GetKeysFromKeyProtectorError::InvalidAesUnwrapOutputSize {
                            output_size: aes_unwrapped_key.len(),
                            expected_size: AES_GCM_KEY_LENGTH,
                        })?
                    }

                    ingress_key[..aes_unwrapped_key.len()].copy_from_slice(&aes_unwrapped_key);
                } else {
                    tracing::info!(
                        CVM_CONFIDENTIAL,
                        "dek[{}] hold an RSA-wrapped key",
                        ingress_idx
                    );

                    ingress_key[..rsa_unwrapped_key.len()].copy_from_slice(&rsa_unwrapped_key);
                }
            }

            if use_des_key {
                Some(rsa_unwrapped_key)
            } else {
                None
            }
        } else {
            None
        };

        let decrypt_egress_key = if found_egress_dek {
            tracing::info!(CVM_ALLOWED, "found egress dek");

            // Key rolling did not complete successfully last time (normally egress should be empty).
            // Any existing egress key can be used to decrypt the VMGS but must not be used to
            // re-encrypt the VMGS, as its value can be controlled by the host.
            let dek_buffer = self.dek[egress_idx].dek_buffer;
            let old_egress_key = if let Some(unwrapping_key) = &des_key {
                // The DEK buffer should contain an AES-wrapped key.
                crypto::aes_key_unwrap_with_padding(
                    unwrapping_key,
                    &dek_buffer[..AES_WRAPPED_AES_KEY_LENGTH],
                )
                .map_err(GetKeysFromKeyProtectorError::EgressDekAesUnwrap)?
            } else {
                // The DEK buffer should contain an RSA-wrapped key.
                crypto::rsa_oaep_decrypt(
                    ingress_kek,
                    &dek_buffer[..modulus_size],
                    crypto::RsaOaepHashAlgorithm::Sha256,
                )
                .map_err(GetKeysFromKeyProtectorError::EgressDekRsaUnwrap)?
            };
            let mut key = [0u8; AES_GCM_KEY_LENGTH];
            key[..old_egress_key.len()].copy_from_slice(&old_egress_key);
            Some(key)
        } else {
            tracing::info!(CVM_ALLOWED, "there is no egress dek");
            None
        };

        // Always generate a new "encrypt egress key". This is generated randomly by
        // OpenHCL, and so cannot be controlled by the host, and is safe to use to
        // encrypt the VMGS.
        getrandom::fill(&mut encrypt_egress_key).expect("rng failure");

        let new_egress_key = if let Some(wrapping_key) = des_key {
            // Create an AES wrapped key
            crypto::aes_key_wrap_with_padding(&wrapping_key, &encrypt_egress_key)
                .map_err(GetKeysFromKeyProtectorError::EgressKeyAesWrap)?
        } else {
            // Create an RSA wrapped key
            crypto::rsa_oaep_encrypt(
                ingress_kek,
                &encrypt_egress_key,
                crypto::RsaOaepHashAlgorithm::Sha256,
            )
            .map_err(GetKeysFromKeyProtectorError::EgressKeyRsaWrap)?
        };

        if new_egress_key.len() > DEK_BUFFER_SIZE {
            Err(GetKeysFromKeyProtectorError::InvalidWrappedEgressKeySize {
                key_size: new_egress_key.len(),
                expected_size: DEK_BUFFER_SIZE,
            })?
        }

        self.dek[egress_idx].dek_buffer[..new_egress_key.len()].copy_from_slice(&new_egress_key);

        tracing::info!(
            CVM_CONFIDENTIAL,
            egress_idx = egress_idx,
            egress_key_len = new_egress_key.len(),
            "store new egress key to dek"
        );

        Ok(Keys {
            ingress: ingress_key,
            decrypt_egress: decrypt_egress_key,
            encrypt_egress: encrypt_egress_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::FromBytes;

    /// Generate an RSA-2k key
    fn generate_rsa_2k() -> Rsa<Private> {
        let result = Rsa::generate(2048);
        assert!(result.is_ok());

        result.unwrap()
    }

    /// Generate an AES-256 key
    fn generate_aes_256() -> [u8; 32] {
        let mut buf = [0u8; 32];
        let result = getrandom::fill(&mut buf[..]);
        assert!(result.is_ok());

        buf
    }

    #[test]
    fn key_protector() {
        // Test KEK (RSA-2K)
        let kek = generate_rsa_2k();

        // Test DEK (AES-256)
        let dek = generate_aes_256();

        // Test DEK wrapped by the test RSA KEK
        let result = crypto::rsa_oaep_encrypt(&kek, &dek, crypto::RsaOaepHashAlgorithm::Sha256);
        assert!(result.is_ok());
        let rsa_wrapped_dek = result.unwrap();

        // Test key rotation for first boot

        let ingress_index = 0;
        let egress_index = 1;

        let mut data = [0u8; openhcl_attestation_protocol::vmgs::KEY_PROTECTOR_SIZE];
        data[..rsa_wrapped_dek.len()].copy_from_slice(&rsa_wrapped_dek);

        let result = KeyProtector::read_from_prefix(&data);
        assert!(result.is_ok());
        let mut key_protector = result.unwrap().0;
        assert_eq!(
            key_protector.dek[ingress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );
        assert_eq!(
            key_protector.dek[egress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            true
        );

        let result = key_protector.unwrap_and_rotate_keys(&kek, None, ingress_index, egress_index);
        assert!(result.is_ok());
        let keys = result.unwrap();
        assert_eq!(keys.ingress, dek);
        assert_eq!(
            key_protector.dek[ingress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );
        assert_eq!(
            key_protector.dek[egress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );

        let result = crypto::rsa_oaep_decrypt(
            &kek,
            &key_protector.dek[egress_index].dek_buffer[..kek.size() as usize],
            crypto::RsaOaepHashAlgorithm::Sha256,
        );
        assert!(result.is_ok());
        let plaintext = result.unwrap();
        assert_eq!(plaintext, keys.encrypt_egress);
        let key_egress_first_boot = keys.encrypt_egress;

        // Test key rotation for reboot

        let ingress_index = 1;
        let egress_index = 0;

        let result = key_protector.unwrap_and_rotate_keys(&kek, None, ingress_index, egress_index);
        assert!(result.is_ok());
        let keys = result.unwrap();
        assert_eq!(keys.ingress, key_egress_first_boot);
        assert_eq!(
            key_protector.dek[ingress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );
        assert_eq!(
            key_protector.dek[egress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );

        let result = crypto::rsa_oaep_decrypt(
            &kek,
            &key_protector.dek[egress_index].dek_buffer[..kek.size() as usize],
            crypto::RsaOaepHashAlgorithm::Sha256,
        );
        assert!(result.is_ok());
        let plaintext = result.unwrap();
        assert_eq!(plaintext, keys.encrypt_egress);
    }

    #[test]
    fn key_protector_with_wrapped_key() {
        // Test KEK (RSA-2K)
        let kek = generate_rsa_2k();

        // Test DEK (AES-256)
        let dek = generate_aes_256();

        // Test DEK wrapped by the test DES key (AES-256)
        let des = generate_aes_256();
        let result = crypto::aes_key_wrap_with_padding(&des, &dek);
        assert!(result.is_ok());
        let aes_wrapped_dek = result.unwrap();

        // Test DES key wrapped by the test RSA KEK
        let result = crypto::rsa_oaep_encrypt(&kek, &des, crypto::RsaOaepHashAlgorithm::Sha256);
        assert!(result.is_ok());
        let rsa_wrapped_des = result.unwrap();

        // Test key rotation for first boot

        let ingress_index = 0;
        let egress_index = 1;

        let mut data = [0u8; openhcl_attestation_protocol::vmgs::KEY_PROTECTOR_SIZE];

        data[..aes_wrapped_dek.len()].copy_from_slice(&aes_wrapped_dek);

        let result = KeyProtector::read_from_prefix(&data);
        assert!(result.is_ok());
        let mut key_protector = result.unwrap().0;
        assert_eq!(
            key_protector.dek[ingress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );
        assert_eq!(
            key_protector.dek[egress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            true
        );

        let result = key_protector.unwrap_and_rotate_keys(
            &kek,
            Some(rsa_wrapped_des.as_ref()),
            ingress_index,
            egress_index,
        );
        assert!(result.is_ok());
        let keys = result.unwrap();
        assert_eq!(keys.ingress, dek);
        assert_eq!(
            key_protector.dek[ingress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );
        assert_eq!(
            key_protector.dek[egress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );

        let result =
            crypto::rsa_oaep_decrypt(&kek, &rsa_wrapped_des, crypto::RsaOaepHashAlgorithm::Sha256);
        assert!(result.is_ok());
        let des_key = result.unwrap();

        let result = crypto::aes_key_unwrap_with_padding(
            &des_key,
            &key_protector.dek[egress_index].dek_buffer[..AES_WRAPPED_AES_KEY_LENGTH],
        );
        assert!(result.is_ok());
        let unwrapped_key = result.unwrap();
        assert_eq!(unwrapped_key, keys.encrypt_egress);
        let key_egress_first_boot = keys.encrypt_egress;

        // Test key rotation for reboot

        let ingress_index = 1;
        let egress_index = 0;

        let result = key_protector.unwrap_and_rotate_keys(
            &kek,
            Some(rsa_wrapped_des.as_ref()),
            ingress_index,
            egress_index,
        );
        assert!(result.is_ok());
        let keys = result.unwrap();
        assert_eq!(keys.ingress, key_egress_first_boot);
        assert_eq!(
            key_protector.dek[ingress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );
        assert_eq!(
            key_protector.dek[egress_index]
                .dek_buffer
                .iter()
                .all(|&x| x == 0),
            false
        );

        let result =
            crypto::rsa_oaep_decrypt(&kek, &rsa_wrapped_des, crypto::RsaOaepHashAlgorithm::Sha256);
        assert!(result.is_ok());
        let des_key = result.unwrap();

        let result = crypto::aes_key_unwrap_with_padding(
            &des_key,
            &key_protector.dek[egress_index].dek_buffer[..AES_WRAPPED_AES_KEY_LENGTH],
        );
        assert!(result.is_ok());
        let unwrapped_key = result.unwrap();
        assert_eq!(unwrapped_key, keys.encrypt_egress);
    }

    #[test]
    fn key_protector_with_wrapped_key_invalid_format() {
        // Test KEK (RSA-2K)
        let kek = generate_rsa_2k();

        // Test DEK (AES-256)
        let dek = generate_aes_256();

        // Test DEK wrapped by the test DES key (AES-256)
        let des = generate_aes_256();
        let result = crypto::aes_key_wrap_with_padding(&des, &dek);
        assert!(result.is_ok());
        let mut aes_wrapped_dek = result.unwrap();

        // Test DES key wrapped by the test RSA KEK
        let result = crypto::rsa_oaep_encrypt(&kek, &des, crypto::RsaOaepHashAlgorithm::Sha256);
        assert!(result.is_ok());
        let rsa_wrapped_des = result.unwrap();

        let mut data = [0u8; openhcl_attestation_protocol::vmgs::KEY_PROTECTOR_SIZE];

        // Test the invalid DEK format whose size is larger than AES-wrapped key size.
        aes_wrapped_dek.resize(AES_WRAPPED_AES_KEY_LENGTH + 1, 1);

        data[..aes_wrapped_dek.len()].copy_from_slice(&aes_wrapped_dek);

        let result = KeyProtector::read_from_prefix(&data);
        assert!(result.is_ok());
        let mut key_protector = result.unwrap().0;

        let result =
            key_protector.unwrap_and_rotate_keys(&kek, Some(rsa_wrapped_des.as_ref()), 0, 1);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "The DEK format expects to hold an RSA-WRAPPED AES key, but found an AES-WRAPPED AES key".to_string())
    }
}
