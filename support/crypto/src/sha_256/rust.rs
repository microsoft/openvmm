// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 implementation using the `sha2` RustCrypto crate.

use super::SHA_256_OUTPUT_SIZE_BYTES;
use sha2::Digest;

pub struct Sha256(sha2::Sha256);

impl Sha256 {
    pub fn new() -> Self {
        Self(sha2::Sha256::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> [u8; SHA_256_OUTPUT_SIZE_BYTES] {
        self.0.finalize().into()
    }
}
