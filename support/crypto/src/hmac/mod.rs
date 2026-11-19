// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HMAC message authentication.

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

/// An error for HMAC operations.
#[derive(Clone, Debug, Error)]
#[error("HMAC operation failed")]
pub struct HmacError(#[source] super::BackendError);

/// Compute the HMAC of `data` using `key` and the specified hash algorithm.
/// Empty keys are rejected.
// TODO: Consider splitting up into more steps to allow caching of intermediate state.
pub fn hmac(key: &[u8], data: &[u8], hash_algorithm: HashAlgorithm) -> Result<Vec<u8>, HmacError> {
    sys::hmac(key, data, hash_algorithm)
}

/// Verify a full-length HMAC using the specified hash algorithm.
///
/// Returns `Ok(true)` for a valid tag, `Ok(false)` for an invalid tag
/// (including a wrong length), or an error if computing the HMAC fails.
/// Comparison is constant-time with respect to tag contents, not tag length.
pub fn verify(
    key: &[u8],
    data: &[u8],
    tag: &[u8],
    hash_algorithm: HashAlgorithm,
) -> Result<bool, HmacError> {
    let computed = hmac(key, data, hash_algorithm)?;
    Ok(constant_time_eq::constant_time_eq(&computed, tag))
}

#[cfg(test)]
mod tests {
    #![expect(deprecated)]

    use super::*;

    #[test]
    fn hmac_sha_256_known_vectors() {
        let key: Vec<u8> = (0..32).collect();

        const EMPTY_HMAC: [u8; 32] = [
            0xd3, 0x8b, 0x42, 0x09, 0x6d, 0x80, 0xf4, 0x5f, 0x82, 0x6b, 0x44, 0xa9, 0xd5, 0x60,
            0x7d, 0xe7, 0x24, 0x96, 0xa4, 0x15, 0xd3, 0xf4, 0xa1, 0xa8, 0xc8, 0x8e, 0x3b, 0xb9,
            0xda, 0x8d, 0xc1, 0xcb,
        ];

        let hmac = hmac(key.as_slice(), &[], HashAlgorithm::Sha256).unwrap();
        assert_eq!(hmac, EMPTY_HMAC);

        const PANGRAM: [u8; 32] = [
            0xf8, 0x7a, 0xd2, 0x56, 0x15, 0x1f, 0xc7, 0xb4, 0xc5, 0xdf, 0xfa, 0x4a, 0xdb, 0x3e,
            0xbe, 0x91, 0x1a, 0x8e, 0xeb, 0x8a, 0x8e, 0xbd, 0xee, 0x3c, 0x2a, 0x4a, 0x8e, 0x5f,
            0x5e, 0xc0, 0x2c, 0x32,
        ];

        let hmac = super::hmac(
            key.as_slice(),
            b"The quick brown fox jumps over the lazy dog",
            HashAlgorithm::Sha256,
        )
        .unwrap();
        assert_eq!(hmac, PANGRAM);
    }

    #[test]
    fn known_vectors_all_algorithms() {
        // RFC 2202 and RFC 4231, test case 1.
        for (algorithm, expected) in [
            (
                HashAlgorithm::Sha1,
                "b617318655057264e28bc0b6fb378c8ef146be00",
            ),
            (
                HashAlgorithm::Sha256,
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                HashAlgorithm::Sha384,
                "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59cfaea9ea9076ede7f4af152e8b2fa9cb6",
            ),
            (
                HashAlgorithm::Sha512,
                "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cde\
                 daa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854",
            ),
        ] {
            let expected = hex::decode(expected).unwrap();
            assert_eq!(hmac(&[0x0b; 20], b"Hi There", algorithm).unwrap(), expected);
            assert!(verify(&[0x0b; 20], b"Hi There", &expected, algorithm).unwrap());
        }
    }

    #[test]
    fn verify_accepts_only_matching_full_length_tags() {
        for algorithm in [
            HashAlgorithm::Sha1,
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha384,
            HashAlgorithm::Sha512,
        ] {
            for key_len in [20, 64, 128, 131] {
                let key = vec![0xaa; key_len];
                for data in [b"".as_slice(), b"authenticated message".as_slice()] {
                    let mut tag = hmac(&key, data, algorithm).unwrap();
                    assert!(verify(&key, data, &tag, algorithm).unwrap());
                    assert!(!verify(b"wrong key", data, &tag, algorithm).unwrap());
                    assert!(!verify(&key, b"wrong message", &tag, algorithm).unwrap());

                    for i in 0..tag.len() {
                        tag[i] ^= 1;
                        assert!(!verify(&key, data, &tag, algorithm).unwrap());
                        tag[i] ^= 1;
                    }

                    for len in 0..tag.len() {
                        assert!(!verify(&key, data, &tag[..len], algorithm).unwrap());
                    }
                    tag.push(0);
                    assert!(!verify(&key, data, &tag, algorithm).unwrap());
                }
            }
        }
    }

    #[test]
    fn empty_key_errors_are_propagated() {
        for algorithm in [
            HashAlgorithm::Sha1,
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha384,
            HashAlgorithm::Sha512,
        ] {
            let tag = hmac(b"key", b"message", algorithm).unwrap();
            assert!(hmac(&[], b"message", algorithm).is_err());
            assert!(verify(&[], b"message", &tag, algorithm).is_err());
        }
    }
}
