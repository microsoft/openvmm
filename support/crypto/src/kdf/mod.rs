// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SP800-108 KBKDF (Key-Based Key Derivation Function) using HMAC-SHA-256.

#![cfg(openssl)]

#[cfg(openssl)]
mod ossl;
#[cfg(openssl)]
use ossl as sys;

use thiserror::Error;

/// An error for KDF operations.
// DEVNOTE: This would use BackendError if the kdf functions were implemented in the openssl crate itself
#[cfg(openssl)]
#[derive(Debug, Error)]
#[error("KDF derivation error")]
pub struct KdfError(#[source] openssl_kdf::kdf::KdfError);

/// An error for KDF operations.
#[cfg(not(openssl))]
#[derive(Debug, Error)]
#[error("KDF derivation error")]
pub struct KdfError(#[source] super::BackendError);

/// Derive key material using SP800-108 KBKDF with HMAC-SHA-256 in counter
/// mode.
// TODO: Consider splitting up into more steps to allow caching of intermediate state.
pub fn kbkdf_hmac_sha256(
    key: &[u8],
    context: &[u8],
    salt: &[u8],
    output_len: usize,
) -> Result<Vec<u8>, KdfError> {
    sys::kbkdf_hmac_sha256(key, context, salt, output_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kdf_kat_one() {
        let key = [0; 32];
        let context = [
            0x28, 0x84, 0x18, 0x6c, 0xfe, 0xd2, 0x50, 0x41, 0x10, 0x69, 0x8b, 0x45, 0xd4, 0x80,
            0x72, 0x88, 0xdf, 0x67, 0x4c, 0x48, 0x26, 0x19, 0x7a, 0x98, 0x69, 0x88, 0xaf, 0x96,
            0x05, 0x62, 0xf5, 0x7f,
        ];
        let expected_result = [
            0x9d, 0xb5, 0x8b, 0xb7, 0x0c, 0xa6, 0xcb, 0x6f, 0xaa, 0xe3, 0x81, 0x74, 0x64, 0x21,
            0x76, 0xfa, 0x0d, 0xed, 0x28, 0x67, 0x30, 0x76, 0x90, 0x83, 0x83, 0xa0, 0x1a, 0xd7,
            0x2e, 0xc3, 0xe2, 0x3b,
        ];

        let output = kbkdf_hmac_sha256(&key, &context, b"VMGSKEY", expected_result.len()).unwrap();

        assert_eq!(output, expected_result);
    }

    #[test]
    fn kdf_kat_two() {
        let key = [0; 32];
        let context = [
            0xd6, 0x8a, 0x8d, 0x52, 0x7c, 0x5c, 0xa5, 0x9b, 0x19, 0x5a, 0xe7, 0x45, 0x6c, 0x3f,
            0xef, 0x4d, 0x0e, 0xb0, 0xbe, 0x16, 0xc7, 0x8d, 0x77, 0xbd, 0x28, 0x5a, 0xa1, 0x45,
            0x3e, 0x24, 0xeb, 0x3f,
        ];
        let expected_result = [
            0x0a, 0xda, 0x54, 0x91, 0xd6, 0x09, 0x92, 0x87, 0x2f, 0xd7, 0x1a, 0x15, 0x71, 0x24,
            0x82, 0x36, 0x25, 0xb4, 0xb9, 0x54, 0xc2, 0xf4, 0xeb, 0x47, 0x02, 0x88, 0x42, 0x7b,
            0x1f, 0x8e, 0xdf, 0x3d,
        ];

        let output = kbkdf_hmac_sha256(&key, &context, b"VMGSKEY", expected_result.len()).unwrap();

        assert_eq!(output, expected_result);
    }
}
