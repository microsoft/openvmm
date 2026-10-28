// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA cryptographic operations.

#![cfg(any(openssl, symcrypt, all(native, windows)))]

#[cfg(openssl)]
pub(crate) mod ossl;
#[cfg(openssl)]
use ossl as sys;

#[cfg(all(native, windows))]
pub(crate) mod win;
#[cfg(all(native, windows))]
use win as sys;

#[cfg(symcrypt)]
mod symcrypt;
#[cfg(symcrypt)]
use symcrypt as sys;

use crate::HashAlgorithm;
use thiserror::Error;

/// An error for ECDSA operations.
#[derive(Debug, Error)]
#[error("ECDSA operation failed")]
pub struct EcdsaError(#[source] pub(crate) super::BackendError);

/// The ECC curve to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcdsaCurve {
    /// NIST P-384 (secp384r1)
    P384,
}

impl EcdsaCurve {
    /// The size of a single coordinate or scalar for this curve, in bytes.
    pub fn key_size(self) -> usize {
        match self {
            EcdsaCurve::P384 => 48,
        }
    }
}

/// An ECDSA signature with separate unsigned big-endian scalar components.
///
/// Each component must be exactly [`EcdsaCurve::key_size`] bytes, including
/// leading zero padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcdsaSignature {
    /// The signature's `r` component.
    pub r: Vec<u8>,
    /// The signature's `s` component.
    pub s: Vec<u8>,
}

/// An ECDSA private key (key pair).
#[repr(transparent)] // Needed for the transmute in deref.
pub struct EcdsaKeyPair(pub(crate) sys::EcdsaKeyPairInner);

impl EcdsaKeyPair {
    /// Generate a new random ECDSA key pair for the given curve.
    pub fn generate(curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        sys::EcdsaKeyPairInner::generate(curve).map(Self)
    }

    /// Hash `data` with `hash_algorithm` and sign the resulting digest.
    /// Returns the signature with separate big-endian `r` and `s` components,
    /// each padded to `curve.key_size()` bytes.
    pub fn sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<EcdsaSignature, EcdsaError> {
        let hash = hash_algorithm.hash(data);
        self.0.sign_prehash(&hash)
    }
}

/// An ECDSA public key.
#[repr(transparent)] // Needed for the transmute in deref.
pub struct EcdsaPublicKey(pub(crate) sys::EcdsaPublicKeyInner);

/// The affine coordinates of an ECDSA public key as unsigned big-endian integers.
///
/// Each coordinate must be exactly [`EcdsaCurve::key_size`] bytes, including
/// leading zero padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcdsaPublicKeyComponents {
    /// The public key's `Qx` coordinate.
    pub qx: Vec<u8>,
    /// The public key's `Qy` coordinate.
    pub qy: Vec<u8>,
}

impl EcdsaPublicKeyComponents {
    /// SEC 1 tag for an uncompressed elliptic-curve point (`0x04 || X || Y`).
    const SEC1_UNCOMPRESSED_POINT_TAG: u8 = 0x04;

    /// Decode a SEC 1 uncompressed point (`0x04 || Qx || Qy`).
    ///
    /// Returns `None` for an incorrect tag or empty or unequal coordinates.
    /// Coordinate sizes and curve membership are validated when importing a key.
    pub fn from_uncompressed_point(point: &[u8]) -> Option<Self> {
        let coordinates = point.strip_prefix(&[Self::SEC1_UNCOMPRESSED_POINT_TAG])?;
        let (qx, qy) = coordinates.split_at(coordinates.len() / 2);
        if qx.is_empty() || qx.len() != qy.len() {
            return None;
        }
        Some(Self {
            qx: qx.to_vec(),
            qy: qy.to_vec(),
        })
    }

    /// Encode as a SEC 1 uncompressed point (`0x04 || Qx || Qy`).
    ///
    /// Coordinates are copied as-is, preserving leading zero padding.
    pub fn to_uncompressed_point(&self) -> Vec<u8> {
        let mut point = Vec::with_capacity(1 + self.qx.len() + self.qy.len());
        point.push(Self::SEC1_UNCOMPRESSED_POINT_TAG);
        point.extend_from_slice(&self.qx);
        point.extend_from_slice(&self.qy);
        point
    }
}

impl EcdsaPublicKey {
    /// Construct an ECDSA public key from separate big-endian `Qx` and `Qy`
    /// coordinates, each exactly `curve.key_size()` bytes (the format produced
    /// by [`EcdsaPublicKey::public_key_bytes`]). This is for verifying against
    /// an externally-supplied public key (e.g. one extracted from an X.509
    /// certificate) where no private key is held. Incorrectly sized coordinates
    /// and points that are not on the curve are rejected.
    pub fn from_public_key_bytes(
        curve: EcdsaCurve,
        public_key: &EcdsaPublicKeyComponents,
    ) -> Result<Self, EcdsaError> {
        sys::EcdsaPublicKeyInner::from_public_key_bytes(curve, public_key).map(Self)
    }

