// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the required cryptographic functions for the crate.
//!
//! Delegates to the `crypto` crate for all cryptographic operations.

pub(crate) use ::crypto::aes_256_cbc::Aes256CbcError;
pub(crate) use ::crypto::aes_key_wrap::AesKeyWrapError;
pub(crate) use ::crypto::hmac_sha_256::HmacSha256Error;
pub(crate) use ::crypto::rsa::OaepHashAlgorithm as RsaOaepHashAlgorithm;
pub(crate) use ::crypto::rsa::Pkcs11RsaAesKeyUnwrapError;
pub(crate) use ::crypto::rsa::RsaError;
pub(crate) use ::crypto::rsa::RsaKeyPair;
pub(crate) use ::crypto::rsa::RsaOaepError;

use openhcl_attestation_protocol::vmgs::AES_GCM_KEY_LENGTH;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum KbkdfError {
    #[error("KDF derivation failed")]
    Derive(#[from] ::crypto::kdf::KdfError),
}

/// KBKDF from SP800-108, using the crypto crate
pub fn derive_key(
    key: &[u8],
    context: &[u8],
    label: &[u8],
) -> Result<[u8; AES_GCM_KEY_LENGTH], KbkdfError> {
    // SP800-108's Label is called "Salt" in OpenSSL
    let mut kdf = ::crypto::kdf::Kbkdf::new(
        ::crypto::kdf::HashAlgorithm::Sha256,
        label.to_vec(),
        key.to_vec(),
    );
    kdf.set_context(context.to_vec());
    let mut output = [0; AES_GCM_KEY_LENGTH];
    ::crypto::kdf::derive(kdf, &mut output)?;
    Ok(output)
}

/// RSA-OAEP encrypt
pub fn rsa_oaep_encrypt(
    rsa: &RsaKeyPair,
    input: &[u8],
    hash_algorithm: RsaOaepHashAlgorithm,
) -> Result<Vec<u8>, RsaOaepError> {
    rsa.oaep_encrypt(input, hash_algorithm)
}

/// RSA-OAEP decrypt
pub fn rsa_oaep_decrypt(
    rsa: &RsaKeyPair,
    input: &[u8],
    hash_algorithm: RsaOaepHashAlgorithm,
) -> Result<Vec<u8>, RsaOaepError> {
    rsa.oaep_decrypt(input, hash_algorithm)
}

/// Key wrap with padding scheme (RFC 5649)
pub fn aes_key_wrap_with_padding(
    wrapping_key: &[u8],
    payload: &[u8],
) -> Result<Vec<u8>, AesKeyWrapError> {
    ::crypto::aes_key_wrap::wrap(wrapping_key, payload)
}

/// Key unwrap with padding scheme (RFC 5649)
pub fn aes_key_unwrap_with_padding(
    unwrapping_key: &[u8],
    wrapped_payload: &[u8],
) -> Result<Vec<u8>, AesKeyWrapError> {
    ::crypto::aes_key_wrap::unwrap(unwrapping_key, wrapped_payload)
}

/// AES-256 CBC encrypt
pub fn aes_256_cbc_encrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    ::crypto::aes_256_cbc::encrypt(key, data, iv)
}

/// AES-256 CBC decrypt
pub fn aes_256_cbc_decrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    ::crypto::aes_256_cbc::decrypt(key, data, iv)
}

/// HMAC-SHA-256
pub fn hmac_sha_256(
    key: &[u8],
    data: &[u8],
) -> Result<[u8; ::crypto::hmac_sha_256::OUTPUT_LEN], HmacSha256Error> {
    ::crypto::hmac_sha_256::hmac_sha_256(key, data)
}

/// SHA-256
pub fn sha_256(data: &[u8]) -> [u8; 32] {
    ::crypto::sha_256::sha_256(data)
}

