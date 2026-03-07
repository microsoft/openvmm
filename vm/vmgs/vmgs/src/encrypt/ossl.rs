// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::error::Error;
use openssl::symm::Cipher;
use openssl::symm::decrypt_aead;
use openssl::symm::encrypt_aead;

pub fn vmgs_encrypt(key: &[u8], iv: &[u8], data: &[u8], tag: &mut [u8]) -> Result<Vec<u8>, Error> {
    encrypt_aead(Cipher::aes_256_gcm(), key, Some(iv), &[], data, tag)
        .map_err(|e| Error::OpenSSL(e, "writing encrypted data"))
}

pub fn vmgs_decrypt(key: &[u8], iv: &[u8], data: &[u8], tag: &[u8]) -> Result<Vec<u8>, Error> {
    decrypt_aead(Cipher::aes_256_gcm(), key, Some(iv), &[], data, tag)
        .map_err(|e| Error::OpenSSL(e, "reading decrypted data"))
}
