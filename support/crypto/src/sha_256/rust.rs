// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use sha2::Digest;

pub fn sha_256(data: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(data).into()
}
