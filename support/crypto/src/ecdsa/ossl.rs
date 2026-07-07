// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using OpenSSL.

use super::EcdsaCurve;
use super::EcdsaError;

fn err(e: openssl::error::ErrorStack, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError(e, op))
}

pub struct EcdsaKeyPairInner {
    pkey: openssl::pkey::PKey<openssl::pkey::Private>,
    curve: EcdsaCurve,
}

impl EcdsaKeyPairInner {
    pub fn generate(curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        let nid = match curve {
            EcdsaCurve::P384 => openssl::nid::Nid::SECP384R1,
        };
        let ec_group =
            openssl::ec::EcGroup::from_curve_name(nid).map_err(|e| err(e, "creating EC group"))?;
        let ec_key =
            openssl::ec::EcKey::generate(&ec_group).map_err(|e| err(e, "generating EC key"))?;
        let pkey = openssl::pkey::PKey::from_ec_key(ec_key)
            .map_err(|e| err(e, "converting EC key to PKey"))?;
        Ok(Self { pkey, curve })
    }

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<Vec<u8>, EcdsaError> {
        let ec_key = self
            .pkey
            .ec_key()
            .map_err(|e| err(e, "getting EC key from PKey"))?;
        let sig =
            openssl::ecdsa::EcdsaSig::sign(hash, &ec_key).map_err(|e| err(e, "ECDSA sign"))?;

        let key_size = self.curve.key_size();
        let r_bytes = sig
            .r()
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "padding r"))?;
        let s_bytes = sig
            .s()
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "padding s"))?;

        let mut result = Vec::with_capacity(key_size * 2);
        result.extend_from_slice(&r_bytes);
        result.extend_from_slice(&s_bytes);
        Ok(result)
    }

    pub fn public_key_bytes(&self) -> Result<Vec<u8>, EcdsaError> {
        let ec_key = self
            .pkey
            .ec_key()
            .map_err(|e| err(e, "getting EC key from PKey"))?;
        let group = ec_key.group();
        let pub_key = ec_key.public_key();

        let mut ctx =
            openssl::bn::BigNumContext::new().map_err(|e| err(e, "creating BigNumContext"))?;
        let mut x = openssl::bn::BigNum::new().map_err(|e| err(e, "creating BigNum for x"))?;
        let mut y = openssl::bn::BigNum::new().map_err(|e| err(e, "creating BigNum for y"))?;

        pub_key
            .affine_coordinates_gfp(group, &mut x, &mut y, &mut ctx)
            .map_err(|e| err(e, "getting affine coordinates"))?;

        let key_size = self.curve.key_size();
        let x_bytes = x
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "padding x coordinate"))?;
        let y_bytes = y
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "padding y coordinate"))?;

        let mut result = Vec::with_capacity(key_size * 2);
        result.extend_from_slice(&x_bytes);
        result.extend_from_slice(&y_bytes);
        Ok(result)
    }
}
