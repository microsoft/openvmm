// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HMAC-SHA-256 implementation using OpenSSL.

use super::HmacSha256Error;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> HmacSha256Error {
    HmacSha256Error(crate::BackendError(err, op))
}

pub fn hmac_sha_256(key: &[u8], data: &[u8]) -> Result<[u8; 32], HmacSha256Error> {
    let pkey = openssl::pkey::PKey::hmac(key).map_err(|e| err(e, "creating the HMAC key"))?;
    let mut ctx =
        openssl::md_ctx::MdCtx::new().map_err(|e| err(e, "creating the message digest context"))?;

    ctx.digest_sign_init(Some(openssl::md::Md::sha256()), &pkey)
        .map_err(|e| err(e, "initializing the HMAC context"))?;
    ctx.digest_sign_update(data)
        .map_err(|e| err(e, "hashing the input data"))?;

    let size = ctx
        .digest_sign_final(None)
        .map_err(|e| err(e, "querying the HMAC output size"))?;
    assert_eq!(size, 32);

    let mut output = [0u8; 32];
    ctx.digest_sign_final(Some(&mut output))
        .map_err(|e| err(e, "finalizing the HMAC"))?;

    Ok(output)
}
