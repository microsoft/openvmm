// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HMAC implementation using SymCrypt.

use super::HmacError;
use crate::HashAlgorithm;
use symcrypt::errors::SymCryptError;
use symcrypt::hmac::hmac_sha1;
use symcrypt::hmac::hmac_sha256;
use symcrypt::hmac::hmac_sha384;
use symcrypt::hmac::hmac_sha512;

fn err(e: SymCryptError, op: &'static str) -> HmacError {
    HmacError(crate::BackendError::SymCrypt(e, op))
}

pub fn hmac(key: &[u8], data: &[u8], hash_algorithm: HashAlgorithm) -> Result<Vec<u8>, HmacError> {
    if key.is_empty() {
        return Err(err(SymCryptError::InvalidArgument, "computing the HMAC"));
    }

    #[expect(deprecated)]
    let output = match hash_algorithm {
        HashAlgorithm::Sha1 => hmac_sha1(key, data).map(|h| h.to_vec()),
        HashAlgorithm::Sha256 => hmac_sha256(key, data).map(|h| h.to_vec()),
        HashAlgorithm::Sha384 => hmac_sha384(key, data).map(|h| h.to_vec()),
        HashAlgorithm::Sha512 => hmac_sha512(key, data).map(|h| h.to_vec()),
    };
    output.map_err(|e| err(e, "computing the HMAC"))
}
