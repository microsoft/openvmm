// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 hashing.

/// Compute the SHA-256 hash of `data`.
pub fn sha_256(data: &[u8]) -> [u8; 32] {
    let mut hasher = openssl::sha::Sha256::new();
    hasher.update(data);
    hasher.finish()
}
