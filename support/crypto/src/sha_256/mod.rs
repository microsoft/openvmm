// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 hashing.

#![cfg(any(openssl, rust, symcrypt))]

#[cfg(openssl)]
mod ossl;
#[cfg(openssl)]
use ossl as sys;

#[cfg(rust)]
mod rust;
#[cfg(rust)]
use rust as sys;

#[cfg(symcrypt)]
mod symcrypt;
#[cfg(symcrypt)]
use symcrypt as sys;

/// Compute the SHA-256 hash of `data`.
pub fn sha_256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finish()
}

/// Incremental SHA-256 hasher.
///
/// Equivalent to [`sha_256`] when all data is hashed in a single [`update`](Self::update)
/// call, but allows the data to be hashed in multiple chunks.
pub struct Sha256(sys::Sha256);

impl Sha256 {
    /// Create a new incremental SHA-256 hasher.
    pub fn new() -> Self {
        Self(sys::Sha256::new())
    }

    /// Append `data` to the hash.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// Consume the hasher and return the final 32-byte digest.
    pub fn finish(self) -> [u8; 32] {
        self.0.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_256_known_vectors() {
        const EMPTY_HASH: [u8; 32] = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];

        let hash = sha_256(&[]);
        assert_eq!(hash, EMPTY_HASH);

        const PANGRAM: [u8; 32] = [
            0xd7, 0xa8, 0xfb, 0xb3, 0x07, 0xd7, 0x80, 0x94, 0x69, 0xca, 0x9a, 0xbc, 0xb0, 0x08,
            0x2e, 0x4f, 0x8d, 0x56, 0x51, 0xe4, 0x6d, 0x3c, 0xdb, 0x76, 0x2d, 0x02, 0xd0, 0xbf,
            0x37, 0xc9, 0xe5, 0x92,
        ];

        let hash = sha_256(b"The quick brown fox jumps over the lazy dog");
        assert_eq!(hash, PANGRAM);
    }

    #[test]
    fn sha_256_incremental_known_vectors() {
        const EMPTY_HASH: [u8; 32] = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];

        let hasher = Sha256::new();
        assert_eq!(hasher.finish(), EMPTY_HASH);

        const PANGRAM: [u8; 32] = [
            0xd7, 0xa8, 0xfb, 0xb3, 0x07, 0xd7, 0x80, 0x94, 0x69, 0xca, 0x9a, 0xbc, 0xb0, 0x08,
            0x2e, 0x4f, 0x8d, 0x56, 0x51, 0xe4, 0x6d, 0x3c, 0xdb, 0x76, 0x2d, 0x02, 0xd0, 0xbf,
            0x37, 0xc9, 0xe5, 0x92,
        ];

        let mut hasher = Sha256::new();
        hasher.update(b"The quick brown fox ");
        hasher.update(b"jumps over ");
        hasher.update(b"the lazy dog");
        assert_eq!(hasher.finish(), PANGRAM);
    }
}
