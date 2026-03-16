// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::error::Error;

pub fn vmgs_encrypt(key: &[u8], iv: &[u8], data: &[u8], tag: &mut [u8]) -> Result<Vec<u8>, Error> {
    crypto::aes_256_gcm::encrypt(key, iv, data, tag)
        .map_err(|e| Error::Crypto(e, "writing encrypted data"))
}

pub fn vmgs_decrypt(key: &[u8], iv: &[u8], data: &[u8], tag: &[u8]) -> Result<Vec<u8>, Error> {
    crypto::aes_256_gcm::decrypt(key, iv, data, tag)
        .map_err(|e| Error::Crypto(e, "reading decrypted data"))
}
