// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 hashing.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

/// Compute the SHA-256 hash of `data`.
pub fn sha_256(data: &[u8]) -> [u8; 32] {
    sys::sha_256(data)
}
