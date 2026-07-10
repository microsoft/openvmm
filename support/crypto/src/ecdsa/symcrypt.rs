// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using SymCrypt.

use super::EcdsaCurve;
use super::EcdsaError;

fn err(e: symcrypt::errors::SymCryptError, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError::SymCrypt(e, op))
}

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
