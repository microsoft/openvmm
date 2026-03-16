// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KBKDF (Key-Based Key Derivation Function) from SP800-108.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// Error returned by KDF operations.
#[derive(Debug, Error)]
#[error("KDF error")]
pub struct KdfError(#[source] super::BackendError);

/// Hash algorithm selection for KDF.
#[derive(Debug, Clone, Copy)]
pub enum HashAlgorithm {
    /// SHA-256
    Sha256,
}

/// KDF mode.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Mode {
    /// Counter mode
    Counter,
    /// Feedback mode
    Feedback,
}

/// MAC type for KDF.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Mac {
    /// HMAC
    Hmac,
    /// CMAC
    Cmac,
}

/// Builder for a KBKDF derivation.
pub struct Kbkdf(sys::KbkdfInner);

impl Kbkdf {
    /// Create a KBKDF with HMAC mode using the given hash, salt (label), and
    /// key.
    pub fn new(hash: HashAlgorithm, salt: Vec<u8>, key: Vec<u8>) -> Self {
        Self(sys::KbkdfInner::new(hash, salt, key))
    }

    /// Set the context (info) bytes.
    pub fn set_context(&mut self, context: Vec<u8>) {
        self.0.set_context(context);
    }

    /// Set the KDF mode.
    pub fn set_mode(&mut self, mode: Mode) {
        self.0.set_mode(mode);
    }

    /// Set the MAC type.
    pub fn set_mac(&mut self, mac: Mac) {
        self.0.set_mac(mac);
    }

    /// Set the seed bytes (for feedback mode).
    pub fn set_seed(&mut self, seed: Vec<u8>) {
        self.0.set_seed(seed);
    }

    /// Whether to include a length field.
    pub fn set_l(&mut self, l: bool) {
        self.0.set_l(l);
    }

    /// Whether to include the separator byte.
    pub fn set_separator(&mut self, separator: bool) {
        self.0.set_separator(separator);
    }
}

/// Derive key material into `output` using the configured KBKDF parameters.
pub fn derive(kdf: Kbkdf, output: &mut [u8]) -> Result<(), KdfError> {
    sys::derive(kdf.0, output)
}
