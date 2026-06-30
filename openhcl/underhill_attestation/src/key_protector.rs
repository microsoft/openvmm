// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the key retrieval logic for the [`KeyProtector`].

use crate::Keys;
use crypto::HashAlgorithm;
use crypto::rsa::RsaKeyPair;
use cvm_tracing::CVM_ALLOWED;
use cvm_tracing::CVM_CONFIDENTIAL;
use openhcl_attestation_protocol::vmgs::AES_GCM_KEY_LENGTH;
use openhcl_attestation_protocol::vmgs::DekKp;
use openhcl_attestation_protocol::vmgs::KeyProtector;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum GetKeysFromKeyProtectorError {
    #[error(
        "the DEK format expects to hold an RSA-WRAPPED AES key, but found an AES-WRAPPED AES key"
    )]
    InvalidDekFormat,
    #[error("ingress RSA KEK size {key_size} was larger than expected {expected_size}")]
    InvalidIngressRsaKekSize {
        key_size: usize,
        expected_size: usize,
    },
    #[error(
        "wrapped DiskEncryptionSettings key size {key_size} was smaller than expected {expected_size}"
    )]
    InvalidWrappedDesKeySize {
        key_size: usize,
        expected_size: usize,
    },
    #[error("invalid RSA unwrap output size {output_size}, expected {expected_size}")]
    InvalidRsaUnwrapOutputSize {
        output_size: usize,
        expected_size: usize,
    },
    #[error("invalid AES unwrap output size {output_size}, expected {expected_size}")]
    InvalidAesUnwrapOutputSize {
        output_size: usize,
        expected_size: usize,
    },
    #[error("wrapped egress key too large - {key_size} > {expected_size}")]
    InvalidWrappedEgressKeySize {
        key_size: usize,
        expected_size: usize,
    },
    #[error("failed to unwrap the DiskEncryptionSettings key")]
    DesKeyRsaUnwrap(#[source] crypto::rsa::RsaError),
    #[error("failed to unwrap the ingress DEK entry with RSA-OAEP in KeyProtector")]
    IngressDekRsaUnwrap(#[source] crypto::rsa::RsaError),
    #[error("failed to unwrap the ingress DEK entry with AES-WRAP-WITH-PADDING in KeyProtector")]
    IngressDekAesUnwrap(#[source] crypto::aes_kwp::AesKeyWrapError),
    #[error("failed to unwrap the egress DEK entry with RSA-OAEP in KeyProtector")]
    EgressDekRsaUnwrap(#[source] crypto::rsa::RsaError),
    #[error("failed to unwrap the egress DEK entry with AES-WRAP-WITH-PADDING in KeyProtector")]
    EgressDekAesUnwrap(#[source] crypto::aes_kwp::AesKeyWrapError),
    #[error("failed to wrap the egress key with RSA-OAEP")]
    EgressKeyRsaWrap(#[source] crypto::rsa::RsaError),
    #[error("failed to wrap the egress key with AES-WRAP-WITH-PADDING")]
    EgressKeyAesWrap(#[source] crypto::aes_kwp::AesKeyWrapError),
}

/// AES-Wrapped AES key size (32-byte with 8-byte padding)
pub const AES_WRAPPED_AES_KEY_LENGTH: usize = 40;

/// AES-Wrapped RSA key size (must be at least RSA 2k)
pub const RSA_WRAPPED_AES_KEY_LENGTH: usize = 256;

/// Returns `true` if the DEK buffer contains any non-zero bytes.
pub(crate) fn dek_is_present(dek: &DekKp) -> bool {
    dek.dek_buffer.iter().any(|&x| x != 0)
}

/// Extension trait of [`KeyProtector`].
pub trait KeyProtectorExt {
    /// Unwrap the ingress key for decrypting VMGS (if present) in the Key Protector
    /// and generate a new egress key for (re)encrypting VMGS.
    fn unwrap_and_rotate_keys(
        &mut self,
        ingress_kek: &RsaKeyPair,
        wrapped_des_key: Option<&[u8]>,
        ingress_idx: usize,
        egress_idx: usize,
    ) -> Result<Keys, GetKeysFromKeyProtectorError>;
}

/// RSA-OAEP unwrap the wrapped DiskEncryptionSettings key. The resulting
/// AES key is used for AES-wrapping/unwrapping the DEK entries in the
/// "3-blob" VMGS layout.
fn unwrap_des_key(
    ingress_kek: &RsaKeyPair,
    wrapped_des_key: &[u8],
    modulus_size: usize,
) -> Result<Vec<u8>, GetKeysFromKeyProtectorError> {
    tracing::info!(CVM_ALLOWED, "wrapped key is present");

    if wrapped_des_key.len() < modulus_size {
        return Err(GetKeysFromKeyProtectorError::InvalidWrappedDesKeySize {
            key_size: wrapped_des_key.len(),
            expected_size: modulus_size,
        });
    }

    let key = ingress_kek
        .oaep_decrypt(&wrapped_des_key[..modulus_size], HashAlgorithm::Sha256)
        .map_err(GetKeysFromKeyProtectorError::DesKeyRsaUnwrap)?;

    if key.len() != AES_GCM_KEY_LENGTH {
        return Err(GetKeysFromKeyProtectorError::InvalidRsaUnwrapOutputSize {
            output_size: key.len(),
            expected_size: AES_GCM_KEY_LENGTH,
        });
    }
    Ok(key)
}

/// Unwrap the existing ingress DEK from `kp.dek[ingress_idx]`.
///
/// When `des_key` is `Some(_)` the DEK is expected to be AES-wrapped using
/// that key (3-blob layout); otherwise the DEK is expected to be
/// RSA-wrapped with `ingress_kek` (2-blob layout).
fn unwrap_ingress_dek(
    kp: &KeyProtector,
    ingress_kek: &RsaKeyPair,
    des_key: Option<&[u8]>,
    ingress_idx: usize,
    modulus_size: usize,
) -> Result<[u8; AES_GCM_KEY_LENGTH], GetKeysFromKeyProtectorError> {
    tracing::info!(CVM_CONFIDENTIAL, "found dek, index {}", ingress_idx);

    let dek_buffer = &kp.dek[ingress_idx].dek_buffer;
    let mut ingress_key = [0u8; AES_GCM_KEY_LENGTH];

    if let Some(des_key) = des_key {
        // Validate the DEK format: the bytes following the AES-wrapped key
        // must be zero in the 3-blob layout.
        if dek_buffer[AES_WRAPPED_AES_KEY_LENGTH..]
            .iter()
            .any(|&x| x != 0)
        {
            return Err(GetKeysFromKeyProtectorError::InvalidDekFormat);
        }

        tracing::info!(
            CVM_CONFIDENTIAL,
            "dek[{}] hold an AES-wrapped key",
            ingress_idx
        );

        let aes_unwrapped_key = crypto::aes_kwp::AesKeyWrap::new(des_key)
            .and_then(|kw| {
                kw.unwrapper()?
                    .unwrap(&dek_buffer[..AES_WRAPPED_AES_KEY_LENGTH])
            })
            .map_err(GetKeysFromKeyProtectorError::IngressDekAesUnwrap)?;

        if aes_unwrapped_key.len() != AES_GCM_KEY_LENGTH {
            return Err(GetKeysFromKeyProtectorError::InvalidAesUnwrapOutputSize {
                output_size: aes_unwrapped_key.len(),
                expected_size: AES_GCM_KEY_LENGTH,
            });
        }

        ingress_key[..aes_unwrapped_key.len()].copy_from_slice(&aes_unwrapped_key);
    } else {
        tracing::info!(
            CVM_CONFIDENTIAL,
            "dek[{}] hold an RSA-wrapped key",
            ingress_idx
        );

        let rsa_unwrapped_key = ingress_kek
            .oaep_decrypt(&dek_buffer[..modulus_size], HashAlgorithm::Sha256)
            .map_err(GetKeysFromKeyProtectorError::IngressDekRsaUnwrap)?;

        if rsa_unwrapped_key.len() != AES_GCM_KEY_LENGTH {
            return Err(GetKeysFromKeyProtectorError::InvalidRsaUnwrapOutputSize {
                output_size: rsa_unwrapped_key.len(),
                expected_size: AES_GCM_KEY_LENGTH,
            });
        }

        ingress_key[..rsa_unwrapped_key.len()].copy_from_slice(&rsa_unwrapped_key);
    }

    Ok(ingress_key)
}

/// Unwrap an existing (non-empty) egress DEK left over from a previously
/// incomplete key rotation.
///
/// The returned key MUST NOT be used to re-encrypt the VMGS — the host
/// controls its value. It is only safe for decrypting an existing VMGS.
fn unwrap_existing_egress_dek(
    kp: &KeyProtector,
    ingress_kek: &RsaKeyPair,
    des_key: Option<&[u8]>,
    egress_idx: usize,
    modulus_size: usize,
) -> Result<[u8; AES_GCM_KEY_LENGTH], GetKeysFromKeyProtectorError> {
    tracing::info!(CVM_ALLOWED, "found egress dek");

    let dek_buffer = kp.dek[egress_idx].dek_buffer;
    let old_egress_key = if let Some(unwrapping_key) = des_key {
        // The DEK buffer should contain an AES-wrapped key.
        let aes_unwrapped_key = crypto::aes_kwp::AesKeyWrap::new(unwrapping_key)
            .and_then(|kw| {
                kw.unwrapper()?
                    .unwrap(&dek_buffer[..AES_WRAPPED_AES_KEY_LENGTH])
            })
            .map_err(GetKeysFromKeyProtectorError::EgressDekAesUnwrap)?;

        if aes_unwrapped_key.len() != AES_GCM_KEY_LENGTH {
            return Err(GetKeysFromKeyProtectorError::InvalidAesUnwrapOutputSize {
                output_size: aes_unwrapped_key.len(),
                expected_size: AES_GCM_KEY_LENGTH,
            });
        }

        aes_unwrapped_key
    } else {
        // The DEK buffer should contain an RSA-wrapped key.
        let rsa_unwrapped_key = ingress_kek
            .oaep_decrypt(&dek_buffer[..modulus_size], HashAlgorithm::Sha256)
            .map_err(GetKeysFromKeyProtectorError::EgressDekRsaUnwrap)?;

        if rsa_unwrapped_key.len() != AES_GCM_KEY_LENGTH {
            return Err(GetKeysFromKeyProtectorError::InvalidRsaUnwrapOutputSize {
                output_size: rsa_unwrapped_key.len(),
                expected_size: AES_GCM_KEY_LENGTH,
            });
        }

        rsa_unwrapped_key
    };
    let mut key = [0u8; AES_GCM_KEY_LENGTH];
    key[..old_egress_key.len()].copy_from_slice(&old_egress_key);
    Ok(key)
}

/// Wrap the newly-generated random `encrypt_egress_key` (with AES-wrap if
/// `des_key` is provided, otherwise RSA-OAEP) and store the result in
/// `kp.dek[egress_idx]`.
fn wrap_and_store_new_egress_key(
    kp: &mut KeyProtector,
    ingress_kek: &RsaKeyPair,
    des_key: Option<&[u8]>,
    egress_idx: usize,
    encrypt_egress_key: &[u8; AES_GCM_KEY_LENGTH],
) -> Result<(), GetKeysFromKeyProtectorError> {
    use openhcl_attestation_protocol::vmgs::DEK_BUFFER_SIZE;

    let new_egress_key = if let Some(wrapping_key) = des_key {
        // Create an AES wrapped key
        crypto::aes_kwp::AesKeyWrap::new(wrapping_key)
            .and_then(|kw| kw.wrapper()?.wrap(encrypt_egress_key))
            .map_err(GetKeysFromKeyProtectorError::EgressKeyAesWrap)?
    } else {
        // Create an RSA wrapped key
        ingress_kek
            .oaep_encrypt(encrypt_egress_key, HashAlgorithm::Sha256)
            .map_err(GetKeysFromKeyProtectorError::EgressKeyRsaWrap)?
    };

    if new_egress_key.len() > DEK_BUFFER_SIZE {
        return Err(GetKeysFromKeyProtectorError::InvalidWrappedEgressKeySize {
            key_size: new_egress_key.len(),
            expected_size: DEK_BUFFER_SIZE,
        });
    }

    kp.dek[egress_idx].dek_buffer[..new_egress_key.len()].copy_from_slice(&new_egress_key);

    tracing::info!(
        CVM_CONFIDENTIAL,
        egress_idx = egress_idx,
        egress_key_len = new_egress_key.len(),
        "store new egress key to dek"
    );

    Ok(())
}

impl KeyProtectorExt for KeyProtector {
    fn unwrap_and_rotate_keys(
        &mut self,
        ingress_kek: &RsaKeyPair,
        wrapped_des_key: Option<&[u8]>,
        ingress_idx: usize,
        egress_idx: usize,
    ) -> Result<Keys, GetKeysFromKeyProtectorError> {
        use openhcl_attestation_protocol::vmgs::DEK_BUFFER_SIZE;

        let found_ingress_dek = dek_is_present(&self.dek[ingress_idx]);
        let found_egress_dek = dek_is_present(&self.dek[egress_idx]);
        let modulus_size = ingress_kek.modulus_size();

        // The RSA modulus indexes into a fixed-size DEK buffer on the
        // RSA-unwrap paths; reject keys that wouldn't fit before using
        // `modulus_size` as a slice end.
        let needs_rsa_unwrap = wrapped_des_key.is_some() || found_ingress_dek || found_egress_dek;
        if needs_rsa_unwrap && modulus_size > DEK_BUFFER_SIZE {
            return Err(GetKeysFromKeyProtectorError::InvalidIngressRsaKekSize {
                key_size: modulus_size,
                expected_size: DEK_BUFFER_SIZE,
            });
        }

        // Stage A: optionally unwrap the DES key, then unwrap the ingress DEK.
        let des_key = match wrapped_des_key {
            Some(buf) => Some(unwrap_des_key(ingress_kek, buf, modulus_size)?),
            None => None,
        };
        let ingress_key = if found_ingress_dek {
            unwrap_ingress_dek(
                self,
                ingress_kek,
                des_key.as_deref(),
                ingress_idx,
                modulus_size,
            )?
        } else {
            [0u8; AES_GCM_KEY_LENGTH]
        };

        // Stage B: optionally unwrap any pre-existing egress DEK left
        // behind by a previously-failed key rotation.
        let decrypt_egress_key = if found_egress_dek {
            Some(unwrap_existing_egress_dek(
                self,
                ingress_kek,
                des_key.as_deref(),
                egress_idx,
                modulus_size,
            )?)
        } else {
            tracing::info!(CVM_ALLOWED, "there is no egress dek");
            None
        };

        // Stage C: generate a fresh random egress key, wrap it, and store
        // it in the egress DEK slot. The key is generated by OpenHCL so
        // the host cannot influence its value.
        let mut encrypt_egress_key = [0u8; AES_GCM_KEY_LENGTH];
        getrandom::fill(&mut encrypt_egress_key).expect("rng failure");

        wrap_and_store_new_egress_key(
            self,
            ingress_kek,
            des_key.as_deref(),
            egress_idx,
            &encrypt_egress_key,
        )?;

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
    fn generate_rsa_2k() -> RsaKeyPair {
        RsaKeyPair::generate(2048).unwrap()
    }

    /// Generate an AES-256 key
    fn generate_aes_256() -> [u8; 32] {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect("rng failure");
        buf
    }

    #[test]
    fn key_protector() {
        // Test KEK (RSA-2K)
        let kek = generate_rsa_2k();

        // Test DEK (AES-256)
        let dek = generate_aes_256();

        // Test DEK wrapped by the test RSA KEK
        let result = kek.oaep_encrypt(&dek, HashAlgorithm::Sha256);
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

        let result = kek.oaep_decrypt(
            &key_protector.dek[egress_index].dek_buffer[..kek.modulus_size()],
            HashAlgorithm::Sha256,
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

        let result = kek.oaep_decrypt(
            &key_protector.dek[egress_index].dek_buffer[..kek.modulus_size()],
            HashAlgorithm::Sha256,
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
        let result = crypto::aes_kwp::AesKeyWrap::new(&des).and_then(|kw| kw.wrapper()?.wrap(&dek));
        assert!(result.is_ok());
        let aes_wrapped_dek = result.unwrap();

        // Test DES key wrapped by the test RSA KEK
        let result = kek.oaep_encrypt(&des, HashAlgorithm::Sha256);
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

        let result = kek.oaep_decrypt(&rsa_wrapped_des, HashAlgorithm::Sha256);
        assert!(result.is_ok());
        let des_key = result.unwrap();

        let result = crypto::aes_kwp::AesKeyWrap::new(&des_key).and_then(|kw| {
            kw.unwrapper()?
                .unwrap(&key_protector.dek[egress_index].dek_buffer[..AES_WRAPPED_AES_KEY_LENGTH])
        });
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

        let result = kek.oaep_decrypt(&rsa_wrapped_des, HashAlgorithm::Sha256);
        assert!(result.is_ok());
        let des_key = result.unwrap();

        let result = crypto::aes_kwp::AesKeyWrap::new(&des_key).and_then(|kw| {
            kw.unwrapper()?
                .unwrap(&key_protector.dek[egress_index].dek_buffer[..AES_WRAPPED_AES_KEY_LENGTH])
        });
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
        let result = crypto::aes_kwp::AesKeyWrap::new(&des).and_then(|kw| kw.wrapper()?.wrap(&dek));
        assert!(result.is_ok());
        let mut aes_wrapped_dek = result.unwrap();

        // Test DES key wrapped by the test RSA KEK
        let result = kek.oaep_encrypt(&des, HashAlgorithm::Sha256);
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
        assert!(matches!(
            result,
            Err(GetKeysFromKeyProtectorError::InvalidDekFormat)
        ))
    }
}
