// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn sha_256(data: &[u8]) -> [u8; 32] {
    symcrypt::hash::sha256(data)
}
