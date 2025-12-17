// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the required cryptographic functions for the crate.

use openhcl_attestation_protocol::vmgs::AES_GCM_KEY_LENGTH;
use openhcl_attestation_protocol::vmgs::HMAC_SHA_256_KEY_LENGTH;
use openssl::pkey::Private;
use openssl::rsa::Rsa;
use openssl_kdf::kdf::Kbkdf;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum KbkdfError {
    #[error("KDF derivation failed")]
    Derive(#[from] openssl_kdf::kdf::KdfError),
}

#[derive(Debug, Error)]
pub(crate) enum Pkcs11RsaAesKeyUnwrapError {
    #[error("expected wrapped AES key blob to be {0} bytes, but found {1} bytes")]
    UndersizedWrappedAesKey(usize, usize),
    #[error("wrapped RSA key blob cannot be empty")]
    EmptyWrappedRsaKey,
    #[error("RSA unwrap failed")]
    RsaUnwrap(#[from] RsaOaepError),
    #[error("AES unwrap failed")]
    AesUnwrap(#[from] AesKeyWrapWithPaddingError),
    #[error("failed to convert PKCS #8 DER format to PKey")]
    ConvertPkcs8DerToPkey(#[source] openssl::error::ErrorStack),
    #[error("failed to get an RSA key from PKey")]
    PkeyToRsa(#[from] openssl::error::ErrorStack),
}

#[derive(Debug, Error)]
pub(crate) enum RsaOaepError {
    #[error("failed to convert an RSA key to PKey")]
    RsaToPkey(#[source] openssl::error::ErrorStack),
    #[error("Pkeyctx::new() failed")]
    PkeyCtxNew(#[source] openssl::error::ErrorStack),
    #[error("PkeyCtx encrypt_init() failed")]
    PkeyCtxEncryptInit(#[source] openssl::error::ErrorStack),
    #[error("PkeyCtx decrypt_init() failed")]
    PkeyCtxDecryptInit(#[source] openssl::error::ErrorStack),
    #[error("PkeyCtx set_rsa_padding() failed")]
    PkeyCtxSetRsaPadding(#[source] openssl::error::ErrorStack),
    #[error("PkeyCtx set_rsa_oaep_md() failed")]
    PkeyCtxSetRsaOaepMd(#[source] openssl::error::ErrorStack),
    #[error("Encryption failed, OAEP hash algorithm {1:?}")]
    Encrypt(#[source] openssl::error::ErrorStack, RsaOaepHashAlgorithm),
    #[error("Decryption failed, OAEP hash algorithm {1:?}")]
    Decrypt(#[source] openssl::error::ErrorStack, RsaOaepHashAlgorithm),
}

#[derive(Debug, Error)]
pub(crate) enum AesKeyWrapWithPaddingError {
    #[error("invalid wrapping key size {0}")]
    InvalidWrappingKeySize(usize),
    #[error("Invalid unwrapping key size {0}")]
    InvalidUnwrappingKeySize(usize),
    #[error("CipherCtx::new failed")]
    CipherCtxNew(#[source] openssl::error::ErrorStack),
    #[error("CipherCtx encrypt_init() failed")]
    CipherCtxEncryptInit(#[source] openssl::error::ErrorStack),
    #[error("CipherCtx decrypt_init() failed")]
    CipherCtxDecryptInit(#[source] openssl::error::ErrorStack),
    #[error("AES key wrap with padding update failed")]
    WrapUpdate(#[source] openssl::error::ErrorStack),
    #[error("AES key unwrap with padding update failed")]
    UnwrapUpdate(#[source] openssl::error::ErrorStack),
}

#[derive(Debug, Error)]
pub(crate) enum Aes256CbcError {
    #[error("CipherCtx::new failed")]
    CipherCtxNew(#[source] openssl::error::ErrorStack),
    #[error("CipherCtx encrypt_init() failed")]
    CipherCtxEncryptInit(#[source] openssl::error::ErrorStack),
    #[error("CipherCtx decrypt_init() failed")]
    CipherCtxDecryptInit(#[source] openssl::error::ErrorStack),
    #[error("AES-256-CBC encrypt failed")]
    Encrypt(#[source] openssl::error::ErrorStack),
    #[error("AES-256-CBC decrypt failed")]
    Decrypt(#[source] openssl::error::ErrorStack),
}

#[derive(Debug, Error)]
pub(crate) enum HmacSha256Error {
    #[error("failed to convert an HMAC key to PKey")]
    HmacKeyToPkey(#[source] openssl::error::ErrorStack),
    #[error("MdCtx::new failed")]
    MdCtxNew(#[source] openssl::error::ErrorStack),
    #[error("HMAC init failed")]
    HmacInit(#[source] openssl::error::ErrorStack),
    #[error("HMAC update failed")]
    HmacUpdate(#[source] openssl::error::ErrorStack),
    #[error("HMAC final failed")]
    HmacFinal(#[source] openssl::error::ErrorStack),
    #[error("failed to get the required HMAC output size")]
    GetHmacRequiredSize(#[source] openssl::error::ErrorStack),
    #[error("HMAC SHA 256 failed")]
    OpenSSL(#[from] openssl::error::ErrorStack),
    #[error("invalid output size {0}, expected {1}")]
    InvalidOutputSize(usize, usize),
}

/// KBKDF from SP800-108, using EVP_KDF functionality of OpenSSL
pub fn derive_key(
    key: &[u8],
    context: &[u8],
    label: &[u8],
) -> Result<[u8; AES_GCM_KEY_LENGTH], KbkdfError> {
    // SP800-108's Label is called "Salt" in OpenSSL
    let mut kdf = Kbkdf::new(
        openssl::hash::MessageDigest::sha256(),
        label.to_vec(),
        key.to_vec(),
    );
    kdf.set_context(context.to_vec());
    let mut output = [0; AES_GCM_KEY_LENGTH];
    openssl_kdf::kdf::derive(kdf, &mut output)?;
    Ok(output)
}

/// PKCS#11 RSA AES key unwrap implementation
pub fn pkcs11_rsa_aes_key_unwrap(
    unwrapping_rsa_key: &Rsa<Private>,
    wrapped_key_blob: &[u8],
) -> Result<Rsa<Private>, Pkcs11RsaAesKeyUnwrapError> {
    let modulus_size = unwrapping_rsa_key.size() as usize;

    let (wrapped_aes_key, wrapped_rsa_key) = wrapped_key_blob
        .split_at_checked(modulus_size)
        .ok_or_else(|| {
            Pkcs11RsaAesKeyUnwrapError::UndersizedWrappedAesKey(
                modulus_size,
                wrapped_key_blob.len(),
            )
        })?;

    if wrapped_rsa_key.is_empty() {
        return Err(Pkcs11RsaAesKeyUnwrapError::EmptyWrappedRsaKey);
    }

    let unwrapped_aes_key = rsa_oaep_decrypt(
        unwrapping_rsa_key,
        wrapped_aes_key,
        RsaOaepHashAlgorithm::Sha1,
    )
    .map_err(Pkcs11RsaAesKeyUnwrapError::RsaUnwrap)?;
    let unwrapped_rsa_key = aes_key_unwrap_with_padding(&unwrapped_aes_key, wrapped_rsa_key)
        .map_err(Pkcs11RsaAesKeyUnwrapError::AesUnwrap)?;
    let unwrapped_rsa_key = openssl::pkey::PKey::private_key_from_pkcs8(&unwrapped_rsa_key)
        .map_err(Pkcs11RsaAesKeyUnwrapError::ConvertPkcs8DerToPkey)?;
    let unwrapped_rsa_key = unwrapped_rsa_key
        .rsa()
        .map_err(Pkcs11RsaAesKeyUnwrapError::PkeyToRsa)?;

    Ok(unwrapped_rsa_key)
}

/// Support RSA-OAEP with SHA-1 or SHA-256 from OpenSSL
#[derive(Debug)]
pub enum RsaOaepHashAlgorithm {
    /// SHA-1
    Sha1,
    /// SHA-256
    Sha256,
}

/// RSA-OAEP encrypt
pub fn rsa_oaep_encrypt(
    rsa: &Rsa<Private>,
    input: &[u8],
    hash_algorithm: RsaOaepHashAlgorithm,
) -> Result<Vec<u8>, RsaOaepError> {
    let pkey = openssl::pkey::PKey::from_rsa(rsa.to_owned()).map_err(RsaOaepError::RsaToPkey)?;
    let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&pkey).map_err(RsaOaepError::PkeyCtxNew)?;

    ctx.encrypt_init()
        .map_err(RsaOaepError::PkeyCtxEncryptInit)?;
    ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
        .map_err(RsaOaepError::PkeyCtxSetRsaPadding)?;

    match hash_algorithm {
        RsaOaepHashAlgorithm::Sha1 => ctx.set_rsa_oaep_md(openssl::md::Md::sha1()),
        RsaOaepHashAlgorithm::Sha256 => ctx.set_rsa_oaep_md(openssl::md::Md::sha256()),
    }
    .map_err(RsaOaepError::PkeyCtxSetRsaOaepMd)?;

    let mut output = vec![];
    ctx.encrypt_to_vec(input, &mut output)
        .map_err(|e| RsaOaepError::Encrypt(e, hash_algorithm))?;

    Ok(output)
}

/// RSA-OAEP decrypt
pub fn rsa_oaep_decrypt(
    rsa: &Rsa<Private>,
    input: &[u8],
    hash_algorithm: RsaOaepHashAlgorithm,
) -> Result<Vec<u8>, RsaOaepError> {
    let pkey = openssl::pkey::PKey::from_rsa(rsa.to_owned()).map_err(RsaOaepError::RsaToPkey)?;
    let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&pkey).map_err(RsaOaepError::PkeyCtxNew)?;

    ctx.decrypt_init()
        .map_err(RsaOaepError::PkeyCtxDecryptInit)?;
    ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
        .map_err(RsaOaepError::PkeyCtxSetRsaPadding)?;

    match hash_algorithm {
        RsaOaepHashAlgorithm::Sha1 => ctx.set_rsa_oaep_md(openssl::md::Md::sha1()),
        RsaOaepHashAlgorithm::Sha256 => ctx.set_rsa_oaep_md(openssl::md::Md::sha256()),
    }
    .map_err(RsaOaepError::PkeyCtxSetRsaOaepMd)?;

    let mut output = vec![];
    ctx.decrypt_to_vec(input, &mut output)
        .map_err(|e| RsaOaepError::Decrypt(e, hash_algorithm))?;

    Ok(output)
}

/// Key wrap with padding scheme (RFC 5649) implementation from OpenSSL
pub fn aes_key_wrap_with_padding(
    wrapping_key: &[u8],
    payload: &[u8],
) -> Result<Vec<u8>, AesKeyWrapWithPaddingError> {
    let cipher = match wrapping_key.len() {
        16 => openssl::cipher::Cipher::aes_128_wrap_pad(),
        24 => openssl::cipher::Cipher::aes_192_wrap_pad(),
        32 => openssl::cipher::Cipher::aes_256_wrap_pad(),
        key_size => Err(AesKeyWrapWithPaddingError::InvalidWrappingKeySize(key_size))?,
    };
    let padding = 8 - payload.len() % 8;
    let mut output = vec![0; payload.len() + padding + cipher.block_size()];
    let mut ctx =
        openssl::cipher_ctx::CipherCtx::new().map_err(AesKeyWrapWithPaddingError::CipherCtxNew)?;

    ctx.set_flags(openssl::cipher_ctx::CipherCtxFlags::FLAG_WRAP_ALLOW);
    ctx.encrypt_init(Some(cipher), Some(wrapping_key), None)
        .map_err(AesKeyWrapWithPaddingError::CipherCtxEncryptInit)?;

    let count = ctx
        .cipher_update(payload, Some(&mut output))
        .map_err(AesKeyWrapWithPaddingError::WrapUpdate)?;
    // DEVNOTE: Skip the `cipher_final()`, which is effectively a no-op for this operation
    // according to OpenSSL implementation.
    output.truncate(count);

    Ok(output)
}

/// Key unwrap with padding scheme (RFC 5649) implementation from OpenSSL
pub fn aes_key_unwrap_with_padding(
    unwrapping_key: &[u8],
    wrapped_payload: &[u8],
) -> Result<Vec<u8>, AesKeyWrapWithPaddingError> {
    let cipher = match unwrapping_key.len() {
        16 => openssl::cipher::Cipher::aes_128_wrap_pad(),
        24 => openssl::cipher::Cipher::aes_192_wrap_pad(),
        32 => openssl::cipher::Cipher::aes_256_wrap_pad(),
        key_size => Err(AesKeyWrapWithPaddingError::InvalidUnwrappingKeySize(
            key_size,
        ))?,
    };
    let mut output = vec![0; wrapped_payload.len() + cipher.block_size()];
    let mut ctx =
        openssl::cipher_ctx::CipherCtx::new().map_err(AesKeyWrapWithPaddingError::CipherCtxNew)?;

    ctx.set_flags(openssl::cipher_ctx::CipherCtxFlags::FLAG_WRAP_ALLOW);
    ctx.decrypt_init(Some(cipher), Some(unwrapping_key), None)
        .map_err(AesKeyWrapWithPaddingError::CipherCtxDecryptInit)?;

    let count = ctx
        .cipher_update(wrapped_payload, Some(&mut output))
        .map_err(AesKeyWrapWithPaddingError::UnwrapUpdate)?;
    // DEVNOTE: Skip the `cipher_final()`, which is effectively a no-op for this operation
    // according to OpenSSL implementation.
    output.truncate(count);

    Ok(output)
}

/// AES-256 CBC encrypt
pub fn aes_256_cbc_encrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    let cipher = openssl::cipher::Cipher::aes_256_cbc();
    let mut output = vec![0u8; data.len() + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new().map_err(Aes256CbcError::CipherCtxNew)?;

    ctx.encrypt_init(Some(cipher), Some(key), Some(iv))
        .map_err(Aes256CbcError::CipherCtxEncryptInit)?;
    ctx.set_padding(false);

    let count = ctx
        .cipher_update(data, Some(&mut output))
        .map_err(Aes256CbcError::Encrypt)?;
    let rest = ctx
        .cipher_final(&mut output[count..])
        .map_err(Aes256CbcError::Encrypt)?;
    output.truncate(count + rest);

    Ok(output)
}

/// AES-256 CBC decrypt
pub fn aes_256_cbc_decrypt(key: &[u8], data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
    let cipher = openssl::cipher::Cipher::aes_256_cbc();
    let mut output = vec![0u8; data.len() + cipher.block_size()];
    let mut ctx = openssl::cipher_ctx::CipherCtx::new().map_err(Aes256CbcError::CipherCtxNew)?;

    ctx.decrypt_init(Some(cipher), Some(key), Some(iv))
        .map_err(Aes256CbcError::CipherCtxDecryptInit)?;
    ctx.set_padding(false);

    let count = ctx
        .cipher_update(data, Some(&mut output))
        .map_err(Aes256CbcError::Decrypt)?;
    let rest = ctx
        .cipher_final(&mut output[count..])
        .map_err(Aes256CbcError::Decrypt)?;
    output.truncate(count + rest);

    Ok(output)
}

/// HMAC-SHA-256
pub fn hmac_sha_256(
    key: &[u8],
    data: &[u8],
) -> Result<[u8; HMAC_SHA_256_KEY_LENGTH], HmacSha256Error> {
    let pkey = openssl::pkey::PKey::hmac(key).map_err(HmacSha256Error::HmacKeyToPkey)?;
    let mut ctx = openssl::md_ctx::MdCtx::new().map_err(HmacSha256Error::MdCtxNew)?;

    ctx.digest_sign_init(Some(openssl::md::Md::sha256()), &pkey)
        .map_err(HmacSha256Error::HmacInit)?;
    ctx.digest_sign_update(data)
        .map_err(HmacSha256Error::HmacUpdate)?;

    let size = ctx
        .digest_sign_final(None)
        .map_err(HmacSha256Error::GetHmacRequiredSize)?;
    if size != HMAC_SHA_256_KEY_LENGTH {
        Err(HmacSha256Error::InvalidOutputSize(
            size,
            HMAC_SHA_256_KEY_LENGTH,
        ))?
    }

    let mut output = [0u8; HMAC_SHA_256_KEY_LENGTH];
    ctx.digest_sign_final(Some(&mut output))
        .map_err(HmacSha256Error::HmacFinal)?;

    Ok(output)
}

/// SHA-256
pub fn sha_256(data: &[u8]) -> [u8; 32] {
    let mut hasher = openssl::sha::Sha256::new();
    hasher.update(data);
    hasher.finish()
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
        let rsa = Rsa::generate(2048).unwrap();

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
        let target_key = Rsa::generate(2048).unwrap();
        let pkcs8_target_key = openssl::pkey::PKey::from_rsa(target_key.clone())
            .unwrap()
            .private_key_to_pkcs8()
            .unwrap();

        let mut wrapping_aes_key = [0u8; 32];
        getrandom::fill(&mut wrapping_aes_key[..]).unwrap();

        let wrapping_rsa_key = Rsa::generate(2048).unwrap();
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
