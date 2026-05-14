// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA implementation using SymCrypt.

use super::RsaError;
use symcrypt::rsa::RsaKey;
use symcrypt::rsa::RsaKeyUsage;

fn err(err: symcrypt::errors::SymCryptError, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::SymCrypt(err, op))
}

fn pkcs8_err(err: pkcs8::Error, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::Pkcs8Encoding(err, op))
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaKeyPairInner(pub(crate) RsaKey);

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let rsa = RsaKey::generate_key_pair(bits, None, RsaKeyUsage::SignAndEncrypt)
            .map_err(|e| err(e, "generating RSA key"))?;
        Ok(Self(rsa))
    }

    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, RsaError> {
        use pkcs8::DecodePrivateKey;
        use rsa::traits::PrivateKeyParts;
        use rsa::traits::PublicKeyParts;

        let parsed = rsa::RsaPrivateKey::from_pkcs8_der(der)
            .map_err(|e| pkcs8_err(e, "parsing PKCS#8 DER"))?;
        let primes = parsed.primes();
        if primes.len() != 2 {
            return Err(RsaError(crate::BackendError::Pkcs8Encoding(
                pkcs8::Error::KeyMalformed(pkcs8::KeyError::Invalid),
                "multiprime RSA keys not supported",
            )));
        }
        let rsa = RsaKey::set_key_pair(
            &parsed.n().to_be_bytes_trimmed_vartime(),
            &parsed.e().to_be_bytes_trimmed_vartime(),
            &primes[0].to_be_bytes_trimmed_vartime(),
            &primes[1].to_be_bytes_trimmed_vartime(),
            RsaKeyUsage::SignAndEncrypt,
        )
        .map_err(|e| err(e, "setting RSA key pair"))?;
        Ok(Self(rsa))
    }

    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        use pkcs8::EncodePrivateKey;

        let blob = self
            .0
            .export_key_pair_blob()
            .map_err(|e| err(e, "exporting RSA key blob"))?;
        let rsa = rsa::RsaPrivateKey::from_components(
            rsa::BoxedUint::from_be_slice_vartime(&blob.modulus),
            rsa::BoxedUint::from_be_slice_vartime(&blob.pub_exp),
            rsa::BoxedUint::from_be_slice_vartime(&blob.private_exp),
            vec![
                rsa::BoxedUint::from_be_slice_vartime(&blob.p),
                rsa::BoxedUint::from_be_slice_vartime(&blob.q),
            ],
        )
        .unwrap();
        Ok(rsa
            .to_pkcs8_der()
            .map_err(|e| pkcs8_err(e, "converting to DER"))?
            .as_bytes()
            .to_vec())
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
        // SymCrypt's `pkcs1_sign` expects the caller-supplied buffer to already
        // be the hash digest of the message. Other backends take the raw
        // message and hash internally. Do the hash here before handing off.
        let digest = hash_message(data, hash_algorithm);
        self.0
            .pkcs1_sign(&digest, conv_hash(hash_algorithm))
            .map_err(|e| err(e, "PKCS#1 signing"))
    }

    pub(crate) fn as_pub(&self) -> &RsaPublicKeyInner {
        // SAFETY: RsaPublicKeyInner is just a wrapper around the same RsaKey.
        unsafe { std::mem::transmute::<&RsaKeyPairInner, &RsaPublicKeyInner>(self) }
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaPublicKeyInner(pub(crate) RsaKey);

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
        // SymCrypt's `pkcs1_verify` expects the caller-supplied buffer to already
        // be the hash digest of the message. Other backends take the raw
        // message and hash internally. Do the hash here before handing off.
        let digest = hash_message(data, hash_algorithm);
        match self
            .0
            .pkcs1_verify(&digest, signature, conv_hash(hash_algorithm))
        {
            Ok(()) => Ok(true),
            // `SignatureVerificationFailure` is the expected error for an
            // invalid signature. `InvalidArgument` can also occur when the
            // signature bytes, interpreted as an integer, are >= the modulus
            // or otherwise don't decode to a valid PKCS#1 v1.5 encoding —
            // which happens probabilistically when verifying a signature
            // against the wrong public key. Both indicate "signature does
            // not verify", not a backend bug.
            Err(
                symcrypt::errors::SymCryptError::SignatureVerificationFailure
                | symcrypt::errors::SymCryptError::InvalidArgument,
            ) => Ok(false),
            Err(e) => Err(err(e, "PKCS#1 signature verification")),
        }
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

fn hash_message(data: &[u8], hash_algorithm: super::HashAlgorithm) -> Vec<u8> {
    match hash_algorithm {
        super::HashAlgorithm::Sha1 => symcrypt::hash::sha1(data).to_vec(),
        super::HashAlgorithm::Sha256 => symcrypt::hash::sha256(data).to_vec(),
    }
}
