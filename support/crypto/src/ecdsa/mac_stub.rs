// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA stub for macOS. ECDSA is not yet implemented for the macOS native
//! backend; all operations return an error.

use super::EcdsaCurve;
use super::EcdsaError;

pub struct EcdsaKeyPairInner;

impl EcdsaKeyPairInner {
    pub fn generate(_curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        Err(EcdsaError(crate::BackendError::Null(
            "ECDSA not implemented for macOS native backend",
        )))
    }

    pub fn sign_prehash(&self, _hash: &[u8]) -> Result<Vec<u8>, EcdsaError> {
        unreachable!("cannot be constructed")
    }

    pub fn public_key_bytes(&self) -> Result<Vec<u8>, EcdsaError> {
        unreachable!("cannot be constructed")
    }
}
