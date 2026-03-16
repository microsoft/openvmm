// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HMAC-SHA-256.

use thiserror::Error;

/// The output length of HMAC-SHA-256 in bytes.
pub const OUTPUT_LEN: usize = 32;

/// Error returned by HMAC-SHA-256.
#[derive(Debug, Error)]
pub enum HmacSha256Error {
    #[error("failed to convert an HMAC key to PKey")]
    HmacKeyToPkey(#[source] openssl::error::ErrorStack),
    #[error("MdCtx::new failed")]
    MdCtxNew(#[source] openssl::error::ErrorStack),
    #[error("HMAC init failed")]
    HmacInit(#[source] openssl::error::ErrorStack),
    #[error("HMAC update failed")]
    HmacUpdate(#[source] openssl::error::ErrorStack),
    #[error("HMAC final failed")]
    HmacFinal(#[source] openssl::error::ErrorStack),
    #[error("failed to get the required HMAC output size")]
    GetHmacRequiredSize(#[source] openssl::error::ErrorStack),
    #[error("HMAC SHA 256 failed")]
    OpenSSL(#[from] openssl::error::ErrorStack),
    #[error("invalid output size {0}, expected {1}")]
    InvalidOutputSize(usize, usize),
}

/// Compute HMAC-SHA-256 of `data` with the given `key`.
pub fn hmac_sha_256(key: &[u8], data: &[u8]) -> Result<[u8; OUTPUT_LEN], HmacSha256Error> {
    let pkey = openssl::pkey::PKey::hmac(key).map_err(HmacSha256Error::HmacKeyToPkey)?;
    let mut ctx = openssl::md_ctx::MdCtx::new().map_err(HmacSha256Error::MdCtxNew)?;

    ctx.digest_sign_init(Some(openssl::md::Md::sha256()), &pkey)
        .map_err(HmacSha256Error::HmacInit)?;
    ctx.digest_sign_update(data)
        .map_err(HmacSha256Error::HmacUpdate)?;

    let size = ctx
        .digest_sign_final(None)
        .map_err(HmacSha256Error::GetHmacRequiredSize)?;
    if size != OUTPUT_LEN {
        return Err(HmacSha256Error::InvalidOutputSize(size, OUTPUT_LEN));
    }

    let mut output = [0u8; OUTPUT_LEN];
    ctx.digest_sign_final(Some(&mut output))
        .map_err(HmacSha256Error::HmacFinal)?;

    Ok(output)
}
