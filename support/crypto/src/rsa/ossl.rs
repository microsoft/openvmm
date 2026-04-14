// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::OaepHashAlgorithm;
use super::RsaError;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> RsaError {
    RsaError(crate::BackendError(err, op))
}

pub struct RsaKeyPairInner {
    pub(crate) rsa: openssl::rsa::Rsa<openssl::pkey::Private>,
}

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let rsa = openssl::rsa::Rsa::generate(bits).map_err(|e| err(e, "generating RSA key"))?;
        Ok(Self { rsa })
    }

    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, RsaError> {
        let pkey = openssl::pkey::PKey::private_key_from_pkcs8(der)
            .map_err(|e| err(e, "parsing PKCS#8 DER"))?;
        let rsa = pkey
            .rsa()
            .map_err(|e| err(e, "extracting RSA key from PKey"))?;
        Ok(Self { rsa })
    }

    pub fn modulus_size(&self) -> usize {
        self.rsa.size() as usize
    }

    pub fn modulus(&self) -> Vec<u8> {
        self.rsa.n().to_vec()
    }

    pub fn public_exponent(&self) -> Vec<u8> {
        self.rsa.e().to_vec()
    }

    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let pkey = openssl::pkey::PKey::from_rsa(self.rsa.clone())
            .map_err(|e| err(e, "converting RSA to PKey"))?;
        let mut ctx =
            openssl::pkey_ctx::PkeyCtx::new(&pkey).map_err(|e| err(e, "creating PkeyCtx"))?;

        ctx.encrypt_init().map_err(|e| err(e, "encrypt init"))?;
        ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
            .map_err(|e| err(e, "setting RSA padding"))?;

        match hash_algorithm {
            OaepHashAlgorithm::Sha1 => ctx.set_rsa_oaep_md(openssl::md::Md::sha1()),
            OaepHashAlgorithm::Sha256 => ctx.set_rsa_oaep_md(openssl::md::Md::sha256()),
        }
        .map_err(|e| err(e, "setting OAEP hash"))?;

        let mut output = vec![];
        ctx.encrypt_to_vec(input, &mut output)
            .map_err(|e| err(e, "RSA-OAEP encrypt"))?;

        Ok(output)
    }

    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let pkey = openssl::pkey::PKey::from_rsa(self.rsa.clone())
            .map_err(|e| err(e, "converting RSA to PKey"))?;
        let mut ctx =
            openssl::pkey_ctx::PkeyCtx::new(&pkey).map_err(|e| err(e, "creating PkeyCtx"))?;

        ctx.decrypt_init().map_err(|e| err(e, "decrypt init"))?;
        ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
            .map_err(|e| err(e, "setting RSA padding"))?;

        match hash_algorithm {
            OaepHashAlgorithm::Sha1 => ctx.set_rsa_oaep_md(openssl::md::Md::sha1()),
            OaepHashAlgorithm::Sha256 => ctx.set_rsa_oaep_md(openssl::md::Md::sha256()),
        }
        .map_err(|e| err(e, "setting OAEP hash"))?;

        let mut output = vec![];
        ctx.decrypt_to_vec(input, &mut output)
            .map_err(|e| err(e, "RSA-OAEP decrypt"))?;

        Ok(output)
    }

    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        let pkey = openssl::pkey::PKey::from_rsa(self.rsa.clone())
            .map_err(|e| err(e, "converting RSA to PKey"))?;
        pkey.private_key_to_pkcs8()
            .map_err(|e| err(e, "exporting PKCS#8 DER"))
    }

    pub fn to_private_key_der(&self) -> Result<Vec<u8>, RsaError> {
        self.rsa
            .private_key_to_der()
            .map_err(|e| err(e, "exporting private key DER"))
    }

    pub fn sign_pkcs1_sha256(&self, data: &[u8]) -> Result<Vec<u8>, RsaError> {
        let pkey = openssl::pkey::PKey::from_rsa(self.rsa.clone())
            .map_err(|e| err(e, "converting RSA key to PKey for signing"))?;
        let mut signer = openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &pkey)
            .map_err(|e| err(e, "creating signer"))?;
        signer
            .set_rsa_padding(openssl::rsa::Padding::PKCS1)
            .map_err(|e| err(e, "setting RSA padding"))?;
        signer.update(data).map_err(|e| err(e, "signer update"))?;
        signer.sign_to_vec().map_err(|e| err(e, "signer sign"))
    }
}

pub struct RsaPublicKeyInner {
    pub(crate) pkey: openssl::pkey::PKey<openssl::pkey::Public>,
}

impl RsaPublicKeyInner {
    pub fn from_pkey(pkey: openssl::pkey::PKey<openssl::pkey::Public>) -> Self {
        Self { pkey }
    }

    pub fn verify_pkcs1_sha256(&self, message: &[u8], signature: &[u8]) -> Result<bool, RsaError> {
        let mut verifier =
            openssl::sign::Verifier::new(openssl::hash::MessageDigest::sha256(), &self.pkey)
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
}
