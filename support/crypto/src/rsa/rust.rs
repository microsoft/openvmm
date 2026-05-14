// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA implementation using the `rsa` RustCrypto crate.

use super::RsaError;
use getrandom::SysRng;
use pkcs8::DecodePrivateKey;
use pkcs8::EncodePrivateKey;
use rsa::Oaep;
use rsa::Pkcs1v15Sign;
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use rsa::rand_core;
use rsa::rand_core::UnwrapErr;
use rsa::traits::PublicKeyParts;
use sha2::Digest;

fn rng() -> impl rand_core::CryptoRng {
    UnwrapErr(SysRng)
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaKeyPairInner(pub(crate) RsaPrivateKey);

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let rsa = RsaPrivateKey::new(&mut rng(), bits as usize)
            .map_err(|e| RsaError(e, "generating RSA key"))?;
        Ok(Self(rsa))
    }

    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, RsaError> {
        let parsed = RsaPrivateKey::from_pkcs8_der(der)
            .map_err(|e| RsaError(e.into(), "parsing PKCS#8 DER"))?;
        Ok(Self(parsed))
    }

    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        Ok(self
            .0
            .to_pkcs8_der()
            .map_err(|e| RsaError(e.into(), "converting to DER"))?
            .as_bytes()
            .to_vec())
    }

    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        match hash_algorithm {
            super::HashAlgorithm::Sha1 => {
                self.0
                    .decrypt_blinded(&mut rng(), Oaep::<sha1::Sha1>::new(), input)
            }
            super::HashAlgorithm::Sha256 => {
                self.0
                    .decrypt_blinded(&mut rng(), Oaep::<sha2::Sha256>::new(), input)
            }
        }
        .map_err(|e| RsaError(e, "OAEP decryption"))
    }

    pub fn pkcs1_sign(
        &self,
        data: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        // rsa's `sign` expects the caller-supplied buffer to already
        // be the hash digest of the message. Other backends take the raw
        // message and hash internally. Do the hash here before handing off.
        match hash_algorithm {
            super::HashAlgorithm::Sha1 => {
                let data = sha1::Sha1::digest(data);
                self.0
                    .sign_with_rng(&mut rng(), Pkcs1v15Sign::new::<sha1::Sha1>(), &data)
            }
            super::HashAlgorithm::Sha256 => {
                let data = sha2::Sha256::digest(data);
                self.0
                    .sign_with_rng(&mut rng(), Pkcs1v15Sign::new::<sha2::Sha256>(), &data)
            }
        }
        .map_err(|e| RsaError(e, "PKCS#1 signing"))
    }

    pub(crate) fn as_pub(&self) -> &RsaPublicKeyInner {
        // SAFETY: RsaPublicKeyInner is just a wrapper around an RsaPublicKey.
        unsafe { std::mem::transmute::<&RsaPublicKey, &RsaPublicKeyInner>(self.0.as_public_key()) }
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaPublicKeyInner(pub(crate) RsaPublicKey);

impl RsaPublicKeyInner {
    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        match hash_algorithm {
            super::HashAlgorithm::Sha1 => {
                self.0.encrypt(&mut rng(), Oaep::<sha1::Sha1>::new(), input)
            }
            super::HashAlgorithm::Sha256 => {
                self.0
                    .encrypt(&mut rng(), Oaep::<sha2::Sha256>::new(), input)
            }
        }
        .map_err(|e| RsaError(e, "OAEP encryption"))
    }

    pub fn pkcs1_verify(
        &self,
        data: &[u8],
        signature: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<bool, RsaError> {
        // rsa's `pkcs1_verify` expects the caller-supplied buffer to already
        // be the hash digest of the message. Other backends take the raw
        // message and hash internally. Do the hash here before handing off.
        let result = match hash_algorithm {
            super::HashAlgorithm::Sha1 => {
                let data = sha1::Sha1::digest(data);
                self.0
                    .verify(Pkcs1v15Sign::new::<sha1::Sha1>(), &data, signature)
            }
            super::HashAlgorithm::Sha256 => {
                let data = sha2::Sha256::digest(data);
                self.0
                    .verify(Pkcs1v15Sign::new::<sha2::Sha256>(), &data, signature)
            }
        };
        match result {
            Ok(()) => Ok(true),
            Err(rsa::Error::Verification) => Ok(false),
            Err(e) => Err(RsaError(e, "PKCS#1 signature verification")),
        }
    }

    pub fn modulus_size(&self) -> usize {
        self.0.size()
    }

    pub fn modulus(&self) -> Vec<u8> {
        self.0.n().to_be_bytes_trimmed_vartime().to_vec()
    }

    pub fn public_exponent(&self) -> Vec<u8> {
        self.0.e().to_be_bytes_trimmed_vartime().to_vec()
    }
}
