// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using SymCrypt.

use super::EcdsaCurve;
use super::EcdsaError;
use super::EcdsaPublicKeyComponents;
use super::EcdsaSignatureComponents;
use der::Decode;
use der::Encode;
use der::Reader;
use der::asn1::UintRef;
use symcrypt::ecc::CurveType;
use symcrypt::ecc::EcKey;
use symcrypt::ecc::EcKeyUsage;
use symcrypt::errors::SymCryptError;

fn err(err: SymCryptError, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError::SymCrypt(err, op))
}

fn der_err(err: der::Error, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError::Der(err, op))
}

/// An error for input that decoded but is not valid or supported.
fn invalid_err(reason: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError::Invalid(reason))
}

/// SymCrypt's fixed-width, big-endian `r || s` representation.
pub struct EcdsaSignatureInner(Vec<u8>);

impl EcdsaSignatureInner {
    pub fn from_der(curve: EcdsaCurve, der: &[u8]) -> Result<Self, EcdsaError> {
        // SymCrypt accepts raw scalars, so decode the ASN.1 using RustCrypto.
        let (r, s) = (|| {
            let mut reader = der::SliceReader::new(der)?;
            let components = reader.sequence(|reader| {
                Ok::<_, der::Error>((
                    reader.decode::<UintRef<'_>>()?,
                    reader.decode::<UintRef<'_>>()?,
                ))
            })?;
            reader.finish()?;
            Ok::<_, der::Error>(components)
        })()
        .map_err(|e| der_err(e, "parsing the DER-encoded ECDSA signature"))?;

        let size = curve.key_size();
        let mut signature = vec![0; size * 2];
        for (component, output) in [r, s].iter().zip(signature.chunks_exact_mut(size)) {
            let bytes = component.as_bytes();
            if bytes.len() > size {
                return Err(invalid_err(
                    "ECDSA signature component exceeds the curve's key size",
                ));
            }
            output[size - bytes.len()..].copy_from_slice(bytes);
        }
        Ok(Self(signature))
    }

    pub fn from_components(
        curve: EcdsaCurve,
        components: &EcdsaSignatureComponents,
    ) -> Result<Self, EcdsaError> {
        if components.r.len() != curve.key_size() || components.s.len() != curve.key_size() {
            return Err(invalid_err("unexpected ECDSA signature component lengths"));
        }
        Ok(Self(
            [components.r.as_slice(), components.s.as_slice()].concat(),
        ))
    }

    pub fn to_components(&self) -> Result<EcdsaSignatureComponents, EcdsaError> {
        let (r, s) = self.0.split_at(self.0.len() / 2);
        Ok(EcdsaSignatureComponents {
            r: r.to_vec(),
            s: s.to_vec(),
        })
    }

