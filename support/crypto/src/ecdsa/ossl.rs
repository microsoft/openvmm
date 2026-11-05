// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using OpenSSL.

use super::EcdsaCurve;
use super::EcdsaError;
use super::EcdsaPublicKeyComponents;
use super::EcdsaSignatureComponents;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError(err, op))
}

pub struct EcdsaSignatureInner {
    sig: openssl::ecdsa::EcdsaSig,
    curve: EcdsaCurve,
}

impl EcdsaSignatureInner {
    pub fn from_der(curve: EcdsaCurve, der: &[u8]) -> Result<Self, EcdsaError> {
        let sig = openssl::ecdsa::EcdsaSig::from_der(der)
            .map_err(|e| err(e, "parsing the DER-encoded ECDSA signature"))?;
        // OpenSSL validated the header and length, so these accesses are in bounds.
        let mut header_len = 2;
        let mut content_len = usize::from(der[1]);
        if content_len & 0x80 != 0 {
            header_len += content_len & 0x7f;
            content_len = der[2..header_len]
                .iter()
                .fold(0, |len, &byte| (len << 8) | usize::from(byte));
        }
        if content_len != der.len() - header_len {
            return Err(err(
                openssl::error::ErrorStack::get(),
                "checking for trailing ECDSA signature data",
            ));
        }
        if sig.r().is_negative() || sig.s().is_negative() {
            return Err(err(
                openssl::error::ErrorStack::get(),
                "checking the ECDSA signature's unsigned DER encoding",
            ));
        }
        if sig.r().num_bytes() as usize > curve.key_size()
            || sig.s().num_bytes() as usize > curve.key_size()
        {
            return Err(err(
                openssl::error::ErrorStack::get(),
                "checking the ECDSA signature component sizes",
            ));
        }
        Ok(Self { sig, curve })
    }

    pub fn from_components(
        curve: EcdsaCurve,
        components: &EcdsaSignatureComponents,
    ) -> Result<Self, EcdsaError> {
        if components.r.len() != curve.key_size() || components.s.len() != curve.key_size() {
            return Err(err(
                openssl::error::ErrorStack::get(),
                "checking the ECDSA signature component lengths",
            ));
        }
        let r = openssl::bn::BigNum::from_slice(&components.r)
            .map_err(|e| err(e, "parsing the signature r component"))?;
        let s = openssl::bn::BigNum::from_slice(&components.s)
            .map_err(|e| err(e, "parsing the signature s component"))?;
        let sig = openssl::ecdsa::EcdsaSig::from_private_components(r, s)
            .map_err(|e| err(e, "constructing the ECDSA signature"))?;
        Ok(Self { sig, curve })
    }

    pub fn to_components(&self) -> Result<EcdsaSignatureComponents, EcdsaError> {
        let key_size = self.curve.key_size();
        let r = self
            .sig
            .r()
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "encoding the signature r component"))?;
        let s = self
            .sig
            .s()
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "encoding the signature s component"))?;
        Ok(EcdsaSignatureComponents { r, s })
    }

    pub fn to_der(&self) -> Result<Vec<u8>, EcdsaError> {
        self.sig
            .to_der()
            .map_err(|e| err(e, "encoding the ECDSA signature as DER"))
    }
}

#[repr(C)] // Needed for the transmute in as_pub.
pub struct EcdsaKeyPairInner {
    pub(crate) pkey: openssl::pkey::PKey<openssl::pkey::Private>,
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

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<EcdsaSignatureInner, EcdsaError> {
        let ec_key = self
            .pkey
            .ec_key()
            .map_err(|e| err(e, "extracting the EC key from the PKey"))?;
        let sig = openssl::ecdsa::EcdsaSig::sign(hash, &ec_key)
            .map_err(|e| err(e, "computing the ECDSA signature"))?;

        Ok(EcdsaSignatureInner {
            sig,
            curve: self.curve,
        })
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
    pub(crate) pkey: openssl::pkey::PKey<openssl::pkey::Public>,
    curve: EcdsaCurve,
}

impl EcdsaPublicKeyInner {
    pub fn from_public_key_bytes(
        curve: EcdsaCurve,
        public_key: &EcdsaPublicKeyComponents,
    ) -> Result<Self, EcdsaError> {
        let key_size = curve.key_size();
        if public_key.qx.len() != key_size || public_key.qy.len() != key_size {
            return Err(err(
                openssl::error::ErrorStack::get(),
                "checking EC public key coordinate lengths",
            ));
        }

        let nid = match curve {
            EcdsaCurve::P384 => openssl::nid::Nid::SECP384R1,
        };
        let group = openssl::ec::EcGroup::from_curve_name(nid)
            .map_err(|e| err(e, "creating the EC group"))?;
        let mut ctx =
            openssl::bn::BigNumContext::new().map_err(|e| err(e, "creating the BigNum context"))?;

        let encoded = public_key.to_uncompressed_point();
        let point = openssl::ec::EcPoint::from_bytes(&group, &encoded, &mut ctx)
            .map_err(|e| err(e, "parsing the EC public key point"))?;
        let ec_key = openssl::ec::EcKey::from_public_key(&group, &point)
            .map_err(|e| err(e, "constructing the EC public key"))?;
        let pkey = openssl::pkey::PKey::from_ec_key(ec_key)
            .map_err(|e| err(e, "wrapping the EC key in a PKey"))?;

        Ok(Self { pkey, curve })
    }

    pub fn from_public_key_der(spki_der: &[u8]) -> Result<Self, EcdsaError> {
        let pkey = openssl::pkey::PKey::public_key_from_der(spki_der)
            .map_err(|e| err(e, "parsing the SubjectPublicKeyInfo"))?;
        Self::from_pkey(pkey)
    }

    pub fn to_public_key_der(&self) -> Result<Vec<u8>, EcdsaError> {
        self.pkey
            .public_key_to_der()
            .map_err(|e| err(e, "encoding the SubjectPublicKeyInfo"))
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

    pub fn verify_prehash(
        &self,
        hash: &[u8],
        signature: &EcdsaSignatureInner,
    ) -> Result<bool, EcdsaError> {
        if signature.curve != self.curve {
            return Ok(false);
        }

        let ec_key = self
            .pkey
            .ec_key()
            .map_err(|e| err(e, "extracting the EC key from the PKey"))?;

        signature
            .sig
            .verify(hash, &ec_key)
            .map_err(|e| err(e, "verifying the ECDSA signature"))
    }

    pub fn public_key_bytes(&self) -> Result<EcdsaPublicKeyComponents, EcdsaError> {
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
        let qx = x
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "encoding the x coordinate"))?;
        let qy = y
            .to_vec_padded(key_size as i32)
            .map_err(|e| err(e, "encoding the y coordinate"))?;

        Ok(EcdsaPublicKeyComponents { qx, qy })
    }
}
