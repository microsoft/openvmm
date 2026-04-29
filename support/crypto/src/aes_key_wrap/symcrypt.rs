// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RFC 5649 AES key wrap with padding, implemented on top of `symcrypt`'s
//! `AesExpandedKey`. Symcrypt 0.6 does not expose AES-ECB or AES key wrap
//! directly, so we build the algorithm out of single-block AES encryption.
//! AES-CBC over a single 16-byte block with a zero `chaining_value` is
//! equivalent to AES-ECB on that block.

use super::AesKeyWrapError;
use ::symcrypt::cipher::AesExpandedKey;
use ::symcrypt::errors::SymCryptError;
use std::sync::Arc;

const AIV_PREFIX: [u8; 4] = [0xA6, 0x59, 0x59, 0xA6];

fn err(e: SymCryptError, op: &'static str) -> AesKeyWrapError {
    AesKeyWrapError::Backend(crate::BackendError(e, op))
}

fn validate_key_size(key: &[u8]) -> Result<(), AesKeyWrapError> {
    match key.len() {
        16 | 24 | 32 => Ok(()),
        n => Err(AesKeyWrapError::InvalidKeySize(n)),
    }
}

/// Encrypt a single 16-byte block (AES-ECB equivalent).
fn aes_block_encrypt(key: &AesExpandedKey, block: &[u8; 16]) -> Result<[u8; 16], AesKeyWrapError> {
    let mut chaining_value = [0u8; 16];
    let mut out = [0u8; 16];
    key.aes_cbc_encrypt(&mut chaining_value, block, &mut out)
        .map_err(|e| err(e, "AES block encrypt"))?;
    Ok(out)
}

/// Decrypt a single 16-byte block (AES-ECB equivalent).
fn aes_block_decrypt(key: &AesExpandedKey, block: &[u8; 16]) -> Result<[u8; 16], AesKeyWrapError> {
    let mut chaining_value = [0u8; 16];
    let mut out = [0u8; 16];
    key.aes_cbc_decrypt(&mut chaining_value, block, &mut out)
        .map_err(|e| err(e, "AES block decrypt"))?;
    Ok(out)
}

pub struct AesKeyWrapInner {
    key: Arc<AesExpandedKey>,
}

pub struct AesKeyWrapCtxInner<'a> {
    key: &'a AesExpandedKey,
}

pub struct AesKeyUnwrapCtxInner<'a> {
    key: &'a AesExpandedKey,
}

impl AesKeyWrapInner {
    pub fn new(key: &[u8]) -> Result<Self, AesKeyWrapError> {
        validate_key_size(key)?;
        let expanded = AesExpandedKey::new(key).map_err(|e| err(e, "expanding AES key"))?;
        Ok(Self {
            key: Arc::new(expanded),
        })
    }

    pub fn wrap_ctx(&self) -> Result<AesKeyWrapCtxInner<'_>, AesKeyWrapError> {
        Ok(AesKeyWrapCtxInner { key: &self.key })
    }

    pub fn unwrap_ctx(&self) -> Result<AesKeyUnwrapCtxInner<'_>, AesKeyWrapError> {
        Ok(AesKeyUnwrapCtxInner { key: &self.key })
    }
}

fn aiv(payload_len: u32) -> [u8; 8] {
    let mut a = [0u8; 8];
    a[..4].copy_from_slice(&AIV_PREFIX);
    a[4..].copy_from_slice(&payload_len.to_be_bytes());
    a
}

