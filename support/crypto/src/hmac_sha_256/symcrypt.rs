// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::HmacSha256Error;
use ::symcrypt::errors::SymCryptError;
use ::symcrypt::hmac::hmac_sha256;

fn err(e: SymCryptError, op: &'static str) -> HmacSha256Error {
    HmacSha256Error(crate::BackendError::SymCrypt(e, op))
}

pub fn hmac_sha_256(key: &[u8], data: &[u8]) -> Result<[u8; 32], HmacSha256Error> {
    hmac_sha256(key, data).map_err(|e| err(e, "computing HMAC-SHA-256"))
}