    /// Construct an ECDSA public key from a DER-encoded `SubjectPublicKeyInfo`
    /// (a bare public key, i.e. the `PUBLIC KEY` PEM type). The curve is
    /// determined from the encoded key; fails if it is not a supported curve.
    pub fn from_public_key_der(spki_der: &[u8]) -> Result<Self, EcdsaError> {
        sys::EcdsaPublicKeyInner::from_public_key_der(spki_der).map(Self)
    }

    /// Hash `message` with `hash_algorithm` and verify `signature` against this
    /// public key. Each signature component must be `curve.key_size()` bytes
    /// (i.e. the format produced by [`EcdsaKeyPair::sign`]). Returns `Ok(true)`
    /// if the signature is valid, `Ok(false)` if it is invalid (including
    /// incorrectly sized components), or an error for other failures.
    pub fn verify(
        &self,
        message: &[u8],
        signature: &EcdsaSignature,
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, EcdsaError> {
        let hash = hash_algorithm.hash(message);
        self.0.verify_prehash(&hash, signature)
    }

    /// Export the public key as separate big-endian `Qx` and `Qy` coordinates,
    /// each padded to `curve.key_size()` bytes.
    pub fn public_key_bytes(&self) -> Result<EcdsaPublicKeyComponents, EcdsaError> {
        self.0.public_key_bytes()
    }
}

impl std::ops::Deref for EcdsaKeyPair {
    type Target = EcdsaPublicKey;

    fn deref(&self) -> &Self::Target {
        // SAFETY: EcdsaPublicKey is just a wrapper around EcdsaPublicKeyInner.
        unsafe {
            std::mem::transmute::<&sys::EcdsaPublicKeyInner, &EcdsaPublicKey>(self.0.as_pub())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_p384_key_pair() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let pub_key = key.public_key_bytes().unwrap();
        assert_eq!(pub_key.qx.len(), EcdsaCurve::P384.key_size());
        assert_eq!(pub_key.qy.len(), EcdsaCurve::P384.key_size());
        let point = pub_key.to_uncompressed_point();
        let size = EcdsaCurve::P384.key_size();
        assert_eq!(point.len(), 1 + size * 2);
        assert_eq!(point[0], 0x04);
        assert_eq!(&point[1..1 + size], pub_key.qx.as_slice());
        assert_eq!(&point[1 + size..], pub_key.qy.as_slice());
        assert_eq!(
            EcdsaPublicKeyComponents::from_uncompressed_point(&point).unwrap(),
            pub_key
        );

        let mut padded = pub_key;
        padded.qx[0] = 0;
        padded.qy[0] = 0;
        assert_eq!(
            EcdsaPublicKeyComponents::from_uncompressed_point(&padded.to_uncompressed_point())
                .unwrap(),
            padded
        );
        for invalid in [
            &[][..],
            &[0],
            &[2, 1, 2],
            &[3, 1, 2],
            &[6, 1, 2],
            &[4],
            &[4, 1],
            &[4, 1, 2, 3],
        ] {
            assert!(EcdsaPublicKeyComponents::from_uncompressed_point(invalid).is_none());
        }
    }

    #[test]
    fn sign_p384_produces_correct_size() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let sig = key.sign(b"message to sign", HashAlgorithm::Sha384).unwrap();
        assert_eq!(sig.r.len(), EcdsaCurve::P384.key_size());
        assert_eq!(sig.s.len(), EcdsaCurve::P384.key_size());
    }

    #[test]
    fn sign_p384_is_non_deterministic() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let sig1 = key.sign(b"message to sign", HashAlgorithm::Sha384).unwrap();
        let sig2 = key.sign(b"message to sign", HashAlgorithm::Sha384).unwrap();
        // ECDSA uses a random nonce, so two signatures of the same message
        // should differ (with overwhelming probability).
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn two_keys_produce_different_public_keys() {
        let key1 = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let key2 = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        assert_ne!(
            key1.public_key_bytes().unwrap(),
            key2.public_key_bytes().unwrap()
        );
    }

    #[test]
    fn public_key_is_stable_across_exports() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let pk1 = key.public_key_bytes().unwrap();
        let pk2 = key.public_key_bytes().unwrap();
        assert_eq!(pk1, pk2);
    }

    /// Verify that the signature components (r, s) are valid big-endian
    /// integers — i.e., they are not all zeros and not larger than the
    /// curve order.
    #[test]
    fn signature_components_are_valid() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let sig = key.sign(b"data to sign", HashAlgorithm::Sha384).unwrap();

