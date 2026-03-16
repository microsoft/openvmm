// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub fn encrypt(
    key: &[u8],
    iv: &[u8],
    data: &[u8],
    tag: &mut [u8],
) -> Result<Vec<u8>, Aes256GcmError> {
    openssl::symm::encrypt_aead(
        openssl::symm::Cipher::aes_256_gcm(),
        key,
        Some(iv),
        &[],
        data,
        tag,
    )
    .map_err(|e| Aes256GcmError(crate::BackendError(e, "encryption")))
}

pub fn decrypt(key: &[u8], iv: &[u8], data: &[u8], tag: &[u8]) -> Result<Vec<u8>, Aes256GcmError> {
    openssl::symm::decrypt_aead(
        openssl::symm::Cipher::aes_256_gcm(),
        key,
        Some(iv),
        &[],
        data,
        tag,
    )
    .map_err(|e| Aes256GcmError(crate::BackendError(e, "decryption")))
}
