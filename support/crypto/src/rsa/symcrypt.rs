// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA implementation using SymCrypt.

use super::RsaError;
use super::RsaPublicKeyComponents;
use der::Decode;
use symcrypt::rsa::RsaKey;
use symcrypt::rsa::RsaKeyUsage;

fn err(err: symcrypt::errors::SymCryptError, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::SymCrypt(err, op))
}

fn der_err(e: der::Error, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::Der(e, op))
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaKeyPairInner(pub(crate) RsaKey);

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let rsa = RsaKey::generate_key_pair(bits, None, RsaKeyUsage::SignAndEncrypt)
            .map_err(|e| err(e, "generating RSA key"))?;
        Ok(Self(rsa))
    }

    pub fn from_pkcs8_der(der_bytes: &[u8]) -> Result<Self, RsaError> {
        let pki = pkcs8::PrivateKeyInfoRef::from_der(der_bytes)
            .map_err(|e| der_err(e, "parsing PKCS#8 DER"))?;
        if pki.algorithm.oid != pkcs1::ALGORITHM_OID {
            return Err(RsaError(crate::BackendError::Pkcs8Encoding(
                pkcs8::Error::KeyMalformed(pkcs8::KeyError::Invalid),
                "PKCS#8 algorithm is not rsaEncryption",
            )));
        }
        let key = pkcs1::RsaPrivateKey::from_der(pki.private_key.as_bytes())
            .map_err(|e| der_err(e, "parsing PKCS#1 RSA private key"))?;
        if key.other_prime_infos.is_some() {
            return Err(RsaError(crate::BackendError::Pkcs8Encoding(
                pkcs8::Error::KeyMalformed(pkcs8::KeyError::Invalid),
                "multiprime RSA keys not supported",
            )));
        }
        let rsa = RsaKey::set_key_pair(
            key.modulus.as_bytes(),
            key.public_exponent.as_bytes(),
            key.prime1.as_bytes(),
            key.prime2.as_bytes(),
            RsaKeyUsage::SignAndEncrypt,
        )
        .map_err(|e| err(e, "setting RSA key pair"))?;
        Ok(Self(rsa))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        use der::Encode;
        use der::asn1::OctetString;
        use der::asn1::UintRef;
        use x509_cert::spki::AlgorithmIdentifierOwned;

        let blob = self
            .0
            .export_key_pair_blob()
            .map_err(|e| err(e, "exporting RSA key blob"))?;

        let pkcs1_key = pkcs1::RsaPrivateKey {
            modulus: UintRef::new(&blob.modulus).map_err(|e| der_err(e, "encoding modulus"))?,
            public_exponent: UintRef::new(&blob.pub_exp)
                .map_err(|e| der_err(e, "encoding public exponent"))?,
            private_exponent: UintRef::new(&blob.private_exp)
                .map_err(|e| der_err(e, "encoding private exponent"))?,
            prime1: UintRef::new(&blob.p).map_err(|e| der_err(e, "encoding prime1"))?,
            prime2: UintRef::new(&blob.q).map_err(|e| der_err(e, "encoding prime2"))?,
            exponent1: UintRef::new(&blob.d_p).map_err(|e| der_err(e, "encoding exponent1"))?,
            exponent2: UintRef::new(&blob.d_q).map_err(|e| der_err(e, "encoding exponent2"))?,
            coefficient: UintRef::new(&blob.crt_coefficient)
                .map_err(|e| der_err(e, "encoding coefficient"))?,
            other_prime_infos: None,
        };
        let pkcs1_der = pkcs1_key
            .to_der()
            .map_err(|e| der_err(e, "encoding PKCS#1 RSA private key"))?;

        let pki = pkcs8::PrivateKeyInfoOwned {
            algorithm: AlgorithmIdentifierOwned {
                oid: pkcs1::ALGORITHM_OID,
                parameters: Some(der::Any::null()),
            },
            private_key: OctetString::new(pkcs1_der)
                .map_err(|e| der_err(e, "wrapping PKCS#1 in OCTET STRING"))?,
            public_key: None,
        };
        pki.to_der()
            .map_err(|e| der_err(e, "encoding PKCS#8 PrivateKeyInfo"))
    }

    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0
            .oaep_decrypt(input, hash_algorithm.into(), &[])
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
        let digest = hash_algorithm.hash(data);
        self.0
            .pkcs1_sign(&digest, hash_algorithm.into())
            .map_err(|e| err(e, "PKCS#1 signing"))
    }

    pub fn pss_sign(
        &self,
        data: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        // SymCrypt's PSS signing expects a pre-hashed digest. Use a salt
        // length equal to the hash output size, matching the COSE/JWS
        // convention (RFC 8230 section 2).
        let digest = hash_algorithm.hash(data);
        let salt_length = digest.len();
        self.0
            .pss_sign(&digest, hash_algorithm.into(), salt_length)
            .map_err(|e| err(e, "PSS signing"))
    }

    pub(crate) fn as_pub(&self) -> &RsaPublicKeyInner {
        // SAFETY: RsaPublicKeyInner is just a wrapper around the same RsaKey.
        unsafe { std::mem::transmute::<&RsaKeyPairInner, &RsaPublicKeyInner>(self) }
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct RsaPublicKeyInner(pub(crate) RsaKey);

impl RsaPublicKeyInner {
    pub fn from_components(n: &[u8], e: &[u8]) -> Result<Self, RsaError> {
        let key = RsaKey::set_public_key(n, e, RsaKeyUsage::SignAndEncrypt)
            .map_err(|e| err(e, "constructing RSA public key from components"))?;
        Ok(Self(key))
    }

    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        self.0
            .oaep_encrypt(input, hash_algorithm.into(), &[])
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
        let digest = hash_algorithm.hash(data);
        match self
            .0
            .pkcs1_verify(&digest, signature, hash_algorithm.into())
        {
            Ok(()) => Ok(true),
            // `SignatureVerificationFailure` is the expected error for an
            // invalid signature. `InvalidArgument` can also occur when the
            // signature bytes, interpreted as an integer, are >= the modulus
            // or otherwise don't decode to a valid PKCS#1 v1.5 encoding,
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

    pub fn pss_verify(
        &self,
        data: &[u8],
        signature: &[u8],
        hash_algorithm: super::HashAlgorithm,
    ) -> Result<bool, RsaError> {
        // SymCrypt's `pss_verify` expects the caller-supplied buffer to
        // already be the hash digest of the message, and a salt length
        // equal to the hash output size (COSE/JWS convention, RFC 8230
        // section 2).
        let digest = hash_algorithm.hash(data);
        let salt_length = digest.len();
        match self
            .0
            .pss_verify(&digest, signature, hash_algorithm.into(), salt_length)
        {
            Ok(()) => Ok(true),
            Err(
                symcrypt::errors::SymCryptError::SignatureVerificationFailure
                | symcrypt::errors::SymCryptError::InvalidArgument,
            ) => Ok(false),
            Err(e) => Err(err(e, "PSS signature verification")),
        }
    }

    pub fn modulus_size(&self) -> usize {
        self.0.get_size_of_modulus() as usize
    }

    pub fn to_components(&self) -> RsaPublicKeyComponents {
        let blob = self.0.export_public_key_blob().unwrap();
        RsaPublicKeyComponents {
            modulus: blob.modulus,
            public_exponent: blob.pub_exp,
        }
    }
}
