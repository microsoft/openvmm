// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA key operations including OAEP encryption/decryption and PKCS#11
//! key unwrapping.

use thiserror::Error;

/// An RSA key pair (private + public).
pub struct RsaKeyPair {
    inner: openssl::rsa::Rsa<openssl::pkey::Private>,
}

impl std::fmt::Debug for RsaKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsaKeyPair")
            .field("bits", &(self.inner.size() * 8))
            .finish()
    }
}

/// RSA-OAEP hash algorithm selection.
#[derive(Debug, Clone, Copy)]
pub enum OaepHashAlgorithm {
    /// SHA-1
    Sha1,
    /// SHA-256
    Sha256,
}

/// Error from RSA operations.
#[derive(Debug, Error)]
pub enum RsaError {
    #[error("failed to generate RSA key pair")]
    Generate(#[source] openssl::error::ErrorStack),
    #[error("failed to import RSA private key from DER")]
    ImportDer(#[source] openssl::error::ErrorStack),
    #[error("failed to export RSA private key to DER")]
    ExportDer(#[source] openssl::error::ErrorStack),
}

/// Error from RSA-OAEP operations.
#[derive(Debug, Error)]
pub enum RsaOaepError {
    #[error("failed to convert an RSA key to PKey")]
    RsaToPkey(#[source] openssl::error::ErrorStack),
    #[error("PkeyCtx::new() failed")]
    PkeyCtxNew(#[source] openssl::error::ErrorStack),
    #[error("PkeyCtx encrypt_init() failed")]
    PkeyCtxEncryptInit(#[source] openssl::error::ErrorStack),
    #[error("PkeyCtx decrypt_init() failed")]
    PkeyCtxDecryptInit(#[source] openssl::error::ErrorStack),
    #[error("PkeyCtx set_rsa_padding() failed")]
    PkeyCtxSetRsaPadding(#[source] openssl::error::ErrorStack),
    #[error("PkeyCtx set_rsa_oaep_md() failed")]
    PkeyCtxSetRsaOaepMd(#[source] openssl::error::ErrorStack),
    #[error("encryption failed, OAEP hash algorithm {1:?}")]
    Encrypt(#[source] openssl::error::ErrorStack, OaepHashAlgorithm),
    #[error("decryption failed, OAEP hash algorithm {1:?}")]
    Decrypt(#[source] openssl::error::ErrorStack, OaepHashAlgorithm),
}

/// Error from PKCS#11 RSA-AES key unwrap.
#[derive(Debug, Error)]
pub enum Pkcs11RsaAesKeyUnwrapError {
    #[error("expected wrapped AES key blob to be {0} bytes, but found {1} bytes")]
    UndersizedWrappedAesKey(usize, usize),
    #[error("wrapped RSA key blob cannot be empty")]
    EmptyWrappedRsaKey,
    #[error("RSA unwrap failed")]
    RsaUnwrap(#[from] RsaOaepError),
    #[error("AES unwrap failed")]
    AesUnwrap(#[from] super::aes_key_wrap::AesKeyWrapError),
    #[error("failed to convert PKCS #8 DER format to PKey")]
    ConvertPkcs8DerToPkey(#[source] openssl::error::ErrorStack),
    #[error("failed to get an RSA key from PKey")]
    PkeyToRsa(#[from] openssl::error::ErrorStack),
}

impl RsaKeyPair {
    /// Generate a new RSA key pair with the given number of bits.
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let inner = openssl::rsa::Rsa::generate(bits).map_err(RsaError::Generate)?;
        Ok(Self { inner })
    }

    /// Import an RSA private key from DER-encoded bytes.
    pub fn from_der(der: &[u8]) -> Result<Self, RsaError> {
        let inner = openssl::rsa::Rsa::private_key_from_der(der).map_err(RsaError::ImportDer)?;
        Ok(Self { inner })
    }

    /// Export the RSA private key to DER-encoded bytes.
    pub fn private_key_to_der(&self) -> Result<Vec<u8>, RsaError> {
        self.inner.private_key_to_der().map_err(RsaError::ExportDer)
    }

    /// The modulus size in bytes.
    pub fn modulus_size(&self) -> usize {
        self.inner.size() as usize
    }

    /// The public exponent as big-endian bytes.
    pub fn exponent(&self) -> Vec<u8> {
        self.inner.e().to_vec()
    }

    /// The modulus as big-endian bytes.
    pub fn modulus(&self) -> Vec<u8> {
        self.inner.n().to_vec()
    }

    /// RSA-OAEP encrypt.
    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaOaepError> {
        rsa_oaep_encrypt(&self.inner, input, hash_algorithm)
    }

    /// RSA-OAEP decrypt.
    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaOaepError> {
        rsa_oaep_decrypt(&self.inner, input, hash_algorithm)
    }

    /// PKCS#11 RSA AES key unwrap.
    ///
    /// Unwraps a key blob that was wrapped with the PKCS#11 CKM_RSA_AES_KEY_WRAP
    /// mechanism. Returns the unwrapped RSA key.
    pub fn pkcs11_rsa_aes_key_unwrap(
        &self,
        wrapped_key_blob: &[u8],
    ) -> Result<RsaKeyPair, Pkcs11RsaAesKeyUnwrapError> {
        let modulus_size = self.modulus_size();

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

        let unwrapped_aes_key =
            rsa_oaep_decrypt(&self.inner, wrapped_aes_key, OaepHashAlgorithm::Sha1)
                .map_err(Pkcs11RsaAesKeyUnwrapError::RsaUnwrap)?;
        let unwrapped_rsa_key = super::aes_key_wrap::unwrap(&unwrapped_aes_key, wrapped_rsa_key)
            .map_err(Pkcs11RsaAesKeyUnwrapError::AesUnwrap)?;
        let unwrapped_rsa_key = openssl::pkey::PKey::private_key_from_pkcs8(&unwrapped_rsa_key)
            .map_err(Pkcs11RsaAesKeyUnwrapError::ConvertPkcs8DerToPkey)?;
        let unwrapped_rsa_key = unwrapped_rsa_key
            .rsa()
            .map_err(Pkcs11RsaAesKeyUnwrapError::PkeyToRsa)?;

        Ok(RsaKeyPair {
            inner: unwrapped_rsa_key,
        })
    }
}

fn rsa_oaep_encrypt(
    rsa: &openssl::rsa::Rsa<openssl::pkey::Private>,
    input: &[u8],
    hash_algorithm: OaepHashAlgorithm,
) -> Result<Vec<u8>, RsaOaepError> {
    let pkey = openssl::pkey::PKey::from_rsa(rsa.to_owned()).map_err(RsaOaepError::RsaToPkey)?;
    let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&pkey).map_err(RsaOaepError::PkeyCtxNew)?;

    ctx.encrypt_init()
        .map_err(RsaOaepError::PkeyCtxEncryptInit)?;
    ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
        .map_err(RsaOaepError::PkeyCtxSetRsaPadding)?;

    match hash_algorithm {
        OaepHashAlgorithm::Sha1 => ctx.set_rsa_oaep_md(openssl::md::Md::sha1()),
        OaepHashAlgorithm::Sha256 => ctx.set_rsa_oaep_md(openssl::md::Md::sha256()),
    }
    .map_err(RsaOaepError::PkeyCtxSetRsaOaepMd)?;

    let mut output = vec![];
    ctx.encrypt_to_vec(input, &mut output)
        .map_err(|e| RsaOaepError::Encrypt(e, hash_algorithm))?;

    Ok(output)
}

fn rsa_oaep_decrypt(
    rsa: &openssl::rsa::Rsa<openssl::pkey::Private>,
    input: &[u8],
    hash_algorithm: OaepHashAlgorithm,
) -> Result<Vec<u8>, RsaOaepError> {
    let pkey = openssl::pkey::PKey::from_rsa(rsa.to_owned()).map_err(RsaOaepError::RsaToPkey)?;
    let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&pkey).map_err(RsaOaepError::PkeyCtxNew)?;

    ctx.decrypt_init()
        .map_err(RsaOaepError::PkeyCtxDecryptInit)?;
    ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
        .map_err(RsaOaepError::PkeyCtxSetRsaPadding)?;

    match hash_algorithm {
        OaepHashAlgorithm::Sha1 => ctx.set_rsa_oaep_md(openssl::md::Md::sha1()),
        OaepHashAlgorithm::Sha256 => ctx.set_rsa_oaep_md(openssl::md::Md::sha256()),
    }
    .map_err(RsaOaepError::PkeyCtxSetRsaOaepMd)?;

    let mut output = vec![];
    ctx.decrypt_to_vec(input, &mut output)
        .map_err(|e| RsaOaepError::Decrypt(e, hash_algorithm))?;

    Ok(output)
}
