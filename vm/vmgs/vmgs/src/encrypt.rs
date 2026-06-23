// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::error::Error;
use vmgs_format::VMGS_ENCRYPTION_KEY_SIZE;
use vmgs_format::VMGS_NONCE_SIZE;

// TODO: Consider caching the `Aes256Gcm` context for each key.

pub fn vmgs_encrypt(
    key: &[u8; VMGS_ENCRYPTION_KEY_SIZE],
    iv: &[u8; VMGS_NONCE_SIZE],
    data: &[u8],
    tag: &mut [u8],
) -> Result<Vec<u8>, Error> {
    crypto::aes_256_gcm::Aes256Gcm::new(key)
        .map_err(Error::Crypto)?
        .encrypt()
        .map_err(Error::Crypto)?
        .cipher(iv, data, tag)
        .map_err(Error::Crypto)
}

pub fn vmgs_decrypt(
    key: &[u8; VMGS_ENCRYPTION_KEY_SIZE],
    iv: &[u8; VMGS_NONCE_SIZE],
    data: &[u8],
    tag: &[u8],
) -> Result<Vec<u8>, Error> {
    crypto::aes_256_gcm::Aes256Gcm::new(key)
        .map_err(Error::Crypto)?
        .decrypt()
        .map_err(Error::Crypto)?
        .cipher(iv, data, tag)
        .map_err(Error::Crypto)
}
