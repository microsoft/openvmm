// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-384 implementation using OpenSSL.

pub struct Sha384(openssl::sha::Sha384);

impl Sha384 {
    pub fn new() -> Self {
        Self(openssl::sha::Sha384::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> [u8; 48] {
        self.0.finish()
    }
}
