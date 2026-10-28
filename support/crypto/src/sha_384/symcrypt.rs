// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-384 implementation using SymCrypt.

use super::SHA_384_OUTPUT_SIZE_BYTES;
use symcrypt::hash::HashState;

pub struct Sha384(symcrypt::hash::Sha384State);

impl Sha384 {
    pub fn new() -> Self {
        Self(symcrypt::hash::Sha384State::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.append(data);
    }

    pub fn finish(mut self) -> [u8; SHA_384_OUTPUT_SIZE_BYTES] {
        self.0.result()
    }
}
