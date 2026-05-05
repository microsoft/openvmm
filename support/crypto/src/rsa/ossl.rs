// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::HashAlgorithm;
use super::RsaError;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> RsaError {
    RsaError(crate::BackendError(err, op))
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaKeyPairInner(pub(crate) openssl::pkey::PKey<openssl::pkey::Private>);

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let rsa = openssl::rsa::Rsa::generate(bits).map_err(|e| err(e, "generating RSA key"))?;
        let pkey =
            openssl::pkey::PKey::from_rsa(rsa).map_err(|e| err(e, "converting RSA to PKey"))?;
        Ok(Self(pkey))
    }

    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, RsaError> {
        let pkey = openssl::pkey::PKey::private_key_from_pkcs8(der)
            .map_err(|e| err(e, "parsing PKCS#8 DER"))?;
        // Ensure the key is actually an RSA key.
        pkey.rsa().map_err(|e| err(e, "checking key is RSA"))?;
        Ok(Self(pkey))
    }

    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        self.0
            .private_key_to_pkcs8()
            .map_err(|e| err(e, "exporting PKCS#8 DER"))
    }

    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let mut ctx =
            openssl::pkey_ctx::PkeyCtx::new(&self.0).map_err(|e| err(e, "creating PkeyCtx"))?;
        ctx.decrypt_init().map_err(|e| err(e, "decrypt init"))?;
        ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
            .map_err(|e| err(e, "setting RSA padding"))?;
        ctx.set_rsa_oaep_md(conv_mdref(hash_algorithm))
            .map_err(|e| err(e, "setting OAEP hash"))?;
        let mut output = vec![];
        ctx.decrypt_to_vec(input, &mut output)
            .map_err(|e| err(e, "RSA-OAEP decrypt"))?;
        Ok(output)
    }

    pub fn pkcs1_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let mut signer = openssl::sign::Signer::new(conv_md(hash_algorithm), &self.0)
            .map_err(|e| err(e, "creating signer"))?;
        signer
            .set_rsa_padding(openssl::rsa::Padding::PKCS1)
            .map_err(|e| err(e, "setting RSA padding"))?;
        signer.update(data).map_err(|e| err(e, "signer update"))?;
        signer.sign_to_vec().map_err(|e| err(e, "signer sign"))
    }

    pub(crate) fn as_pub(&self) -> &RsaPublicKeyInner {
        // SAFETY: PKey<Private> can be safely treated as PKey<Public> for read-only operations.
        unsafe { std::mem::transmute::<&RsaKeyPairInner, &RsaPublicKeyInner>(self) }
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaPublicKeyInner(pub(crate) openssl::pkey::PKey<openssl::pkey::Public>);

impl RsaPublicKeyInner {
    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let mut ctx =
            openssl::pkey_ctx::PkeyCtx::new(&self.0).map_err(|e| err(e, "creating PkeyCtx"))?;
        ctx.encrypt_init().map_err(|e| err(e, "encrypt init"))?;
        ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
            .map_err(|e| err(e, "setting RSA padding"))?;
        ctx.set_rsa_oaep_md(conv_mdref(hash_algorithm))
            .map_err(|e| err(e, "setting OAEP hash"))?;
        let mut output = vec![];
        ctx.encrypt_to_vec(input, &mut output)
            .map_err(|e| err(e, "RSA-OAEP encrypt"))?;
        Ok(output)
    }

    pub fn pkcs1_verify(
        &self,
        message: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        let mut verifier = openssl::sign::Verifier::new(conv_md(hash_algorithm), &self.0)
            .map_err(|e| err(e, "creating verifier"))?;
        verifier
            .set_rsa_padding(openssl::rsa::Padding::PKCS1)
            .map_err(|e| err(e, "setting RSA padding"))?;
        verifier
            .update(message)
            .map_err(|e| err(e, "verifier update"))?;
        verifier
            .verify(signature)
            .map_err(|e| err(e, "verifier verify"))
    }

    pub fn modulus_size(&self) -> usize {
        // TODO: This should use EVP_PKEY_get_params but the openssl crate doesn't expose it
        self.0.rsa().unwrap().size() as usize
    }

    pub fn modulus(&self) -> Vec<u8> {
        // TODO: This should use EVP_PKEY_get_params but the openssl crate doesn't expose it
        self.0.rsa().unwrap().n().to_vec()
    }

    pub fn public_exponent(&self) -> Vec<u8> {
        // TODO: This should use EVP_PKEY_get_params but the openssl crate doesn't expose it
        self.0.rsa().unwrap().e().to_vec()
    }
}

fn conv_md(hash_algorithm: HashAlgorithm) -> openssl::hash::MessageDigest {
    match hash_algorithm {
        HashAlgorithm::Sha1 => openssl::hash::MessageDigest::sha1(),
        HashAlgorithm::Sha256 => openssl::hash::MessageDigest::sha256(),
    }
}

fn conv_mdref(hash_algorithm: HashAlgorithm) -> &'static openssl::md::MdRef {
    match hash_algorithm {
        HashAlgorithm::Sha1 => openssl::md::Md::sha1(),
        HashAlgorithm::Sha256 => openssl::md::Md::sha256(),
    }
}
