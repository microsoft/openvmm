// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-384 implementation using SymCrypt.

use symcrypt::hash::HashState;

pub struct Sha384(symcrypt::hash::Sha384State);

impl Sha384 {
    pub fn new() -> Self {
        Self(symcrypt::hash::Sha384State::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.append(data);
    }

    pub fn finish(mut self) -> [u8; 48] {
        self.0.result()
    }
}
