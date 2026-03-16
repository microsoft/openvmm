// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HMAC-SHA-256.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// The output length of HMAC-SHA-256 in bytes.
pub const OUTPUT_LEN: usize = 32;

/// Error returned by HMAC-SHA-256.
#[derive(Debug, Error)]
#[error("HMAC-SHA-256 error")]
pub struct HmacSha256Error(#[source] super::BackendError);

/// Compute HMAC-SHA-256 of `data` with the given `key`.
pub fn hmac_sha_256(key: &[u8], data: &[u8]) -> Result<[u8; OUTPUT_LEN], HmacSha256Error> {
    sys::hmac_sha_256(key, data)
}
