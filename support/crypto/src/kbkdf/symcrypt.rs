// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SP800-108 KBKDF implementation using SymCrypt.

use super::KbkdfError;
use symcrypt::hmac::HmacAlgorithm;
use symcrypt::sp800_108::sp800_108_counter_mode;

pub fn kbkdf_hmac_sha256(
    key: &[u8],
    context: &[u8],
    salt: &[u8],
    output_len: usize,
) -> Result<Vec<u8>, KbkdfError> {
    sp800_108_counter_mode(
        HmacAlgorithm::HmacSha256,
        key,
        salt,
        context,
        output_len as u64,
    )
    .map_err(|e| KbkdfError(crate::BackendError::SymCrypt(e, "deriving SP800-108 KBKDF")))
}
