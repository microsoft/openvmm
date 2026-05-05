// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::RsaError;
use pkcs1::der::Decode;
use pkcs1::der::Encode;
use symcrypt::rsa::RsaKey;
use symcrypt::rsa::RsaKeyUsage;

fn err(err: symcrypt::errors::SymCryptError, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::SymCryptError(err, op))
}

fn der_err(err: der::Error, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::EncodingError(err, op))
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaKeyPairInner(RsaKey);

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let rsa = RsaKey::generate_key_pair(bits, None, RsaKeyUsage::SignAndEncrypt)
            .map_err(|e| err(e, "generating RSA key"))?;
        Ok(Self(rsa))
    }

    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, RsaError> {
        let parsed =
            pkcs1::RsaPrivateKey::from_der(der).map_err(|e| der_err(e, "parsing RSA key"))?;
        let rsa = RsaKey::set_key_pair(
            parsed.modulus.as_bytes(),
            parsed.public_exponent.as_bytes(),
            parsed.prime1.as_bytes(),
            parsed.prime2.as_bytes(),
            RsaKeyUsage::SignAndEncrypt,
        )
        .map_err(|e| err(e, "setting RSA key pair"))?;
        Ok(Self(rsa))
    }

    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        let blob = self
            .0
            .export_key_pair_blob()
            .map_err(|e| err(e, "exporting RSA key blob"))?;
        let pkcs1 = pkcs1::RsaPrivateKey {
            modulus: pkcs1::UintRef::new(&blob.modulus)
                .map_err(|e| der_err(e, "converting modulus"))?,
            public_exponent: pkcs1::UintRef::new(&blob.pub_exp)
                .map_err(|e| der_err(e, "converting public exponent"))?,
            private_exponent: pkcs1::UintRef::new(&blob.private_exp)
                .map_err(|e| der_err(e, "converting private exponent"))?,
            prime1: pkcs1::UintRef::new(&blob.p).map_err(|e| der_err(e, "converting prime1"))?,
            prime2: pkcs1::UintRef::new(&blob.q).map_err(|e| der_err(e, "converting prime2"))?,
            exponent1: pkcs1::UintRef::new(&blob.d_p)
                .map_err(|e| der_err(e, "converting exponent1"))?,
            exponent2: pkcs1::UintRef::new(&blob.d_q)
                .map_err(|e| der_err(e, "converting exponent2"))?,
            coefficient: pkcs1::UintRef::new(&blob.crt_coefficient)
                .map_err(|e| der_err(e, "converting coefficient"))?,
            other_prime_infos: None,
        };
        pkcs1.to_der().map_err(|e| der_err(e, "encoding RSA key"))
    }

    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0
            .oaep_decrypt(input, conv_hash(hash_algorithm), &[])
            .map_err(|e| err(e, "OAEP decryption"))
    }

    pub fn pkcs1_sign(
        &self,
        data: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0
            .pkcs1_sign(data, conv_hash(hash_algorithm))
            .map_err(|e| err(e, "PKCS#1 signing"))
    }

    pub(crate) fn as_pub(&self) -> &RsaPublicKeyInner {
        // SAFETY: RsaPublicKeyInner is just a wrapper around the same RsaKey.
        unsafe { std::mem::transmute::<&RsaKeyPairInner, &RsaPublicKeyInner>(self) }
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaPublicKeyInner(RsaKey);

impl RsaPublicKeyInner {
    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0
            .oaep_encrypt(input, conv_hash(hash_algorithm), &[])
            .map_err(|e| err(e, "OAEP encryption"))
    }

    pub fn pkcs1_verify(
        &self,
        data: &[u8],
        signature: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<bool, RsaError> {
        self.0
            .pkcs1_verify(data, signature, conv_hash(hash_algorithm))
            .map_err(|e| err(e, "PKCS#1 signature verification"))?;
        Ok(true)
    }

    pub fn modulus_size(&self) -> usize {
        self.0.get_size_of_modulus() as usize
    }

    pub fn modulus(&self) -> Vec<u8> {
        // TODO: Maybe cache the pub blob?
        self.0.export_public_key_blob().unwrap().modulus
    }

    pub fn public_exponent(&self) -> Vec<u8> {
        // TODO: Maybe cache the pub blob?
        self.0.export_public_key_blob().unwrap().pub_exp
    }
}

fn conv_hash(hash_algorithm: super::HashAlgorithm) -> symcrypt::hash::HashAlgorithm {
    match hash_algorithm {
        super::HashAlgorithm::Sha1 => symcrypt::hash::HashAlgorithm::Sha1,
        super::HashAlgorithm::Sha256 => symcrypt::hash::HashAlgorithm::Sha256,
    }
}
