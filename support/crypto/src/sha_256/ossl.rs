// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 implementation using OpenSSL.

pub fn sha_256(data: &[u8]) -> [u8; 32] {
    let mut hasher = openssl::sha::Sha256::new();
    hasher.update(data);
    hasher.finish()
}