impl AesKeyWrapCtxInner<'_> {
    pub fn wrap(&mut self, payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        // Pad payload to a multiple of 8 bytes with zeros.
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| err(SymCryptError::WrongDataSize, "payload too large"))?;
        let n = payload.len().div_ceil(8).max(1);
        let padded_len = n * 8;
        let mut padded = vec![0u8; padded_len];
        padded[..payload.len()].copy_from_slice(payload);

        let a = aiv(payload_len);

        if n == 1 {
            // Single 64-bit block: AES-Encrypt(KEK, AIV || padded).
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&a);
            block[8..].copy_from_slice(&padded);
            let ct = aes_block_encrypt(self.key, &block)?;
            return Ok(ct.to_vec());
        }

        // RFC 3394 wrap with initial value AIV.
        let mut a = a;
        let mut r: Vec<[u8; 8]> = (0..n)
            .map(|i| {
                let mut blk = [0u8; 8];
                blk.copy_from_slice(&padded[i * 8..(i + 1) * 8]);
                blk
            })
            .collect();
        for j in 0..6u64 {
            for i in 1..=n {
                let mut block = [0u8; 16];
                block[..8].copy_from_slice(&a);
                block[8..].copy_from_slice(&r[i - 1]);
                let b = aes_block_encrypt(self.key, &block)?;
                let t: u64 = (n as u64) * j + i as u64;
                let mut msb = [0u8; 8];
                msb.copy_from_slice(&b[..8]);
                let msb = u64::from_be_bytes(msb);
                a = (msb ^ t).to_be_bytes();
                r[i - 1].copy_from_slice(&b[8..]);
            }
        }

        let mut output = Vec::with_capacity(8 * (n + 1));
        output.extend_from_slice(&a);
        for blk in &r {
            output.extend_from_slice(blk);
        }
        Ok(output)
    }
}

impl AesKeyUnwrapCtxInner<'_> {
    pub fn unwrap(&mut self, wrapped_payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        if wrapped_payload.len() < 16 || !wrapped_payload.len().is_multiple_of(8) {
            return Err(err(SymCryptError::WrongDataSize, "wrapped payload size"));
        }

        let (a, padded) = if wrapped_payload.len() == 16 {
            let mut block = [0u8; 16];
            block.copy_from_slice(wrapped_payload);
            let pt = aes_block_decrypt(self.key, &block)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(&pt[..8]);
            (a, pt[8..].to_vec())
        } else {
            // RFC 3394 unwrap.
            let n = wrapped_payload.len() / 8 - 1;
            let mut a = [0u8; 8];
            a.copy_from_slice(&wrapped_payload[..8]);
            let mut r: Vec<[u8; 8]> = (0..n)
                .map(|i| {
                    let mut blk = [0u8; 8];
                    blk.copy_from_slice(&wrapped_payload[8 + i * 8..16 + i * 8]);
                    blk
                })
                .collect();
            for j in (0..6u64).rev() {
                for i in (1..=n).rev() {
                    let t: u64 = (n as u64) * j + i as u64;
                    let mut msb = [0u8; 8];
                    msb.copy_from_slice(&a);
                    let msb = u64::from_be_bytes(msb);
                    let xored = (msb ^ t).to_be_bytes();
                    let mut block = [0u8; 16];
                    block[..8].copy_from_slice(&xored);
                    block[8..].copy_from_slice(&r[i - 1]);
                    let b = aes_block_decrypt(self.key, &block)?;
                    a.copy_from_slice(&b[..8]);
                    r[i - 1].copy_from_slice(&b[8..]);
                }
            }
            let mut padded = Vec::with_capacity(n * 8);
            for blk in &r {
                padded.extend_from_slice(blk);
            }
            (a, padded)
        };

        if a[..4] != AIV_PREFIX {
            return Err(err(
                SymCryptError::AuthenticationFailure,
                "AIV magic mismatch",
            ));
        }
        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&a[4..]);
        let payload_len = u32::from_be_bytes(len_bytes) as usize;
        if payload_len > padded.len() || padded.len() - payload_len >= 8 {
            return Err(err(
                SymCryptError::AuthenticationFailure,
                "AIV length out of range",
            ));
        }
        // Validate the trailing pad bytes are all zero.
        if padded[payload_len..].iter().any(|&b| b != 0) {
            return Err(err(
                SymCryptError::AuthenticationFailure,
                "non-zero padding",
            ));
        }
        Ok(padded[..payload_len].to_vec())
    }
}
