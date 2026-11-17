// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HMAC implementation using OpenSSL.

use super::HmacError;
use crate::HashAlgorithm;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> HmacError {
    HmacError(crate::BackendError(err, op))
}

pub fn hmac(key: &[u8], data: &[u8], hash_algorithm: HashAlgorithm) -> Result<Vec<u8>, HmacError> {
    let pkey = openssl::pkey::PKey::hmac(key).map_err(|e| err(e, "creating the HMAC key"))?;
    let mut ctx =
        openssl::md_ctx::MdCtx::new().map_err(|e| err(e, "creating the message digest context"))?;

    ctx.digest_sign_init(Some(hash_algorithm.into()), &pkey)
        .map_err(|e| err(e, "initializing the HMAC context"))?;
    ctx.digest_sign_update(data)
        .map_err(|e| err(e, "hashing the input data"))?;

    let size = ctx
        .digest_sign_final(None)
        .map_err(|e| err(e, "querying the HMAC output size"))?;

    let mut output = vec![0u8; size];
    let written = ctx
        .digest_sign_final(Some(&mut output))
        .map_err(|e| err(e, "finalizing the HMAC"))?;
    assert_eq!(written, size);

    Ok(output)
}
