// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using SymCrypt.

use super::EcdsaCurve;
use super::EcdsaError;
use super::EcdsaPublicKeyComponents;
use super::EcdsaSignature;
use der::Decode;
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

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<EcdsaSignature, EcdsaError> {
        let mut r = self
            .0
            .ecdsa_sign(hash)
            .map_err(|e| err(e, "computing the ECDSA signature"))?;
        let s = r.split_off(self.0.get_curve_size() as usize);
        Ok(EcdsaSignature { r, s })
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

        let components =
            EcdsaPublicKeyComponents::from_uncompressed_point(spki.subject_public_key.raw_bytes())
                .ok_or_else(|| {
                    invalid_err("EC public key is not an uncompressed point (0x04 || Qx || Qy)")
                })?;
        Self::from_public_key_bytes(curve, &components)
    }

    pub fn verify_prehash(
        &self,
        hash: &[u8],
        signature: &EcdsaSignature,
    ) -> Result<bool, EcdsaError> {
        let key_size = self.0.get_curve_size() as usize;
        if signature.r.len() != key_size || signature.s.len() != key_size {
            return Ok(false);
        }
        let signature = [signature.r.as_slice(), signature.s.as_slice()].concat();
        match self.0.ecdsa_verify(&signature, hash) {
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
