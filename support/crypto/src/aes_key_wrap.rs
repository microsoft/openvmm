// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES key wrap with padding (RFC 5649).

use thiserror::Error;

/// Error returned by AES key wrap operations.
#[derive(Debug, Error)]
pub enum AesKeyWrapError {
    #[error("invalid wrapping key size {0}")]
    InvalidWrappingKeySize(usize),
    #[error("invalid unwrapping key size {0}")]
    InvalidUnwrappingKeySize(usize),
    #[error("CipherCtx::new failed")]
    CipherCtxNew(#[source] openssl::error::ErrorStack),
    #[error("CipherCtx encrypt_init() failed")]
    CipherCtxEncryptInit(#[source] openssl::error::ErrorStack),
    #[error("CipherCtx decrypt_init() failed")]
    CipherCtxDecryptInit(#[source] openssl::error::ErrorStack),
    #[error("AES key wrap with padding update failed")]
    WrapUpdate(#[source] openssl::error::ErrorStack),
    #[error("AES key unwrap with padding update failed")]
    UnwrapUpdate(#[source] openssl::error::ErrorStack),
}

/// Wrap `payload` using AES key wrap with padding (RFC 5649).
pub fn wrap(wrapping_key: &[u8], payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
    let cipher = match wrapping_key.len() {
        16 => openssl::cipher::Cipher::aes_128_wrap_pad(),
        24 => openssl::cipher::Cipher::aes_192_wrap_pad(),
        32 => openssl::cipher::Cipher::aes_256_wrap_pad(),
        key_size => return Err(AesKeyWrapError::InvalidWrappingKeySize(key_size)),
    };
    let padding = 8 - payload.len() % 8;
    let mut output = vec![0; payload.len() + padding + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new().map_err(AesKeyWrapError::CipherCtxNew)?;

    ctx.set_flags(openssl::cipher_ctx::CipherCtxFlags::FLAG_WRAP_ALLOW);
    ctx.encrypt_init(Some(cipher), Some(wrapping_key), None)
        .map_err(AesKeyWrapError::CipherCtxEncryptInit)?;

    let count = ctx
        .cipher_update(payload, Some(&mut output))
        .map_err(AesKeyWrapError::WrapUpdate)?;
    output.truncate(count);

    Ok(output)
}

/// Unwrap `wrapped_payload` using AES key unwrap with padding (RFC 5649).
pub fn unwrap(unwrapping_key: &[u8], wrapped_payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
    let cipher = match unwrapping_key.len() {
        16 => openssl::cipher::Cipher::aes_128_wrap_pad(),
        24 => openssl::cipher::Cipher::aes_192_wrap_pad(),
        32 => openssl::cipher::Cipher::aes_256_wrap_pad(),
        key_size => return Err(AesKeyWrapError::InvalidUnwrappingKeySize(key_size)),
    };
    let mut output = vec![0; wrapped_payload.len() + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new().map_err(AesKeyWrapError::CipherCtxNew)?;

    ctx.set_flags(openssl::cipher_ctx::CipherCtxFlags::FLAG_WRAP_ALLOW);
    ctx.decrypt_init(Some(cipher), Some(unwrapping_key), None)
        .map_err(AesKeyWrapError::CipherCtxDecryptInit)?;

    let count = ctx
        .cipher_update(wrapped_payload, Some(&mut output))
        .map_err(AesKeyWrapError::UnwrapUpdate)?;
    output.truncate(count);

    Ok(output)
}
