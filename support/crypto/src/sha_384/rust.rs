// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-384 implementation using the `sha2` RustCrypto crate.

use super::SHA_384_OUTPUT_SIZE_BYTES;
use sha2::Digest;

pub struct Sha384(sha2::Sha384);

impl Sha384 {
    pub fn new() -> Self {
        Self(sha2::Sha384::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> [u8; SHA_384_OUTPUT_SIZE_BYTES] {
        self.0.finalize().into()
    }
}
