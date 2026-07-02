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

    pub fn public_key_bytes(&self) -> Result<Vec<u8>, EcdsaError> {
        self.key
            .export_public_key()
            .map_err(|e| err(e, "exporting public key"))
    }
}
