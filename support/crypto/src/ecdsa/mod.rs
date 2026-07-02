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