/// PKCS#11 RSA AES key unwrap implementation
pub fn pkcs11_rsa_aes_key_unwrap(
    unwrapping_rsa_key: &RsaKeyPair,
    wrapped_key_blob: &[u8],
) -> Result<RsaKeyPair, Pkcs11RsaAesKeyUnwrapError> {
    unwrapping_rsa_key.pkcs11_rsa_aes_key_unwrap(wrapped_key_blob)
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

        let result = derive_key(&key, &context, crate::VMGS_KEY_DERIVE_LABEL).unwrap();

        assert_eq!(result, expected_result);
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

        let result = derive_key(&key, &context, crate::VMGS_KEY_DERIVE_LABEL).unwrap();

        assert_eq!(result, expected_result);
    }

    #[test]
    fn test_aes_key_wrap_with_padding_kat() {
        const KEK: [u8; 24] = [
            0x58, 0x40, 0xdf, 0x6e, 0x29, 0xb0, 0x2a, 0xf1, 0xab, 0x49, 0x3b, 0x70, 0x5b, 0xf1,
            0x6e, 0xa1, 0xae, 0x83, 0x38, 0xf4, 0xdc, 0xc1, 0x76, 0xa8,
        ];
        const KEY20: [u8; 20] = [
            0xc3, 0x7b, 0x7e, 0x64, 0x92, 0x58, 0x43, 0x40, 0xbe, 0xd1, 0x22, 0x07, 0x80, 0x89,
            0x41, 0x15, 0x50, 0x68, 0xf7, 0x38,
        ];
        const WRAP20: [u8; 32] = [
            0x13, 0x8b, 0xde, 0xaa, 0x9b, 0x8f, 0xa7, 0xfc, 0x61, 0xf9, 0x77, 0x42, 0xe7, 0x22,
            0x48, 0xee, 0x5a, 0xe6, 0xae, 0x53, 0x60, 0xd1, 0xae, 0x6a, 0x5f, 0x54, 0xf3, 0x73,
            0xfa, 0x54, 0x3b, 0x6a,
        ];
        const KEY7: [u8; 7] = [0x46, 0x6f, 0x72, 0x50, 0x61, 0x73, 0x69];
        const WRAP7: [u8; 16] = [
            0xaf, 0xbe, 0xb0, 0xf0, 0x7d, 0xfb, 0xf5, 0x41, 0x92, 0x00, 0xf2, 0xcc, 0xb5, 0x0b,
            0xb2, 0x4f,
        ];

        let result = aes_key_wrap_with_padding(&KEK, &KEY20);
        assert!(result.is_ok());
        let wrapped_key = result.unwrap();
        assert_eq!(wrapped_key, WRAP20);

        let result = aes_key_unwrap_with_padding(&KEK, &WRAP20);
        assert!(result.is_ok());
        let unwrapped_key = result.unwrap();
        assert_eq!(unwrapped_key, KEY20);

        let result = aes_key_wrap_with_padding(&KEK, &KEY7);
        assert!(result.is_ok());
        let wrapped_key = result.unwrap();
        assert_eq!(wrapped_key, WRAP7);

        let result = aes_key_unwrap_with_padding(&KEK, &WRAP7);
        assert!(result.is_ok());
        let unwrapped_key = result.unwrap();
        assert_eq!(unwrapped_key, KEY7);
    }

    #[test]
    fn test_aes_key_wrap_with_padding() {
        const KEY: [u8; 32] = [
            0x3f, 0xf4, 0xdb, 0xdb, 0x74, 0xd9, 0x3d, 0x22, 0x35, 0xc6, 0x7c, 0x9e, 0x17, 0x6a,
            0x88, 0x7f, 0xf9, 0x11, 0xd6, 0x5b, 0x5a, 0x56, 0x06, 0xa7, 0xfb, 0x52, 0x58, 0xfc,
            0x4e, 0x76, 0xce, 0x49,
        ];

        const AES_WRAPPED_KEY: [u8; 40] = [
            0x56, 0x53, 0xe9, 0x29, 0xa9, 0x35, 0x0c, 0x32, 0xd0, 0x24, 0x22, 0xb4, 0x98, 0xe1,
            0x13, 0xe7, 0x4a, 0x81, 0xc1, 0xf3, 0xb2, 0xa6, 0x27, 0x70, 0x6e, 0x0d, 0x12, 0x97,
            0xfd, 0xa5, 0x07, 0x0a, 0x5e, 0xb0, 0xd2, 0xde, 0xb2, 0x8a, 0x06, 0x72,
        ];

        const WRAPPING_KEY: [u8; 32] = [
            0x10, 0x84, 0xD2, 0x2F, 0x53, 0x5F, 0xD3, 0x10, 0xE2, 0xC6, 0x17, 0x31, 0x3D, 0xCA,
            0xE7, 0xEF, 0x19, 0xDD, 0x45, 0x2A, 0xED, 0x1C, 0xE6, 0xB1, 0xBE, 0xF5, 0xB9, 0xD0,
            0x1B, 0xF1, 0x5F, 0x44,
        ];

        let result = aes_key_wrap_with_padding(&WRAPPING_KEY, &KEY);
        assert!(result.is_ok());
        let wrapped_key = result.unwrap();
        assert_eq!(wrapped_key, AES_WRAPPED_KEY);

        let result = aes_key_unwrap_with_padding(&WRAPPING_KEY, &AES_WRAPPED_KEY);
        assert!(result.is_ok());
        let unwrapped_key = result.unwrap();
        assert_eq!(unwrapped_key, KEY);
    }

    #[test]
    fn fail_to_unwrap_pkcs11_rsa_aep_with_undersized_wrapped_key_blob() {
        let rsa = RsaKeyPair::generate(2048).unwrap();

        // undersized aes key blob
        let wrapped_key_blob = vec![0; 256 - 1];
        let result = pkcs11_rsa_aes_key_unwrap(&rsa, &wrapped_key_blob);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "expected wrapped AES key blob to be 256 bytes, but found 255 bytes".to_string()
        );

        // empty rsa key blob
        let wrapped_key_blob = vec![0; 256];
        let result = pkcs11_rsa_aes_key_unwrap(&rsa, &wrapped_key_blob);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "wrapped RSA key blob cannot be empty".to_string()
        );
    }

    #[test]
    fn test_pkcs11_rsa_aes_key_unwrap() {
        // Use openssl directly for test key generation
        let target_key = openssl::rsa::Rsa::generate(2048).unwrap();
        let pkcs8_target_key = openssl::pkey::PKey::from_rsa(target_key.clone())
            .unwrap()
            .private_key_to_pkcs8()
            .unwrap();

        let mut wrapping_aes_key = [0u8; 32];
        openssl::rand::rand_bytes(&mut wrapping_aes_key[..]).unwrap();

        let wrapping_rsa_key = RsaKeyPair::generate(2048).unwrap();
        let wrapped_aes_key = rsa_oaep_encrypt(
            &wrapping_rsa_key,
            &wrapping_aes_key,
            RsaOaepHashAlgorithm::Sha1,
        )
        .unwrap();
        let wrapped_target_key =
            aes_key_wrap_with_padding(&wrapping_aes_key, &pkcs8_target_key).unwrap();
        let wrapped_key_blob = [wrapped_aes_key, wrapped_target_key].concat();
        let unwrapped_target_key =
            pkcs11_rsa_aes_key_unwrap(&wrapping_rsa_key, wrapped_key_blob.as_slice()).unwrap();
        assert_eq!(
            unwrapped_target_key.private_key_to_der().unwrap(),
            target_key.private_key_to_der().unwrap()
        );
    }

    #[test]
    fn test_hmac_sha_256() {
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

    #[test]
    fn test_sha256() {
        const EMPTY_HASH: [u8; 32] = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];

        let hash = sha_256(&[]);
        assert_eq!(hash, EMPTY_HASH);

        const PANGRAM: [u8; 32] = [
            0xd7, 0xa8, 0xfb, 0xb3, 0x07, 0xd7, 0x80, 0x94, 0x69, 0xca, 0x9a, 0xbc, 0xb0, 0x08,
            0x2e, 0x4f, 0x8d, 0x56, 0x51, 0xe4, 0x6d, 0x3c, 0xdb, 0x76, 0x2d, 0x02, 0xd0, 0xbf,
            0x37, 0xc9, 0xe5, 0x92,
        ];

        let hash = sha_256(b"The quick brown fox jumps over the lazy dog");
        assert_eq!(hash, PANGRAM);
    }
}
