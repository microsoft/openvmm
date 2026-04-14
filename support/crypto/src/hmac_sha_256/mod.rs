// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HMAC-SHA-256 message authentication.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// An error for HMAC-SHA-256 operations.
#[derive(Clone, Debug, Error)]
#[error("HMAC-SHA-256 error")]
pub struct HmacSha256Error(#[source] super::BackendError);

/// Compute the HMAC-SHA-256 of `data` using `key`.
// TODO: Consider splitting up into more steps to allow caching of intermediate state.
pub fn hmac_sha_256(key: &[u8], data: &[u8]) -> Result<[u8; 32], HmacSha256Error> {
    sys::hmac_sha_256(key, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha_256_known_vectors() {
        let key: Vec<u8> = (0..32).collect();

        const EMPTY_HMAC: [u8; 32] = [
            0xd3, 0x8b, 0x42, 0x09, 0x6d, 0x80, 0xf4, 0x5f, 0x82, 0x6b, 0x44, 0xa9, 0xd5, 0x60,
            0x7d, 0xe7, 0x24, 0x96, 0xa4, 0x15, 0xd3, 0xf4, 0xa1, 0xa8, 0xc8, 0x8e, 0x3b, 0xb9,
            0xda, 0x8d, 0xc1, 0xcb,
        ];

        let hmac = hmac_sha_256(key.as_slice(), &[]).unwrap();
        assert_eq!(hmac, EMPTY_HMAC);

        const PANGRAM: [u8; 32] = [
            0xf8, 0x7a, 0xd2, 0x56, 0x15, 0x1f, 0xc7, 0xb4, 0xc5, 0xdf, 0xfa, 0x4a, 0xdb, 0x3e,
            0xbe, 0x91, 0x1a, 0x8e, 0xeb, 0x8a, 0x8e, 0xbd, 0xee, 0x3c, 0x2a, 0x4a, 0x8e, 0x5f,
            0x5e, 0xc0, 0x2c, 0x32,
        ];

        let hmac = hmac_sha_256(
            key.as_slice(),
            b"The quick brown fox jumps over the lazy dog",
        )
        .unwrap();
        assert_eq!(hmac, PANGRAM);
    }
}
