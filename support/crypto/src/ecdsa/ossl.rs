// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using OpenSSL.

use super::EcdsaCurve;
use super::EcdsaError;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError(err, op))
}

#[repr(C)] // Needed for the transmute in as_pub.
pub struct EcdsaKeyPairInner {
    pkey: openssl::pkey::PKey<openssl::pkey::Private>,
    curve: EcdsaCurve,
}

impl EcdsaKeyPairInner {
    pub fn generate(curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        let nid = match curve {
            EcdsaCurve::P384 => openssl::nid::Nid::SECP384R1,
        };
        let ec_group = openssl::ec::EcGroup::from_curve_name(nid)
            .map_err(|e| err(e, "creating the EC group"))?;
        let ec_key =
            openssl::ec::EcKey::generate(&ec_group).map_err(|e| err(e, "generating the EC key"))?;
        let pkey = openssl::pkey::PKey::from_ec_key(ec_key)
            .map_err(|e| err(e, "wrapping the EC key in a PKey"))?;
        Ok(Self { pkey, curve })
    }

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<Vec<u8>, EcdsaError> {
        let ec_key = self
            .pkey
            .ec_key()
            .map_err(|e| err(e, "extracting the EC key from the PKey"))?;
        let sig = openssl::ecdsa::EcdsaSig::sign(hash, &ec_key)
            .map_err(|e| err(e, "computing the ECDSA signature"))?;

        let key_size = self.curve.key_size();
        let r_bytes = sig
            .r()
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "encoding the signature r component"))?;
        let s_bytes = sig
            .s()
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "encoding the signature s component"))?;

        let mut result = Vec::with_capacity(key_size * 2);
        result.extend_from_slice(&r_bytes);
        result.extend_from_slice(&s_bytes);
        Ok(result)
    }

    pub(crate) fn as_pub(&self) -> &EcdsaPublicKeyInner {
        // SAFETY: both types are `repr(C)` with the same field layout, and
        // PKey<Private> can be safely treated as PKey<Public> for read-only
        // operations.
        unsafe { std::mem::transmute::<&EcdsaKeyPairInner, &EcdsaPublicKeyInner>(self) }
    }
}

#[repr(C)] // Needed for the transmute in as_pub.
pub struct EcdsaPublicKeyInner {
    pkey: openssl::pkey::PKey<openssl::pkey::Public>,
    curve: EcdsaCurve,
}

impl EcdsaPublicKeyInner {
    pub fn from_public_key_bytes(curve: EcdsaCurve, public_key: &[u8]) -> Result<Self, EcdsaError> {
        let nid = match curve {
            EcdsaCurve::P384 => openssl::nid::Nid::SECP384R1,
        };
        let group = openssl::ec::EcGroup::from_curve_name(nid)
            .map_err(|e| err(e, "creating the EC group"))?;
        let mut ctx =
            openssl::bn::BigNumContext::new().map_err(|e| err(e, "creating the BigNum context"))?;

        // `public_key` is the raw `Qx || Qy` affine coordinates. Prepend the
        // `0x04` uncompressed-point tag and let OpenSSL parse the encoding,
        // which rejects a wrong-length input and a point that is not on the
        // curve.
        let mut encoded = Vec::with_capacity(1 + public_key.len());
        encoded.push(0x04);
        encoded.extend_from_slice(public_key);
        let point = openssl::ec::EcPoint::from_bytes(&group, &encoded, &mut ctx)
            .map_err(|e| err(e, "parsing the EC public key point"))?;
        let ec_key = openssl::ec::EcKey::from_public_key(&group, &point)
            .map_err(|e| err(e, "constructing the EC public key"))?;
        ec_key
            .check_key()
            .map_err(|e| err(e, "validating the EC public key"))?;
        let pkey = openssl::pkey::PKey::from_ec_key(ec_key)
            .map_err(|e| err(e, "wrapping the EC key in a PKey"))?;

        Ok(Self { pkey, curve })
    }

    pub fn from_public_key_der(spki_der: &[u8]) -> Result<Self, EcdsaError> {
        let pkey = openssl::pkey::PKey::public_key_from_der(spki_der)
            .map_err(|e| err(e, "parsing the SubjectPublicKeyInfo"))?;
        Self::from_pkey(pkey)
    }

    /// Wrap an already-parsed OpenSSL public key as an ECDSA public key,
    /// determining the curve from the key and validating it. Fails if the key
    /// is not an EC key on a supported curve.
    pub(crate) fn from_pkey(
        pkey: openssl::pkey::PKey<openssl::pkey::Public>,
    ) -> Result<Self, EcdsaError> {
        let ec_key = pkey
            .ec_key()
            .map_err(|e| err(e, "checking that the public key is an EC key"))?;
        // Determine the curve from the key rather than requiring the caller to
        // specify it.
        let curve = match ec_key.group().curve_name() {
            Some(openssl::nid::Nid::SECP384R1) => EcdsaCurve::P384,
            _ => {
                return Err(err(
                    openssl::error::ErrorStack::get(),
                    "checking that the public key uses a supported EC curve",
                ));
            }
        };
        ec_key
            .check_key()
            .map_err(|e| err(e, "validating the EC public key"))?;
        Ok(Self { pkey, curve })
    }

    pub fn verify_prehash(&self, hash: &[u8], signature: &[u8]) -> Result<bool, EcdsaError> {
        let key_size = self.curve.key_size();
        // A signature must be exactly `r || s`, each `key_size` bytes. Any
        // other length cannot be a valid signature for this curve.
        if signature.len() != key_size * 2 {
            return Ok(false);
        }

        let ec_key = self
            .pkey
            .ec_key()
            .map_err(|e| err(e, "extracting the EC key from the PKey"))?;

        let r = openssl::bn::BigNum::from_slice(&signature[..key_size])
            .map_err(|e| err(e, "parsing the signature r component"))?;
        let s = openssl::bn::BigNum::from_slice(&signature[key_size..])
            .map_err(|e| err(e, "parsing the signature s component"))?;
        let sig = openssl::ecdsa::EcdsaSig::from_private_components(r, s)
            .map_err(|e| err(e, "constructing the ECDSA signature"))?;

        sig.verify(hash, &ec_key)
            .map_err(|e| err(e, "verifying the ECDSA signature"))
    }

    pub fn public_key_bytes(&self) -> Result<Vec<u8>, EcdsaError> {
        let ec_key = self
            .pkey
            .ec_key()
            .map_err(|e| err(e, "extracting the EC key from the PKey"))?;
        let group = ec_key.group();
        let pub_key = ec_key.public_key();

        let mut ctx =
            openssl::bn::BigNumContext::new().map_err(|e| err(e, "creating the BigNum context"))?;
        let mut x = openssl::bn::BigNum::new()
            .map_err(|e| err(e, "creating a BigNum for the x coordinate"))?;
        let mut y = openssl::bn::BigNum::new()
            .map_err(|e| err(e, "creating a BigNum for the y coordinate"))?;

        pub_key
            .affine_coordinates_gfp(group, &mut x, &mut y, &mut ctx)
            .map_err(|e| err(e, "reading the public key affine coordinates"))?;

        let key_size = self.curve.key_size();
        let x_bytes = x
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "encoding the x coordinate"))?;
        let y_bytes = y
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "encoding the y coordinate"))?;

        let mut result = Vec::with_capacity(key_size * 2);
        result.extend_from_slice(&x_bytes);
        result.extend_from_slice(&y_bytes);
        Ok(result)
    }
}
