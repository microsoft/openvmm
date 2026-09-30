// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA implementation using OpenSSL.

use super::RsaError;
use super::RsaPublicKeyComponents;
use crate::HashAlgorithm;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> RsaError {
    RsaError(crate::BackendError(err, op))
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaKeyPairInner(pub(crate) openssl::pkey::PKey<openssl::pkey::Private>);

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let rsa =
            openssl::rsa::Rsa::generate(bits).map_err(|e| err(e, "generating the RSA key"))?;
        let pkey = openssl::pkey::PKey::from_rsa(rsa)
            .map_err(|e| err(e, "wrapping the RSA key in a PKey"))?;
        Ok(Self(pkey))
    }

    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, RsaError> {
        let pkey = openssl::pkey::PKey::private_key_from_pkcs8(der)
            .map_err(|e| err(e, "parsing the PKCS#8 DER private key"))?;
        // Ensure the key is actually an RSA key.
        pkey.rsa()
            .map_err(|e| err(e, "checking that the key is an RSA key"))?;
        Ok(Self(pkey))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        self.0
            .private_key_to_pkcs8()
            .map_err(|e| err(e, "encoding the private key as PKCS#8 DER"))
    }

    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&self.0)
            .map_err(|e| err(e, "creating the decryption context"))?;
        ctx.decrypt_init()
            .map_err(|e| err(e, "initializing the decryption context"))?;
        ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
            .map_err(|e| err(e, "selecting OAEP padding"))?;
        ctx.set_rsa_oaep_md(hash_algorithm.into())
            .map_err(|e| err(e, "setting the OAEP hash algorithm"))?;
        let mut output = vec![];
        ctx.decrypt_to_vec(input, &mut output)
            .map_err(|e| err(e, "decrypting with RSA-OAEP"))?;
        Ok(output)
    }

    pub fn pkcs1_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let mut signer = openssl::sign::Signer::new(hash_algorithm.into(), &self.0)
            .map_err(|e| err(e, "creating the signer"))?;
        signer
            .set_rsa_padding(openssl::rsa::Padding::PKCS1)
            .map_err(|e| err(e, "selecting PKCS#1 v1.5 padding"))?;
        signer
            .update(data)
            .map_err(|e| err(e, "hashing the message to sign"))?;
        signer
            .sign_to_vec()
            .map_err(|e| err(e, "computing the PKCS#1 v1.5 signature"))
    }

    pub fn pss_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let mut signer = openssl::sign::Signer::new(hash_algorithm.into(), &self.0)
            .map_err(|e| err(e, "creating the signer"))?;
        signer
            .set_rsa_padding(openssl::rsa::Padding::PKCS1_PSS)
            .map_err(|e| err(e, "selecting PSS padding"))?;
        signer
            .set_rsa_mgf1_md(hash_algorithm.into())
            .map_err(|e| err(e, "setting the MGF1 hash algorithm"))?;
        signer
            .set_rsa_pss_saltlen(openssl::sign::RsaPssSaltlen::DIGEST_LENGTH)
            .map_err(|e| err(e, "setting the PSS salt length"))?;
        signer
            .update(data)
            .map_err(|e| err(e, "hashing the message to sign"))?;
        signer
            .sign_to_vec()
            .map_err(|e| err(e, "computing the PSS signature"))
    }

    pub(crate) fn as_pub(&self) -> &RsaPublicKeyInner {
        // SAFETY: PKey<Private> can be safely treated as PKey<Public> for read-only operations.
        unsafe { std::mem::transmute::<&RsaKeyPairInner, &RsaPublicKeyInner>(self) }
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaPublicKeyInner(pub(crate) openssl::pkey::PKey<openssl::pkey::Public>);

impl RsaPublicKeyInner {
    pub fn from_components(n: &[u8], e: &[u8]) -> Result<Self, RsaError> {
        let n = openssl::bn::BigNum::from_slice(n).map_err(|e| err(e, "parsing the modulus"))?;
        let e = openssl::bn::BigNum::from_slice(e)
            .map_err(|e| err(e, "parsing the public exponent"))?;
        let rsa = openssl::rsa::Rsa::from_public_components(n, e)
            .map_err(|e| err(e, "constructing the RSA public key from its components"))?;
        let pkey = openssl::pkey::PKey::from_rsa(rsa)
            .map_err(|e| err(e, "wrapping the RSA key in a PKey"))?;
        Ok(Self(pkey))
    }

    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&self.0)
            .map_err(|e| err(e, "creating the encryption context"))?;
        ctx.encrypt_init()
            .map_err(|e| err(e, "initializing the encryption context"))?;
        ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1_OAEP)
            .map_err(|e| err(e, "selecting OAEP padding"))?;
        ctx.set_rsa_oaep_md(hash_algorithm.into())
            .map_err(|e| err(e, "setting the OAEP hash algorithm"))?;
        let mut output = vec![];
        ctx.encrypt_to_vec(input, &mut output)
            .map_err(|e| err(e, "encrypting with RSA-OAEP"))?;
        Ok(output)
    }

    pub fn pkcs1_verify(
        &self,
        message: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        let mut verifier = openssl::sign::Verifier::new(hash_algorithm.into(), &self.0)
            .map_err(|e| err(e, "creating the verifier"))?;
        verifier
            .set_rsa_padding(openssl::rsa::Padding::PKCS1)
            .map_err(|e| err(e, "selecting PKCS#1 v1.5 padding"))?;
        verifier
            .update(message)
            .map_err(|e| err(e, "hashing the signed message"))?;
        verifier
            .verify(signature)
            .map_err(|e| err(e, "verifying the PKCS#1 v1.5 signature"))
    }

    pub fn pss_verify(
        &self,
        message: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        let mut verifier = openssl::sign::Verifier::new(hash_algorithm.into(), &self.0)
            .map_err(|e| err(e, "creating the verifier"))?;
        verifier
            .set_rsa_padding(openssl::rsa::Padding::PKCS1_PSS)
            .map_err(|e| err(e, "selecting PSS padding"))?;
        verifier
            .set_rsa_mgf1_md(hash_algorithm.into())
            .map_err(|e| err(e, "setting the MGF1 hash algorithm"))?;
        verifier
            .set_rsa_pss_saltlen(openssl::sign::RsaPssSaltlen::DIGEST_LENGTH)
            .map_err(|e| err(e, "setting the PSS salt length"))?;
        verifier
            .update(message)
            .map_err(|e| err(e, "hashing the signed message"))?;
        verifier
            .verify(signature)
            .map_err(|e| err(e, "verifying the PSS signature"))
    }

    pub fn modulus_size(&self) -> usize {
        // TODO: This should use EVP_PKEY_get_params but the openssl crate doesn't expose it
        self.0.rsa().unwrap().size() as usize
    }

    pub fn to_components(&self) -> RsaPublicKeyComponents {
        // TODO: This should use EVP_PKEY_get_params but the openssl crate doesn't expose it
        let rsa = self.0.rsa().unwrap();
        RsaPublicKeyComponents {
            modulus: rsa.n().to_vec(),
            public_exponent: rsa.e().to_vec(),
        }
    }
}
