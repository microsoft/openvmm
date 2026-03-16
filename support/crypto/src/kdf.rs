// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KBKDF (Key-Based Key Derivation Function) from SP800-108.

pub use openssl_kdf::kdf::KdfError;
pub use openssl_kdf::kdf::Mac;
pub use openssl_kdf::kdf::Mode;

/// Hash algorithm selection for KDF.
#[derive(Debug, Clone, Copy)]
pub enum HashAlgorithm {
    /// SHA-256
    Sha256,
}

impl HashAlgorithm {
    fn to_message_digest(self) -> openssl::hash::MessageDigest {
        match self {
            HashAlgorithm::Sha256 => openssl::hash::MessageDigest::sha256(),
        }
    }
}

/// Builder for a KBKDF derivation.
pub struct Kbkdf {
    inner: openssl_kdf::kdf::Kbkdf,
}

impl Kbkdf {
    /// Create a KBKDF with HMAC mode using the given hash, salt (label), and
    /// key.
    pub fn new(hash: HashAlgorithm, salt: Vec<u8>, key: Vec<u8>) -> Self {
        Self {
            inner: openssl_kdf::kdf::Kbkdf::new(hash.to_message_digest(), salt, key),
        }
    }

    /// Set the context (info) bytes.
    pub fn set_context(&mut self, context: Vec<u8>) {
        self.inner.set_context(context);
    }

    /// Set the KDF mode.
    pub fn set_mode(&mut self, mode: Mode) {
        self.inner.set_mode(mode);
    }

    /// Set the MAC type.
    pub fn set_mac(&mut self, mac: Mac) {
        self.inner.set_mac(mac);
    }

    /// Set the seed bytes (for feedback mode).
    pub fn set_seed(&mut self, seed: Vec<u8>) {
        self.inner.set_seed(seed);
    }

    /// Whether to include a length field.
    pub fn set_l(&mut self, l: bool) {
        self.inner.set_l(l);
    }

    /// Whether to include the separator byte.
    pub fn set_separator(&mut self, separator: bool) {
        self.inner.set_separator(separator);
    }
}

/// Derive key material into `output` using the configured KBKDF parameters.
pub fn derive(kdf: Kbkdf, output: &mut [u8]) -> Result<(), KdfError> {
    openssl_kdf::kdf::derive(kdf.inner, output)
}
