// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA cryptographic operations (key generation, signing, public key export).

// The ecdsa module is available on backends that support it: OpenSSL (Linux glibc),
// SymCrypt (Linux musl), and BCrypt (Windows). On macOS, ECDSA is not yet implemented.
#![cfg(any(
    openssl,
    symcrypt,
    all(native, windows),
    all(native, target_os = "macos")
))]

#[cfg(openssl)]
mod ossl;
#[cfg(openssl)]
use ossl as sys;

#[cfg(all(native, windows))]
mod win;
#[cfg(all(native, windows))]
use win as sys;

#[cfg(symcrypt)]
mod symcrypt_stub;
#[cfg(symcrypt)]
use symcrypt_stub as sys;

// macOS stub: provides the types so the module compiles under clippy,
// but all operations return an error at runtime.
#[cfg(all(native, target_os = "macos"))]
mod mac_stub;
#[cfg(all(native, target_os = "macos"))]
use mac_stub as sys;

use thiserror::Error;

/// An error for ECDSA operations.
#[derive(Debug, Error)]
#[error("ECDSA error")]
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

/// An ECDSA key pair (private + public key).
pub struct EcdsaKeyPair(sys::EcdsaKeyPairInner);

impl EcdsaKeyPair {
    /// Generate a new random ECDSA key pair for the given curve.
    pub fn generate(curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        sys::EcdsaKeyPairInner::generate(curve).map(Self)
    }

    /// Sign a pre-computed hash value. Returns the signature as `r || s`
    /// in big-endian, each component `curve.key_size()` bytes.
    pub fn sign_prehash(&self, hash: &[u8]) -> Result<Vec<u8>, EcdsaError> {
        self.0.sign_prehash(hash)
    }

    /// Export the public key as `Qx || Qy` in big-endian, each component
    /// `curve.key_size()` bytes.
    pub fn public_key_bytes(&self) -> Result<Vec<u8>, EcdsaError> {
        self.0.public_key_bytes()
    }
}

#[cfg(all(test, not(all(native, target_os = "macos"))))]
mod tests {
    use super::*;

    #[test]
    fn generate_p384_key_pair() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let pub_key = key.public_key_bytes().unwrap();
        // P-384 public key is Qx || Qy, each 48 bytes.
        assert_eq!(pub_key.len(), 96);
    }

    #[test]
    fn sign_prehash_p384_produces_correct_size() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        // SHA-384 hash (48 bytes)
        let hash = [0xABu8; 48];
        let sig = key.sign_prehash(&hash).unwrap();
        // P-384 signature is r || s, each 48 bytes.
        assert_eq!(sig.len(), 96);
    }

    #[test]
    fn sign_prehash_p384_is_non_deterministic() {
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let hash = [0x42u8; 48];
        let sig1 = key.sign_prehash(&hash).unwrap();
        let sig2 = key.sign_prehash(&hash).unwrap();
        // ECDSA uses a random nonce, so two signatures of the same hash
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
        let hash = [0x01u8; 48];
        let sig = key.sign_prehash(&hash).unwrap();

        let r = &sig[..48];
        let s = &sig[48..];

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

    /// Verify signature using OpenSSL (roundtrip test). This test exercises
    /// the full flow: generate key → sign → export public key → verify with
    /// the exported public key using the openssl crate directly.
    #[cfg(openssl)]
    #[test]
    fn roundtrip_sign_verify_with_openssl() {
        use openssl::bn::BigNum;
        use openssl::bn::BigNumContext;
        use openssl::ec::EcGroup;
        use openssl::ec::EcKey;
        use openssl::ec::EcPoint;
        use openssl::ecdsa::EcdsaSig;
        use openssl::nid::Nid;

        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let hash = [0xDE, 0xAD, 0xBE, 0xEF].repeat(12); // 48 bytes

        let sig_bytes = key.sign_prehash(&hash).unwrap();
        let pub_bytes = key.public_key_bytes().unwrap();

        // Reconstruct the public key using OpenSSL.
        let group = EcGroup::from_curve_name(Nid::SECP384R1).unwrap();
        let mut ctx = BigNumContext::new().unwrap();
        let x = BigNum::from_slice(&pub_bytes[..48]).unwrap();
        let y = BigNum::from_slice(&pub_bytes[48..]).unwrap();
        let mut pub_point = EcPoint::new(&group).unwrap();
        pub_point
            .set_affine_coordinates_gfp(&group, &x, &y, &mut ctx)
            .unwrap();
        let ec_pub = EcKey::from_public_key(&group, &pub_point).unwrap();

        // Reconstruct the ECDSA signature.
        let r = BigNum::from_slice(&sig_bytes[..48]).unwrap();
        let s = BigNum::from_slice(&sig_bytes[48..]).unwrap();
        let sig = EcdsaSig::from_private_components(r, s).unwrap();

        // Verify.
        assert!(sig.verify(&hash, &ec_pub).unwrap());
    }
}
