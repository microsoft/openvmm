// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub(crate) struct RsaKeyPairInner {
    rsa: openssl::rsa::Rsa<openssl::pkey::Private>,
}

pub fn generate(bits: u32) -> Result<RsaKeyPairInner, RsaError> {
    let rsa = openssl::rsa::Rsa::generate(bits)
        .map_err(|e| RsaError(crate::BackendError(e, "generating RSA key pair")))?;
    Ok(RsaKeyPairInner { rsa })
}

pub fn from_der(der: &[u8]) -> Result<RsaKeyPairInner, RsaError> {
    let rsa = openssl::rsa::Rsa::private_key_from_der(der)
        .map_err(|e| RsaError(crate::BackendError(e, "importing RSA private key")))?;
    Ok(RsaKeyPairInner { rsa })
}

impl RsaKeyPairInner {
    pub fn private_key_to_der(&self) -> Result<Vec<u8>, RsaError> {
        self.rsa
            .private_key_to_der()
            .map_err(|e| RsaError(crate::BackendError(e, "exporting RSA private key")))
    }

    pub fn modulus_size(&self) -> usize {
        self.rsa.size() as usize
    }

    pub fn exponent(&self) -> Vec<u8> {
        self.rsa.e().to_vec()
    }

    pub fn modulus(&self) -> Vec<u8> {
        self.rsa.n().to_vec()
    }

    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaOaepError> {
        rsa_oaep_encrypt(&self.rsa, input, hash_algorithm)
    }

    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaOaepError> {
        rsa_oaep_decrypt(&self.rsa, input, hash_algorithm)
    }

    pub fn pkcs11_rsa_aes_key_unwrap(
        &self,
        wrapped_key_blob: &[u8],
    ) -> Result<RsaKeyPairInner, Pkcs11RsaAesKeyUnwrapError> {
        let modulus_size = self.modulus_size();

        let (wrapped_aes_key, wrapped_rsa_key) = wrapped_key_blob
            .split_at_checked(modulus_size)
            .ok_or_else(|| {
                Pkcs11RsaAesKeyUnwrapError(crate::BackendError(
                    openssl::error::ErrorStack::get(),
                    "undersized wrapped AES key blob",
                ))
            })?;

        if wrapped_rsa_key.is_empty() {
            return Err(Pkcs11RsaAesKeyUnwrapError(crate::BackendError(
                openssl::error::ErrorStack::get(),
                "empty wrapped RSA key",
            )));
        }

        let unwrapped_aes_key =
            rsa_oaep_decrypt(&self.rsa, wrapped_aes_key, OaepHashAlgorithm::Sha1)
                .map_err(|e| Pkcs11RsaAesKeyUnwrapError(e.0))?;
        let unwrapped_rsa_key = crate::aes_key_wrap::unwrap(&unwrapped_aes_key, wrapped_rsa_key)
            .map_err(|_| {
                Pkcs11RsaAesKeyUnwrapError(crate::BackendError(
                    openssl::error::ErrorStack::get(),
                    "AES key unwrap",
                ))
            })?;
        let unwrapped_rsa_key = openssl::pkey::PKey::private_key_from_pkcs8(&unwrapped_rsa_key)
            .map_err(|e| {
                Pkcs11RsaAesKeyUnwrapError(crate::BackendError(e, "converting PKCS#8 to PKey"))
            })?;
        let unwrapped_rsa_key = unwrapped_rsa_key.rsa().map_err(|e| {
            Pkcs11RsaAesKeyUnwrapError(crate::BackendError(e, "extracting RSA from PKey"))
        })?;

        Ok(RsaKeyPairInner {
            rsa: unwrapped_rsa_key,
        })
    }
}

fn rsa_oaep_encrypt(
    rsa: &openssl::rsa::Rsa<openssl::pkey::Private>,
    input: &[u8],
    hash_algorithm: OaepHashAlgorithm,
) -> Result<Vec<u8>, RsaOaepError> {
    let pkey = openssl::pkey::PKey::from_rsa(rsa.to_owned())
        .map_err(|e| RsaOaepError(crate::BackendError(e, "converting RSA to PKey")))?;
    let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&pkey)
        .map_err(|e| RsaOaepError(crate::BackendError(e, "creating PKey context")))?;

    ctx.encrypt_init()
        .map_err(|e| RsaOaepError(crate::BackendError(e, "encrypt init")))?;
    ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
        .map_err(|e| RsaOaepError(crate::BackendError(e, "setting RSA padding")))?;

    match hash_algorithm {
        OaepHashAlgorithm::Sha1 => ctx.set_rsa_oaep_md(openssl::md::Md::sha1()),
        OaepHashAlgorithm::Sha256 => ctx.set_rsa_oaep_md(openssl::md::Md::sha256()),
    }
    .map_err(|e| RsaOaepError(crate::BackendError(e, "setting OAEP hash")))?;

    let mut output = vec![];
    ctx.encrypt_to_vec(input, &mut output)
        .map_err(|e| RsaOaepError(crate::BackendError(e, "encryption")))?;

    Ok(output)
}

fn rsa_oaep_decrypt(
    rsa: &openssl::rsa::Rsa<openssl::pkey::Private>,
    input: &[u8],
    hash_algorithm: OaepHashAlgorithm,
) -> Result<Vec<u8>, RsaOaepError> {
    let pkey = openssl::pkey::PKey::from_rsa(rsa.to_owned())
        .map_err(|e| RsaOaepError(crate::BackendError(e, "converting RSA to PKey")))?;
    let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&pkey)
        .map_err(|e| RsaOaepError(crate::BackendError(e, "creating PKey context")))?;

    ctx.decrypt_init()
        .map_err(|e| RsaOaepError(crate::BackendError(e, "decrypt init")))?;
    ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
        .map_err(|e| RsaOaepError(crate::BackendError(e, "setting RSA padding")))?;

    match hash_algorithm {
        OaepHashAlgorithm::Sha1 => ctx.set_rsa_oaep_md(openssl::md::Md::sha1()),
        OaepHashAlgorithm::Sha256 => ctx.set_rsa_oaep_md(openssl::md::Md::sha256()),
    }
    .map_err(|e| RsaOaepError(crate::BackendError(e, "setting OAEP hash")))?;

    let mut output = vec![];
    ctx.decrypt_to_vec(input, &mut output)
        .map_err(|e| RsaOaepError(crate::BackendError(e, "decryption")))?;

    Ok(output)
}
