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
    let mut p = vec![0u8; padded_len];
    p[..payload.len()].copy_from_slice(payload);
    let a = {
        let mut a = [0u8; SEMIBLOCK_LEN];
        a[..4].copy_from_slice(&AIV_PREFIX);
        a[4..].copy_from_slice(&mli.to_be_bytes());
        a
    };

    if p.len() == SEMIBLOCK_LEN {
        // Single semiblock: encrypt AIV || P directly.
        let mut block = [0u8; AES_BLOCK_LEN];
        block[..SEMIBLOCK_LEN].copy_from_slice(&a);
        block[SEMIBLOCK_LEN..].copy_from_slice(&p);
        return Ok(encrypt_block(block)?.to_vec());
    }

    // RFC 3394 wrap with AIV as initial value.
    let n = p.len() / SEMIBLOCK_LEN;
    let mut a_reg = a;
    let mut r: Vec<[u8; SEMIBLOCK_LEN]> = (0..n)
        .map(|i| {
            let mut s = [0u8; SEMIBLOCK_LEN];
            s.copy_from_slice(&p[i * SEMIBLOCK_LEN..(i + 1) * SEMIBLOCK_LEN]);
            s
        })
        .collect();
    for j in 0..6u64 {
        for (i, ri) in r.iter_mut().enumerate() {
            let mut block = [0u8; AES_BLOCK_LEN];
            block[..SEMIBLOCK_LEN].copy_from_slice(&a_reg);
            block[SEMIBLOCK_LEN..].copy_from_slice(ri);
            let b = encrypt_block(block)?;
            let t = (n as u64) * j + (i as u64) + 1;
            let msb = u64::from_be_bytes(b[..SEMIBLOCK_LEN].try_into().unwrap()) ^ t;
            a_reg = msb.to_be_bytes();
            ri.copy_from_slice(&b[SEMIBLOCK_LEN..]);
        }
    }
    let mut out = Vec::with_capacity(SEMIBLOCK_LEN + p.len());
    out.extend_from_slice(&a_reg);
    for s in &r {
        out.extend_from_slice(s);
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
    let (a_reg, p_bytes) = if wrapped.len() == AES_BLOCK_LEN {
        let mut block = [0u8; AES_BLOCK_LEN];
        block.copy_from_slice(wrapped);
        let dec = decrypt_block(block)?;
        let mut a = [0u8; SEMIBLOCK_LEN];
        a.copy_from_slice(&dec[..SEMIBLOCK_LEN]);
        (a, dec[SEMIBLOCK_LEN..].to_vec())
    } else {
        let n = wrapped.len() / SEMIBLOCK_LEN - 1;
        let mut a_reg = [0u8; SEMIBLOCK_LEN];
        a_reg.copy_from_slice(&wrapped[..SEMIBLOCK_LEN]);
        let mut r: Vec<[u8; SEMIBLOCK_LEN]> = (0..n)
            .map(|i| {
                let mut s = [0u8; SEMIBLOCK_LEN];
                s.copy_from_slice(&wrapped[(i + 1) * SEMIBLOCK_LEN..(i + 2) * SEMIBLOCK_LEN]);
                s
            })
            .collect();
        for j in (0..6u64).rev() {
            for i in (0..n).rev() {
                let t = (n as u64) * j + (i as u64) + 1;
                let a_xor = u64::from_be_bytes(a_reg) ^ t;
                let mut block = [0u8; AES_BLOCK_LEN];
                block[..SEMIBLOCK_LEN].copy_from_slice(&a_xor.to_be_bytes());
                block[SEMIBLOCK_LEN..].copy_from_slice(&r[i]);
                let b = decrypt_block(block)?;
                a_reg.copy_from_slice(&b[..SEMIBLOCK_LEN]);
                r[i].copy_from_slice(&b[SEMIBLOCK_LEN..]);
            }
        }
        let mut p = Vec::with_capacity(n * SEMIBLOCK_LEN);
        for s in &r {
            p.extend_from_slice(s);
        }
        (a_reg, p)
    };

    if a_reg[..4] != AIV_PREFIX {
        return Ok(None);
    }
    let mli = u32::from_be_bytes([a_reg[4], a_reg[5], a_reg[6], a_reg[7]]) as usize;
    if mli == 0 || mli > p_bytes.len() || (p_bytes.len() - mli) >= SEMIBLOCK_LEN {
        return Ok(None);
    }
    if p_bytes[mli..].iter().any(|&b| b != 0) {
        return Ok(None);
    }
    Ok(Some(p_bytes[..mli].to_vec()))
}
