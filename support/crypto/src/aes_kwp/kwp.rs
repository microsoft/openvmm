// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backend-agnostic RFC 5649 AES key wrap with padding, built on top of
//! a single-block AES-ECB primitive supplied by the caller.
//!
//! Used by the native Windows (BCrypt) and macOS (CommonCrypto) backends,
//! which do not expose KWP directly.

#![cfg(any(all(native, windows), all(native, target_os = "macos")))]

use super::AesKeyWrapError;

pub(super) const AES_BLOCK_LEN: usize = 16;
pub(super) const SEMIBLOCK_LEN: usize = 8;
const AIV_PREFIX: [u8; 4] = [0xA6, 0x59, 0x59, 0xA6];

pub(super) fn wrap<F>(payload: &[u8], mut encrypt_block: F) -> Result<Vec<u8>, AesKeyWrapError>
where
    F: FnMut([u8; AES_BLOCK_LEN]) -> Result<[u8; AES_BLOCK_LEN], AesKeyWrapError>,
{
    let mli = payload.len() as u32;
    let padded_len = payload.len().div_ceil(SEMIBLOCK_LEN) * SEMIBLOCK_LEN;
    let a = {
        let mut a = [0u8; SEMIBLOCK_LEN];
        a[..4].copy_from_slice(&AIV_PREFIX);
        a[4..].copy_from_slice(&mli.to_be_bytes());
        a
    };

    if padded_len == SEMIBLOCK_LEN {
        // Single semiblock: encrypt AIV || P directly.
        let mut block = [0u8; AES_BLOCK_LEN];
        block[..SEMIBLOCK_LEN].copy_from_slice(&a);
        block[SEMIBLOCK_LEN..SEMIBLOCK_LEN + payload.len()].copy_from_slice(payload);
        return Ok(encrypt_block(block)?.to_vec());
    }

    // RFC 3394 wrap with AIV as the initial value, operating in place on the
    // output buffer to avoid scratch allocations: `out[..8]` holds the A
    // register and `out[8..]` holds the registers R[1..=n], initialized to the
    // zero-padded payload.
    let n = padded_len / SEMIBLOCK_LEN;
    let mut out = vec![0u8; SEMIBLOCK_LEN + padded_len];
    out[..SEMIBLOCK_LEN].copy_from_slice(&a);
    out[SEMIBLOCK_LEN..SEMIBLOCK_LEN + payload.len()].copy_from_slice(payload);
    for j in 0..6u64 {
        for i in 0..n {
            let r = (i + 1) * SEMIBLOCK_LEN;
            let mut block = [0u8; AES_BLOCK_LEN];
            block[..SEMIBLOCK_LEN].copy_from_slice(&out[..SEMIBLOCK_LEN]);
            block[SEMIBLOCK_LEN..].copy_from_slice(&out[r..r + SEMIBLOCK_LEN]);
            let b = encrypt_block(block)?;
            let t = (n as u64) * j + (i as u64) + 1;
            let msb = u64::from_be_bytes(b[..SEMIBLOCK_LEN].try_into().unwrap()) ^ t;
            out[..SEMIBLOCK_LEN].copy_from_slice(&msb.to_be_bytes());
            out[r..r + SEMIBLOCK_LEN].copy_from_slice(&b[SEMIBLOCK_LEN..]);
        }
    }
    Ok(out)
}

/// Unwraps using RFC 5649. Returns `Ok(None)` on integrity failure so the
/// caller can map that to a backend-specific error; returns `Err` for
/// underlying cipher failures.
pub(super) fn unwrap<F>(
    wrapped: &[u8],
    mut decrypt_block: F,
) -> Result<Option<Vec<u8>>, AesKeyWrapError>
where
    F: FnMut([u8; AES_BLOCK_LEN]) -> Result<[u8; AES_BLOCK_LEN], AesKeyWrapError>,
{
    if wrapped.len() < AES_BLOCK_LEN || !wrapped.len().is_multiple_of(SEMIBLOCK_LEN) {
        return Ok(None);
    }
    let mut a_reg = [0u8; SEMIBLOCK_LEN];
    // `p` starts as the ciphertext registers R[1..=n] and is unwrapped in place
    // into the recovered plaintext, reusing the same allocation for the result.
    let mut p = if wrapped.len() == AES_BLOCK_LEN {
        let mut block = [0u8; AES_BLOCK_LEN];
        block.copy_from_slice(wrapped);
        let dec = decrypt_block(block)?;
        a_reg.copy_from_slice(&dec[..SEMIBLOCK_LEN]);
        dec[SEMIBLOCK_LEN..].to_vec()
    } else {
        let n = wrapped.len() / SEMIBLOCK_LEN - 1;
        a_reg.copy_from_slice(&wrapped[..SEMIBLOCK_LEN]);
        let mut p = wrapped[SEMIBLOCK_LEN..].to_vec();
        for j in (0..6u64).rev() {
            for i in (0..n).rev() {
                let r = i * SEMIBLOCK_LEN;
                let t = (n as u64) * j + (i as u64) + 1;
                let a_xor = u64::from_be_bytes(a_reg) ^ t;
                let mut block = [0u8; AES_BLOCK_LEN];
                block[..SEMIBLOCK_LEN].copy_from_slice(&a_xor.to_be_bytes());
                block[SEMIBLOCK_LEN..].copy_from_slice(&p[r..r + SEMIBLOCK_LEN]);
                let b = decrypt_block(block)?;
                a_reg.copy_from_slice(&b[..SEMIBLOCK_LEN]);
                p[r..r + SEMIBLOCK_LEN].copy_from_slice(&b[SEMIBLOCK_LEN..]);
            }
        }
        p
    };

    if a_reg[..4] != AIV_PREFIX {
        return Ok(None);
    }
    let mli = u32::from_be_bytes([a_reg[4], a_reg[5], a_reg[6], a_reg[7]]) as usize;
    if mli == 0 || mli > p.len() || (p.len() - mli) >= SEMIBLOCK_LEN {
        return Ok(None);
    }
    if p[mli..].iter().any(|&b| b != 0) {
        return Ok(None);
    }
    p.truncate(mli);
    Ok(Some(p))
}
