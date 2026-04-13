// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn sha_256(data: &[u8]) -> [u8; 32] {
    let mut hasher = openssl::sha::Sha256::new();
    hasher.update(data);
    hasher.finish()
}
