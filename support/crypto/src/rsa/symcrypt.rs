// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::OaepHashAlgorithm;
use super::RsaError;
use ::symcrypt::errors::SymCryptError;
use ::symcrypt::hash::HashAlgorithm;
use ::symcrypt::hash::sha256;
use ::symcrypt::rsa::RsaKey;
use ::symcrypt::rsa::RsaKeyUsage;
use std::sync::Arc;

fn err(e: SymCryptError, op: &'static str) -> RsaError {
    RsaError(crate::BackendError(e, op))
}

fn oaep_hash(h: OaepHashAlgorithm) -> HashAlgorithm {
    match h {
        OaepHashAlgorithm::Sha1 => HashAlgorithm::Sha1,
        OaepHashAlgorithm::Sha256 => HashAlgorithm::Sha256,
    }
}

pub struct RsaKeyPairInner {
    pub(crate) key: Arc<RsaKey>,
}

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let key = RsaKey::generate_key_pair(bits, None, RsaKeyUsage::SignAndEncrypt)
            .map_err(|e| err(e, "generating RSA key"))?;
        Ok(Self { key: Arc::new(key) })
    }

    pub fn modulus_size(&self) -> usize {
        self.key.get_size_of_modulus() as usize
    }

    pub fn modulus(&self) -> Vec<u8> {
        self.key
            .export_key_pair_blob()
            .map(|b| b.modulus)
            .unwrap_or_default()
    }

    pub fn public_exponent(&self) -> Vec<u8> {
        self.key
            .export_key_pair_blob()
            .map(|b| b.pub_exp)
            .unwrap_or_default()
    }

    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.key
            .oaep_encrypt(input, oaep_hash(hash_algorithm), &[])
            .map_err(|e| err(e, "RSA-OAEP encrypt"))
    }

    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: OaepHashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.key
            .oaep_decrypt(input, oaep_hash(hash_algorithm), &[])
            .map_err(|e| err(e, "RSA-OAEP decrypt"))
    }

    pub fn sign_pkcs1_sha256(&self, data: &[u8]) -> Result<Vec<u8>, RsaError> {
        let digest = sha256(data);
        self.key
            .pkcs1_sign(&digest, HashAlgorithm::Sha256)
            .map_err(|e| err(e, "RSA PKCS#1 sign"))
    }
}

pub struct RsaPublicKeyInner {
    pub(crate) key: Arc<RsaKey>,
}

impl RsaPublicKeyInner {
    pub fn verify_pkcs1_sha256(&self, message: &[u8], signature: &[u8]) -> Result<bool, RsaError> {
        let digest = sha256(message);
        match self
            .key
            .pkcs1_verify(&digest, signature, HashAlgorithm::Sha256)
        {
            Ok(()) => Ok(true),
            Err(SymCryptError::SignatureVerificationFailure) => Ok(false),
            Err(e) => Err(err(e, "RSA PKCS#1 verify")),
        }
    }
}
