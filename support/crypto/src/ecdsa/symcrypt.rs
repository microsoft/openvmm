// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using SymCrypt.

use super::EcdsaCurve;
use super::EcdsaError;
use der::Decode;

fn err(e: symcrypt::errors::SymCryptError, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError::SymCrypt(e, op))
}

fn der_err(e: der::Error, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError::Der(e, op))
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct EcdsaKeyPairInner {
    key: symcrypt::ecc::EcKey,
}

impl EcdsaKeyPairInner {
    pub fn generate(curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        let curve_type = match curve {
            EcdsaCurve::P384 => symcrypt::ecc::CurveType::NistP384,
        };
        let key =
            symcrypt::ecc::EcKey::generate_key_pair(curve_type, symcrypt::ecc::EcKeyUsage::EcDsa)
                .map_err(|e| err(e, "generating ECDSA key pair"))?;
        Ok(Self { key })
    }

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<Vec<u8>, EcdsaError> {
        self.key.ecdsa_sign(hash).map_err(|e| err(e, "ECDSA sign"))
    }

    pub(crate) fn as_pub(&self) -> &EcdsaPublicKeyInner {
        // SAFETY: EcdsaPublicKeyInner is just a wrapper around the same EcKey.
        unsafe { std::mem::transmute::<&EcdsaKeyPairInner, &EcdsaPublicKeyInner>(self) }
    }
}

#[repr(transparent)] // Needed for the transmute in as_pub.
pub struct EcdsaPublicKeyInner {
    key: symcrypt::ecc::EcKey,
}

impl EcdsaPublicKeyInner {
    pub fn new(curve: EcdsaCurve, public_key: &[u8]) -> Result<Self, EcdsaError> {
        let curve_type = match curve {
            EcdsaCurve::P384 => symcrypt::ecc::CurveType::NistP384,
        };
        // SymCrypt's `set_public_key` validates that `public_key` is exactly the
        // expected `Qx || Qy` length for the curve (rejecting both short and
        // long inputs with `InvalidArgument`), so no separate length check is
        // needed here.
        let key = symcrypt::ecc::EcKey::set_public_key(
            curve_type,
            public_key,
            symcrypt::ecc::EcKeyUsage::EcDsa,
        )
        .map_err(|e| err(e, "importing public key"))?;
        Ok(Self { key })
    }

    pub fn from_public_key_der(spki_der: &[u8]) -> Result<Self, EcdsaError> {
        /// RFC 5480 named-curve OID for NIST P-384 (secp384r1).
        const SECP384R1: der::asn1::ObjectIdentifier =
            der::asn1::ObjectIdentifier::new_unwrap("1.3.132.0.34");

        let spki = x509_cert::spki::SubjectPublicKeyInfoRef::from_der(spki_der)
            .map_err(|e| der_err(e, "parsing SubjectPublicKeyInfo"))?;

        // Determine the curve from the algorithm's named-curve parameter rather
        // than requiring the caller to specify it.
        let curve_oid = spki
            .algorithm
            .parameters
            .ok_or_else(|| {
                err(
                    symcrypt::errors::SymCryptError::InvalidArgument,
                    "missing EC curve parameters",
                )
            })?
            .decode_as::<der::asn1::ObjectIdentifier>()
            .map_err(|e| der_err(e, "decoding EC curve OID"))?;
        let curve = if curve_oid == SECP384R1 {
            EcdsaCurve::P384
        } else {
            return Err(err(
                symcrypt::errors::SymCryptError::InvalidArgument,
                "unsupported or unrecognized EC curve",
            ));
        };

        // The SubjectPublicKey bit string is the uncompressed EC point
        // `0x04 || Qx || Qy`; strip the `0x04` tag to get `Qx || Qy`.
        let point = spki.subject_public_key.raw_bytes();
        if point.len() != 1 + 2 * curve.key_size() || point[0] != 0x04 {
            return Err(err(
                symcrypt::errors::SymCryptError::InvalidArgument,
                "public key is not an uncompressed EC point (0x04 || Qx || Qy)",
            ));
        }
        Self::new(curve, &point[1..])
    }

    pub fn verify_prehash(&self, hash: &[u8], signature: &[u8]) -> Result<bool, EcdsaError> {
        match self.key.ecdsa_verify(signature, hash) {
            Ok(()) => Ok(true),
            // `SignatureVerificationFailure` is the expected error for a
            // signature that does not match. `InvalidArgument` occurs when the
            // signature is malformed (e.g. wrong length, or a component that is
            // out of range), which likewise means "does not verify".
            Err(
                symcrypt::errors::SymCryptError::SignatureVerificationFailure
                | symcrypt::errors::SymCryptError::InvalidArgument,
            ) => Ok(false),
            Err(e) => Err(err(e, "ECDSA verify")),
        }
    }

    pub fn public_key_bytes(&self) -> Result<Vec<u8>, EcdsaError> {
        self.key
            .export_public_key()
            .map_err(|e| err(e, "exporting public key"))
    }
}
