// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SP800-108 KBKDF implementation using SymCrypt.

use super::KbkdfError;
use symcrypt::hmac::HmacAlgorithm;
use symcrypt::sp800_108::sp800_108_counter_mode;

pub fn kbkdf_hmac_sha256(
    key: &[u8],
    context: &[u8],
    label: &[u8],
    output_len: usize,
) -> Result<Vec<u8>, KbkdfError> {
    // SymCrypt accepts an empty key, but other backends (e.g. OpenSSL) reject
    // it. Reject it here for consistent behavior across backends.
    if key.is_empty() {
        return Err(KbkdfError(crate::BackendError::SymCrypt(
            symcrypt::errors::SymCryptError::InvalidArgument,
            "deriving SP800-108 KBKDF",
        )));
    }
    sp800_108_counter_mode(HmacAlgorithm::HmacSha256, key, label, context, output_len)
        .map_err(|e| KbkdfError(crate::BackendError::SymCrypt(e, "deriving SP800-108 KBKDF")))
}