    pub fn to_der(&self) -> Result<Vec<u8>, EcdsaError> {
        let (r, s) = self.0.split_at(self.0.len() / 2);
        let r = UintRef::new(r).map_err(|e| der_err(e, "encoding the signature r component"))?;
        let s = UintRef::new(s).map_err(|e| der_err(e, "encoding the signature s component"))?;
        [r, s]
            .to_der()
            .map_err(|e| der_err(e, "encoding the ECDSA signature as DER"))
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct EcdsaKeyPairInner(EcKey);

impl EcdsaKeyPairInner {
    pub fn generate(curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        let curve_type = match curve {
            EcdsaCurve::P384 => CurveType::NistP384,
        };
        let key = EcKey::generate_key_pair(curve_type, EcKeyUsage::EcDsa)
            .map_err(|e| err(e, "generating the ECDSA key pair"))?;
        Ok(Self(key))
    }

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<EcdsaSignatureInner, EcdsaError> {
        let signature = self
            .0
            .ecdsa_sign(hash)
            .map_err(|e| err(e, "computing the ECDSA signature"))?;
        Ok(EcdsaSignatureInner(signature))
    }

    pub(crate) fn as_pub(&self) -> &EcdsaPublicKeyInner {
        // SAFETY: EcdsaPublicKeyInner is just a wrapper around the same EcKey.
        unsafe { std::mem::transmute::<&EcdsaKeyPairInner, &EcdsaPublicKeyInner>(self) }
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct EcdsaPublicKeyInner(EcKey);

impl EcdsaPublicKeyInner {
    pub fn from_public_key_bytes(
        curve: EcdsaCurve,
        public_key: &EcdsaPublicKeyComponents,
    ) -> Result<Self, EcdsaError> {
        let key_size = curve.key_size();
        if public_key.qx.len() != key_size || public_key.qy.len() != key_size {
            return Err(invalid_err("unexpected EC public key coordinate lengths"));
        }

        let curve_type = match curve {
            EcdsaCurve::P384 => CurveType::NistP384,
        };
        let public_key = [public_key.qx.as_slice(), public_key.qy.as_slice()].concat();
        let key = EcKey::set_public_key(curve_type, &public_key, EcKeyUsage::EcDsa)
            .map_err(|e| err(e, "importing the EC public key"))?;
        Ok(Self(key))
    }

    pub fn from_public_key_der(spki_der: &[u8]) -> Result<Self, EcdsaError> {
        let spki = x509_cert::spki::SubjectPublicKeyInfoRef::from_der(spki_der)
            .map_err(|e| der_err(e, "parsing the SubjectPublicKeyInfo"))?;
        Self::from_public_key_info(&spki)
    }

    pub(crate) fn from_public_key_info(
        spki: &x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
    ) -> Result<Self, EcdsaError> {
        if spki.algorithm.oid != der::oid::db::rfc5912::ID_EC_PUBLIC_KEY {
            return Err(invalid_err("public key is not an EC key"));
        }
        // Determine the curve from the algorithm's named-curve parameter rather
        // than requiring the caller to specify it.
        let curve_oid = spki
            .algorithm
            .parameters
            .ok_or_else(|| invalid_err("EC public key has no named curve parameter"))?
            .decode_as::<der::asn1::ObjectIdentifier>()
            .map_err(|e| der_err(e, "decoding the EC curve OID"))?;
        let curve = if curve_oid == der::oid::db::rfc5912::SECP_384_R_1 {
            EcdsaCurve::P384
        } else {
            return Err(invalid_err("public key uses an unsupported EC curve"));
        };

        let point = spki
            .subject_public_key
            .as_bytes()
            .ok_or_else(|| invalid_err("EC public key is not byte-aligned"))?;
        let components =
            EcdsaPublicKeyComponents::from_uncompressed_point(point).ok_or_else(|| {
                invalid_err("EC public key is not an uncompressed point (0x04 || Qx || Qy)")
            })?;
        Self::from_public_key_bytes(curve, &components)
    }

    pub fn to_public_key_der(&self) -> Result<Vec<u8>, EcdsaError> {
        self.to_public_key_info()?
            .to_der()
            .map_err(|e| der_err(e, "encoding the SubjectPublicKeyInfo"))
    }

    pub(crate) fn to_public_key_info(
        &self,
    ) -> Result<x509_cert::spki::SubjectPublicKeyInfoOwned, EcdsaError> {
        let point = self.public_key_bytes()?.to_uncompressed_point();
        Ok(x509_cert::spki::SubjectPublicKeyInfoOwned {
            algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                oid: der::oid::db::rfc5912::ID_EC_PUBLIC_KEY,
                parameters: Some(der::oid::db::rfc5912::SECP_384_R_1.into()),
            },
            subject_public_key: der::asn1::BitString::from_bytes(&point)
                .map_err(|e| der_err(e, "encoding the EC public key point"))?,
        })
    }

    pub fn verify_prehash(
        &self,
        hash: &[u8],
        signature: &EcdsaSignatureInner,
    ) -> Result<bool, EcdsaError> {
        let key_size = self.0.get_curve_size() as usize;
        if signature.0.len() != key_size * 2 {
            return Ok(false);
        }
        match self.0.ecdsa_verify(&signature.0, hash) {
            Ok(()) => Ok(true),
            // `SignatureVerificationFailure` is the expected error for a
            // signature that does not match. `InvalidArgument` occurs when the
            // signature is malformed (e.g. wrong length, or a component that is
            // out of range), which likewise means "does not verify".
            Err(SymCryptError::SignatureVerificationFailure | SymCryptError::InvalidArgument) => {
                Ok(false)
            }
            Err(e) => Err(err(e, "verifying the ECDSA signature")),
        }
    }

    pub fn public_key_bytes(&self) -> Result<EcdsaPublicKeyComponents, EcdsaError> {
        let mut qx = self
            .0
            .export_public_key()
            .map_err(|e| err(e, "exporting the EC public key"))?;
        let key_size = self.0.get_curve_size() as usize;
        let qy = qx.split_off(key_size);
        Ok(EcdsaPublicKeyComponents { qx, qy })
    }
}
