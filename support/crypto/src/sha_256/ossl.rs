// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 implementation using OpenSSL.

pub struct Sha256(openssl::sha::Sha256);

impl Sha256 {
    pub fn new() -> Self {
        Self(openssl::sha::Sha256::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> [u8; 32] {
        self.0.finish()
    }
}
