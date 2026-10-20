// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SP800-108 KBKDF (Key-Based Key Derivation Function) using HMAC.

#![cfg(any(openssl, symcrypt))]

#[cfg(openssl)]
mod ossl;
#[cfg(openssl)]
use ossl as sys;

#[cfg(symcrypt)]
mod symcrypt;
#[cfg(symcrypt)]
use symcrypt as sys;

use crate::HashAlgorithm;
use thiserror::Error;

/// An error for KBKDF operations.
// DEVNOTE: The openssl variant would use BackendError if the kdf functions were implemented in the openssl crate itself
#[cfg(openssl)]
#[derive(Debug, Error)]
#[error("KBKDF derivation failed")]
pub struct KbkdfError(#[source] openssl_kdf::kdf::KdfError);

/// An error for KBKDF operations.
#[cfg(not(openssl))]
#[derive(Debug, Error)]
#[error("KBKDF derivation failed")]
pub struct KbkdfError(#[source] super::BackendError);

/// Derive key material using SP800-108 KBKDF with HMAC in counter mode,
/// using `hash_algorithm` as the HMAC hash.
// TODO: Consider splitting up into more steps to allow caching of intermediate state.
pub fn kbkdf_hmac(
    key: &[u8],
    context: &[u8],
    label: &[u8],
    output_len: usize,
    hash_algorithm: HashAlgorithm,
) -> Result<Vec<u8>, KbkdfError> {
    sys::kbkdf_hmac(key, context, label, output_len, hash_algorithm)
}

#[cfg(test)]
mod tests {
    #![expect(deprecated)]

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

        let output = kbkdf_hmac(
            &key,
            &context,
            b"VMGSKEY",
            expected_result.len(),
            HashAlgorithm::Sha256,
        )
        .unwrap();

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

        let output = kbkdf_hmac(
            &key,
            &context,
            b"VMGSKEY",
            expected_result.len(),
            HashAlgorithm::Sha256,
        )
        .unwrap();

        assert_eq!(output, expected_result);
    }

    #[test]
    fn kdf_all_hash_algorithms() {
        // Independent SP800-108 vectors, with output spanning multiple HMAC blocks.
        let vectors = [
            (
                HashAlgorithm::Sha1,
                concat!(
                    "0a5e3f12af5ffc08f172425f83cbc4ac71c4d6ad990491954ab334b79ddf1b805",
                    "811260b4af1d483ad52fd9fef5e1493433b78a08a5b964436ae56b2432e5c123",
                    "71d2c373af21bf89091a975a2d9a18b",
                ),
            ),
            (
                HashAlgorithm::Sha256,
                concat!(
                    "d3e9bce30aa3d93085eb75d3d062ea7d4bb611286c1de15b5bbd32c600bdc31c",
                    "3c79095570aec8fcfdbd2b28d641dbd2e311c545e1b35a6af774b65589c7f0a2",
                    "71a0696067d4190a93f1f37c217c19a0",
                ),
            ),
            (
                HashAlgorithm::Sha384,
                concat!(
                    "1459e80748257f9bce780f29c6bdfe20dc1db5286192248b22661e9193f3d315b",
                    "20909121c49c52407d127b6c9203f1248ef42f25d00edee4cd7bee9d00e80a98",
                    "a93fa4eb4abc4d165f8dcea204694d7",
                ),
            ),
            (
                HashAlgorithm::Sha512,
                concat!(
                    "59c7a1154a015a32c5db6fa330cd63654368b9f279de66c209f12e58d0904697c",
                    "3c6f167889a3354cda3c842cd0756e8b1b8f2c3cad715ccfab2d131ae96ccfa2",
                    "ca66f5a60b9348508a5426fc3e9206b",
                ),
            ),
        ];
        for (hash_algorithm, expected) in vectors {
            let expected = hex::decode(expected).unwrap();
            let output = kbkdf_hmac(
                &[0; 32],
                &[0; 32],
                b"VMGSKEY",
                expected.len(),
                hash_algorithm,
            )
            .unwrap();
            assert_eq!(output, expected, "{hash_algorithm:?}");
        }
    }

    #[test]
    fn kdf_empty_key_errors() {
        let key = [];
        let context = [0u8; 32];
        for hash_algorithm in [
            HashAlgorithm::Sha1,
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha384,
            HashAlgorithm::Sha512,
        ] {
            let result = kbkdf_hmac(&key, &context, b"VMGSKEY", 32, hash_algorithm);
            assert!(
                result.is_err(),
                "expected error for empty key: {hash_algorithm:?}"
            );
        }
    }
}
