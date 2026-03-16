// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub fn encrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    let cipher = openssl::cipher::Cipher::aes_256_cbc();
    let mut output = vec![0u8; data.len() + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new()
        .map_err(|e| Aes256CbcError(crate::BackendError(e, "creating cipher context")))?;

    ctx.encrypt_init(Some(cipher), Some(key), Some(iv))
        .map_err(|e| Aes256CbcError(crate::BackendError(e, "encrypt init")))?;
    ctx.set_padding(false);

    let count = ctx
        .cipher_update(data, Some(&mut output))
        .map_err(|e| Aes256CbcError(crate::BackendError(e, "encryption")))?;
    let rest = ctx
        .cipher_final(&mut output[count..])
        .map_err(|e| Aes256CbcError(crate::BackendError(e, "encryption")))?;
    output.truncate(count + rest);

    Ok(output)
}

pub fn decrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    let cipher = openssl::cipher::Cipher::aes_256_cbc();
    let mut output = vec![0u8; data.len() + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new()
        .map_err(|e| Aes256CbcError(crate::BackendError(e, "creating cipher context")))?;

    ctx.decrypt_init(Some(cipher), Some(key), Some(iv))
        .map_err(|e| Aes256CbcError(crate::BackendError(e, "decrypt init")))?;
    ctx.set_padding(false);

    let count = ctx
        .cipher_update(data, Some(&mut output))
        .map_err(|e| Aes256CbcError(crate::BackendError(e, "decryption")))?;
    let rest = ctx
        .cipher_final(&mut output[count..])
        .map_err(|e| Aes256CbcError(crate::BackendError(e, "decryption")))?;
    output.truncate(count + rest);

    Ok(output)
}
