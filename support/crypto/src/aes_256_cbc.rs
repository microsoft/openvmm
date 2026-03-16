// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES-256-CBC encryption and decryption.

use thiserror::Error;

/// Error returned by AES-256-CBC operations.
#[derive(Debug, Error)]
pub enum Aes256CbcError {
    #[error("CipherCtx::new failed")]
    CipherCtxNew(#[source] openssl::error::ErrorStack),
    #[error("CipherCtx encrypt_init() failed")]
    CipherCtxEncryptInit(#[source] openssl::error::ErrorStack),
    #[error("CipherCtx decrypt_init() failed")]
    CipherCtxDecryptInit(#[source] openssl::error::ErrorStack),
    #[error("AES-256-CBC encrypt failed")]
    Encrypt(#[source] openssl::error::ErrorStack),
    #[error("AES-256-CBC decrypt failed")]
    Decrypt(#[source] openssl::error::ErrorStack),
}

/// AES-256-CBC encrypt with no padding.
pub fn encrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    let cipher = openssl::cipher::Cipher::aes_256_cbc();
    let mut output = vec![0u8; data.len() + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new().map_err(Aes256CbcError::CipherCtxNew)?;

    ctx.encrypt_init(Some(cipher), Some(key), Some(iv))
        .map_err(Aes256CbcError::CipherCtxEncryptInit)?;
    ctx.set_padding(false);

    let count = ctx
        .cipher_update(data, Some(&mut output))
        .map_err(Aes256CbcError::Encrypt)?;
    let rest = ctx
        .cipher_final(&mut output[count..])
        .map_err(Aes256CbcError::Encrypt)?;
    output.truncate(count + rest);

    Ok(output)
}

/// AES-256-CBC decrypt with no padding.
pub fn decrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    let cipher = openssl::cipher::Cipher::aes_256_cbc();
    let mut output = vec![0u8; data.len() + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new().map_err(Aes256CbcError::CipherCtxNew)?;

    ctx.decrypt_init(Some(cipher), Some(key), Some(iv))
        .map_err(Aes256CbcError::CipherCtxDecryptInit)?;
    ctx.set_padding(false);

    let count = ctx
        .cipher_update(data, Some(&mut output))
        .map_err(Aes256CbcError::Decrypt)?;
    let rest = ctx
        .cipher_final(&mut output[count..])
        .map_err(Aes256CbcError::Decrypt)?;
    output.truncate(count + rest);

    Ok(output)
}
