// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

fn select_cipher(key_len: usize) -> Result<&'static openssl::cipher::CipherRef, AesKeyWrapError> {
    match key_len {
        16 => Ok(openssl::cipher::Cipher::aes_128_wrap_pad()),
        24 => Ok(openssl::cipher::Cipher::aes_192_wrap_pad()),
        32 => Ok(openssl::cipher::Cipher::aes_256_wrap_pad()),
        _ => Err(AesKeyWrapError(crate::BackendError(
            openssl::error::ErrorStack::get(),
            "invalid wrapping key size",
        ))),
    }
}

pub fn wrap(wrapping_key: &[u8], payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
    let cipher = select_cipher(wrapping_key.len())?;
    let padding = 8 - payload.len() % 8;
    let mut output = vec![0; payload.len() + padding + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new()
        .map_err(|e| AesKeyWrapError(crate::BackendError(e, "creating cipher context")))?;

    ctx.set_flags(openssl::cipher_ctx::CipherCtxFlags::FLAG_WRAP_ALLOW);
    ctx.encrypt_init(Some(cipher), Some(wrapping_key), None)
        .map_err(|e| AesKeyWrapError(crate::BackendError(e, "wrap init")))?;

    let count = ctx
        .cipher_update(payload, Some(&mut output))
        .map_err(|e| AesKeyWrapError(crate::BackendError(e, "wrapping")))?;
    output.truncate(count);

    Ok(output)
}

pub fn unwrap(unwrapping_key: &[u8], wrapped_payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
    let cipher = select_cipher(unwrapping_key.len())?;
    let mut output = vec![0; wrapped_payload.len() + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new()
        .map_err(|e| AesKeyWrapError(crate::BackendError(e, "creating cipher context")))?;

    ctx.set_flags(openssl::cipher_ctx::CipherCtxFlags::FLAG_WRAP_ALLOW);
    ctx.decrypt_init(Some(cipher), Some(unwrapping_key), None)
        .map_err(|e| AesKeyWrapError(crate::BackendError(e, "unwrap init")))?;

    let count = ctx
        .cipher_update(wrapped_payload, Some(&mut output))
        .map_err(|e| AesKeyWrapError(crate::BackendError(e, "unwrapping")))?;
    output.truncate(count);

    Ok(output)
}