        let r = sig.r.as_slice();
        let s = sig.s.as_slice();

        // Neither component should be all zeros.
        assert_ne!(r, &[0u8; 48][..]);
        assert_ne!(s, &[0u8; 48][..]);

        // P-384 order n (big-endian):
        // FFFFFFFF FFFFFFFF FFFFFFFF FFFFFFFF FFFFFFFF FFFFFFFF
        // C7634D81 F4372DDF 581A0DB2 48B0A77A ECEC196A CCC52973
        let n: [u8; 48] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC7, 0x63, 0x4D, 0x81,
            0xF4, 0x37, 0x2D, 0xDF, 0x58, 0x1A, 0x0D, 0xB2, 0x48, 0xB0, 0xA7, 0x7A, 0xEC, 0xEC,
            0x19, 0x6A, 0xCC, 0xC5, 0x29, 0x73,
        ];

        // r and s must be < n (valid ECDSA signature).
        assert!(r < &n[..], "r must be less than curve order");
        assert!(s < &n[..], "s must be less than curve order");
    }

    /// Round-trip test exercising the full sign → verify flow using the
    /// crate's own API, so it runs on every implemented backend.
    #[test]
    fn roundtrip_sign_verify() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let public_key: &EcdsaPublicKey = &key;
        let message = b"roundtrip test message";

        let signature = key.sign(message, HashAlgorithm::Sha384).unwrap();

        // A valid signature over the original message verifies.
        assert!(
            public_key
                .verify(message, &signature, HashAlgorithm::Sha384)
                .unwrap()
        );

        // A signature checked against a different message does not verify.
        assert!(
            !public_key
                .verify(b"tampered message", &signature, HashAlgorithm::Sha384)
                .unwrap()
        );

        // A tampered signature does not verify.
        for tamper_r in [true, false] {
            let mut bad_signature = signature.clone();
            let component = if tamper_r {
                &mut bad_signature.r
            } else {
                &mut bad_signature.s
            };
            *component.last_mut().unwrap() ^= 0x01;
            assert!(
                !public_key
                    .verify(message, &bad_signature, HashAlgorithm::Sha384)
                    .unwrap()
            );
        }
    }

    #[test]
    fn verify_rejects_wrong_component_lengths() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let message = b"component length test";
        let signature = key.sign(message, HashAlgorithm::Sha384).unwrap();
        let size = EcdsaCurve::P384.key_size();

        for (r_len, s_len) in [
            (0, size),
            (size - 1, size),
            (size + 1, size),
            (size, 0),
            (size, size - 1),
            (size, size + 1),
            (size - 1, size + 1),
            (size + 1, size - 1),
        ] {
            let mut invalid = signature.clone();
            invalid.r.resize(r_len, 0);
            invalid.s.resize(s_len, 0);
            assert!(
                !key.verify(message, &invalid, HashAlgorithm::Sha384)
                    .unwrap(),
                "accepted component lengths r={r_len}, s={s_len}"
            );
        }

        // Leading zeroes preserve the scalar values, but not their required widths.
        for pad_r in [true, false] {
            let mut invalid = signature.clone();
            if pad_r {
                invalid.r.insert(0, 0);
            } else {
                invalid.s.insert(0, 0);
            }
            assert!(
                !key.verify(message, &invalid, HashAlgorithm::Sha384)
                    .unwrap()
            );
        }

        // Moving the boundary preserves r || s, but not the component widths.
        let mut invalid = signature;
        invalid.s.insert(0, invalid.r.pop().unwrap());
        assert!(
            !key.verify(message, &invalid, HashAlgorithm::Sha384)
                .unwrap()
        );
    }

    #[test]
    fn verify_rejects_out_of_range_components() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let message = b"component range test";
        let signature = key.sign(message, HashAlgorithm::Sha384).unwrap();

        for value in [0, 0xff] {
            for invalid_r in [true, false] {
                let mut invalid = signature.clone();
                if invalid_r {
                    invalid.r.fill(value);
                } else {
                    invalid.s.fill(value);
                }
                assert!(
                    !key.verify(message, &invalid, HashAlgorithm::Sha384)
                        .unwrap()
                );
            }
        }
    }

    /// Round-trip through a public key reconstructed from separate coordinates
    /// via [`EcdsaPublicKey::from_public_key_bytes`]: a signature made with a
    /// key pair verifies under its exported-and-reimported public key, and is
    /// rejected under a different key or a tampered message.
    #[test]
    fn roundtrip_public_key_from_bytes() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let message = b"reimport test message";
        let signature = key.sign(message, HashAlgorithm::Sha384).unwrap();

        let pk_bytes = key.public_key_bytes().unwrap();
        let public_key =
            EcdsaPublicKey::from_public_key_bytes(EcdsaCurve::P384, &pk_bytes).unwrap();
        assert_eq!(public_key.public_key_bytes().unwrap(), pk_bytes);

        // The reimported public key verifies the signature.
        assert!(
            public_key
                .verify(message, &signature, HashAlgorithm::Sha384)
                .unwrap()
        );

        // A tampered message does not verify.
        assert!(
            !public_key
                .verify(b"other message", &signature, HashAlgorithm::Sha384)
                .unwrap()
        );

        // A signature made by a different key does not verify.
        let other = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let other_sig = other.sign(message, HashAlgorithm::Sha384).unwrap();
        assert!(
            !public_key
                .verify(message, &other_sig, HashAlgorithm::Sha384)
                .unwrap()
        );
    }

    /// Each coordinate must have exactly the curve's key size, even if the
    /// concatenated coordinates would otherwise form a valid encoding.
    #[test]
    fn public_key_from_bytes_rejects_wrong_coordinate_lengths() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let pk_bytes = key.public_key_bytes().unwrap();
        let size = EcdsaCurve::P384.key_size();

        for (qx_len, qy_len) in [
            (0, 0),
            (size - 1, size - 1),
            (size + 1, size + 1),
            (0, size),
            (size - 1, size),
            (size + 1, size),
            (size, 0),
            (size, size - 1),
            (size, size + 1),
            (size - 1, size + 1),
            (size + 1, size - 1),
        ] {
            let mut invalid = pk_bytes.clone();
            invalid.qx.resize(qx_len, 0);
            invalid.qy.resize(qy_len, 0);
            assert!(
                EcdsaPublicKey::from_public_key_bytes(EcdsaCurve::P384, &invalid).is_err(),
                "accepted coordinate lengths qx={qx_len}, qy={qy_len}"
            );
        }

        let mut invalid = pk_bytes;
        invalid.qy.insert(0, invalid.qx.pop().unwrap());
        assert!(EcdsaPublicKey::from_public_key_bytes(EcdsaCurve::P384, &invalid).is_err());
    }

    #[test]
    fn public_key_from_bytes_rejects_invalid_coordinates() {
        let size = EcdsaCurve::P384.key_size();
        for value in [0, 0xff] {
            let invalid = EcdsaPublicKeyComponents {
                qx: vec![value; size],
                qy: vec![value; size],
            };
            assert!(EcdsaPublicKey::from_public_key_bytes(EcdsaCurve::P384, &invalid).is_err());
        }
    }

    /// A key exported as `Qx || Qy`, wrapped into a DER `SubjectPublicKeyInfo`,
    /// round-trips through [`EcdsaPublicKey::from_public_key_der`] and can
    /// verify a signature made by the original key pair.
    #[test]
    fn from_public_key_der_round_trip() {
        use der::Encode;

        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let message = b"spki round-trip message";
        let signature = key.sign(message, HashAlgorithm::Sha384).unwrap();
        let pk = key.public_key_bytes().unwrap();

        let point = pk.to_uncompressed_point();
        let spki = x509_cert::spki::SubjectPublicKeyInfo {
            algorithm: x509_cert::spki::AlgorithmIdentifier {
                oid: der::oid::db::rfc5912::ID_EC_PUBLIC_KEY,
                parameters: Some(der::oid::db::rfc5912::SECP_384_R_1),
            },
            subject_public_key: der::asn1::BitString::from_bytes(&point).unwrap(),
        };
        let spki_der = spki.to_der().unwrap();

        let public_key = EcdsaPublicKey::from_public_key_der(&spki_der).unwrap();
        assert_eq!(public_key.public_key_bytes().unwrap(), pk);
        assert!(
            public_key
                .verify(message, &signature, HashAlgorithm::Sha384)
                .unwrap()
        );
        // A tampered message must not verify.
        assert!(
            !public_key
                .verify(b"tampered", &signature, HashAlgorithm::Sha384)
                .unwrap()
        );

        // SEC 1's point at infinity can decode but is not a valid public key.
        let mut invalid_spki = spki;
        invalid_spki.subject_public_key = der::asn1::BitString::from_bytes(&[0]).unwrap();
        assert!(EcdsaPublicKey::from_public_key_der(&invalid_spki.to_der().unwrap()).is_err());

        invalid_spki.subject_public_key = der::asn1::BitString::from_bytes(&point).unwrap();
        for parameters in [
            None,
            Some(der::asn1::ObjectIdentifier::new_unwrap("1.3.132.0.35")),
        ] {
            invalid_spki.algorithm.parameters = parameters;
            assert!(EcdsaPublicKey::from_public_key_der(&invalid_spki.to_der().unwrap()).is_err());
        }
    }
}
