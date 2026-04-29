// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::KdfError;
use ::symcrypt::errors::SymCryptError;
use ::symcrypt::hmac::HmacSha256State;
use ::symcrypt::hmac::HmacState;
use ::symcrypt::hmac::SHA256_HMAC_RESULT_SIZE;

fn err(e: SymCryptError, op: &'static str) -> KdfError {
    KdfError(crate::BackendError(e, op))
}

/// SP800-108 KBKDF in counter mode using HMAC-SHA-256 as the PRF.
///
/// For each iteration `i` (1, 2, ...):
///   `K(i) = HMAC-SHA-256(key, [i]_BE32 || salt || 0x00 || context || [L]_BE32)`
///
/// where `L` is the output length in bits, big-endian as a 32-bit value.
pub fn kbkdf_hmac_sha256(
    key: &[u8],
    context: &[u8],
    salt: &[u8],
    output_len: usize,
) -> Result<Vec<u8>, KdfError> {
    let l_bits: u32 = u32::try_from(output_len)
        .ok()
        .and_then(|n| n.checked_mul(8))
        .ok_or_else(|| {
            err(
                SymCryptError::WrongDataSize,
                "computing output length in bits",
            )
        })?;
    let l_be = l_bits.to_be_bytes();

    let mut output = Vec::with_capacity(output_len);
    let mut counter: u32 = 1;
    while output.len() < output_len {
        let mut state =
            HmacSha256State::new(key).map_err(|e| err(e, "creating HMAC-SHA-256 state"))?;
        state.append(&counter.to_be_bytes());
        state.append(salt);
        state.append(&[0u8]);
        state.append(context);
        state.append(&l_be);
        let block = state.result();

        let remaining = output_len - output.len();
        let take = remaining.min(SHA256_HMAC_RESULT_SIZE);
        output.extend_from_slice(&block[..take]);

        counter = counter
            .checked_add(1)
            .ok_or_else(|| err(SymCryptError::WrongDataSize, "counter overflow"))?;
    }
    Ok(output)
}
