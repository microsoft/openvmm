// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub fn hmac_sha_256(key: &[u8], data: &[u8]) -> Result<[u8; OUTPUT_LEN], HmacSha256Error> {
    let pkey = openssl::pkey::PKey::hmac(key)
        .map_err(|e| HmacSha256Error(crate::BackendError(e, "converting HMAC key")))?;
    let mut ctx = openssl::md_ctx::MdCtx::new()
        .map_err(|e| HmacSha256Error(crate::BackendError(e, "creating context")))?;

    ctx.digest_sign_init(Some(openssl::md::Md::sha256()), &pkey)
        .map_err(|e| HmacSha256Error(crate::BackendError(e, "HMAC init")))?;
    ctx.digest_sign_update(data)
        .map_err(|e| HmacSha256Error(crate::BackendError(e, "HMAC update")))?;

    let size = ctx
        .digest_sign_final(None)
        .map_err(|e| HmacSha256Error(crate::BackendError(e, "getting HMAC output size")))?;
    assert_eq!(size, OUTPUT_LEN, "unexpected HMAC-SHA-256 output size");

    let mut output = [0u8; OUTPUT_LEN];
    ctx.digest_sign_final(Some(&mut output))
        .map_err(|e| HmacSha256Error(crate::BackendError(e, "HMAC finalization")))?;

    Ok(output)
}
