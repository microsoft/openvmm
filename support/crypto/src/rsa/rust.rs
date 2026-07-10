// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA implementation using the `rsa` RustCrypto crate.

#![expect(deprecated)]

use super::RsaError;
use super::RsaPublicKeyComponents;
use crate::HashAlgorithm;
use getrandom::SysRng;
use pkcs8::DecodePrivateKey;
use rsa::Oaep;
use rsa::Pkcs1v15Sign;
use rsa::Pss;
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use rsa::rand_core;
use rsa::rand_core::UnwrapErr;
use rsa::traits::PublicKeyParts;

const fn rng() -> impl rand_core::CryptoRng {
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

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        use pkcs8::EncodePrivateKey;
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
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        match hash_algorithm {
            HashAlgorithm::Sha1 => {
                self.0
                    .decrypt_blinded(&mut rng(), Oaep::<sha1::Sha1>::new(), input)
            }
            HashAlgorithm::Sha256 => {
                self.0
                    .decrypt_blinded(&mut rng(), Oaep::<sha2::Sha256>::new(), input)
            }
            HashAlgorithm::Sha384 => {
                self.0
                    .decrypt_blinded(&mut rng(), Oaep::<sha2::Sha384>::new(), input)
            }
        }
        .map_err(|e| RsaError(e, "OAEP decryption"))
    }

    pub fn pkcs1_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        // rsa's `sign` expects the caller-supplied buffer to already
        // be the hash digest of the message. Other backends take the raw
        // message and hash internally. Do the hash here before handing off.
        let data = hash_algorithm.hash(data);
        match hash_algorithm {
            HashAlgorithm::Sha1 => {
                self.0
                    .sign_with_rng(&mut rng(), Pkcs1v15Sign::new::<sha1::Sha1>(), &data)
            }
            HashAlgorithm::Sha256 => {
                self.0
                    .sign_with_rng(&mut rng(), Pkcs1v15Sign::new::<sha2::Sha256>(), &data)
            }
            HashAlgorithm::Sha384 => {
                self.0
                    .sign_with_rng(&mut rng(), Pkcs1v15Sign::new::<sha2::Sha384>(), &data)
            }
        }
        .map_err(|e| RsaError(e, "PKCS#1 signing"))
    }

    pub fn pss_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        // rsa's PSS signing expects a pre-hashed digest, just like the
        // PKCS#1 v1.5 path above. `Pss::<H>::new()` defaults the salt
        // length to the digest output size, matching the COSE/JWS
        // convention (RFC 8230 section 2).
        let data = hash_algorithm.hash(data);
        match hash_algorithm {
            HashAlgorithm::Sha1 => {
                self.0
                    .sign_with_rng(&mut rng(), Pss::<sha1::Sha1>::new(), &data)
            }
            HashAlgorithm::Sha256 => {
                self.0
                    .sign_with_rng(&mut rng(), Pss::<sha2::Sha256>::new(), &data)
            }
            HashAlgorithm::Sha384 => {
                self.0
                    .sign_with_rng(&mut rng(), Pss::<sha2::Sha384>::new(), &data)
            }
        }
        .map_err(|e| RsaError(e, "PSS signing"))
    }

    pub(crate) fn as_pub(&self) -> &RsaPublicKeyInner {
        // SAFETY: RsaPublicKeyInner is just a wrapper around an RsaPublicKey.
        unsafe { std::mem::transmute::<&RsaPublicKey, &RsaPublicKeyInner>(self.0.as_public_key()) }
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaPublicKeyInner(pub(crate) RsaPublicKey);

impl RsaPublicKeyInner {
    pub fn from_components(n: &[u8], e: &[u8]) -> Result<Self, RsaError> {
        let key = RsaPublicKey::new(
            rsa::BoxedUint::from_be_slice_vartime(n),
            rsa::BoxedUint::from_be_slice_vartime(e),
        )
        .map_err(|e| RsaError(e, "constructing RSA public key from components"))?;
        Ok(Self(key))
    }

    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        match hash_algorithm {
            HashAlgorithm::Sha1 => self.0.encrypt(&mut rng(), Oaep::<sha1::Sha1>::new(), input),
            HashAlgorithm::Sha256 => self
                .0
                .encrypt(&mut rng(), Oaep::<sha2::Sha256>::new(), input),
            HashAlgorithm::Sha384 => self
                .0
                .encrypt(&mut rng(), Oaep::<sha2::Sha384>::new(), input),
        }
        .map_err(|e| RsaError(e, "OAEP encryption"))
    }

    pub fn pkcs1_verify(
        &self,
        data: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        // rsa's `pkcs1_verify` expects the caller-supplied buffer to already
        // be the hash digest of the message. Other backends take the raw
        // message and hash internally. Do the hash here before handing off.
        let data = hash_algorithm.hash(data);
        let result = match hash_algorithm {
            HashAlgorithm::Sha1 => {
                self.0
                    .verify(Pkcs1v15Sign::new::<sha1::Sha1>(), &data, signature)
            }
            HashAlgorithm::Sha256 => {
                self.0
                    .verify(Pkcs1v15Sign::new::<sha2::Sha256>(), &data, signature)
            }
            HashAlgorithm::Sha384 => {
                self.0
                    .verify(Pkcs1v15Sign::new::<sha2::Sha384>(), &data, signature)
            }
        };
        match result {
            Ok(()) => Ok(true),
            Err(rsa::Error::Verification) => Ok(false),
            Err(e) => Err(RsaError(e, "PKCS#1 signature verification")),
        }
    }

    pub fn pss_verify(
        &self,
        data: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        // PSS verification follows the same pre-hash convention as the
        // PKCS#1 v1.5 path; `Pss::<H>::new()` uses a salt length equal
        // to the digest output size (COSE/JWS convention, RFC 8230
        // section 2).
        let data = hash_algorithm.hash(data);
        let result = match hash_algorithm {
            HashAlgorithm::Sha1 => self.0.verify(Pss::<sha1::Sha1>::new(), &data, signature),
            HashAlgorithm::Sha256 => self.0.verify(Pss::<sha2::Sha256>::new(), &data, signature),
            HashAlgorithm::Sha384 => self.0.verify(Pss::<sha2::Sha384>::new(), &data, signature),
        };
        match result {
            Ok(()) => Ok(true),
            Err(rsa::Error::Verification) => Ok(false),
            Err(e) => Err(RsaError(e, "PSS signature verification")),
        }
    }

    pub fn modulus_size(&self) -> usize {
        self.0.size()
    }

    pub fn to_components(&self) -> RsaPublicKeyComponents {
        RsaPublicKeyComponents {
            modulus: self.0.n().to_be_bytes_trimmed_vartime().to_vec(),
            public_exponent: self.0.e().to_be_bytes_trimmed_vartime().to_vec(),
        }
    }
}
