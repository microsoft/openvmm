// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 implementation using SymCrypt.

use symcrypt::hash::HashState;

pub struct Sha256(symcrypt::hash::Sha256State);

impl Sha256 {
    pub fn new() -> Self {
        Self(symcrypt::hash::Sha256State::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.append(data);
    }

    pub fn finish(mut self) -> [u8; 32] {
        self.0.result()
    }
}
