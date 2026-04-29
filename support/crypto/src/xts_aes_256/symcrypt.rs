// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IEEE 1619 XTS-AES-256, implemented on top of `symcrypt`'s `AesExpandedKey`.
//! Symcrypt 0.6 does not expose AES-XTS or AES-ECB directly; we build XTS
//! out of single-block AES encrypt/decrypt operations. AES-CBC over a single
//! 16-byte block with a zero `chaining_value` is equivalent to AES-ECB for
//! that block.

use super::*;
use ::symcrypt::cipher::AesExpandedKey;
use ::symcrypt::errors::SymCryptError;
use std::sync::Arc;

const BLOCK: usize = 16;
const HALF_KEY: usize = KEY_LEN / 2;

fn err(e: SymCryptError, op: &'static str) -> XtsAes256Error {
    XtsAes256Error(crate::BackendError(e, op))
}

fn aes_block_encrypt(
    key: &AesExpandedKey,
    block: &[u8; BLOCK],
) -> Result<[u8; BLOCK], XtsAes256Error> {
    let mut chaining_value = [0u8; BLOCK];
    let mut out = [0u8; BLOCK];
    key.aes_cbc_encrypt(&mut chaining_value, block, &mut out)
        .map_err(|e| err(e, "AES block encrypt"))?;
    Ok(out)
}

fn aes_block_decrypt(
    key: &AesExpandedKey,
    block: &[u8; BLOCK],
) -> Result<[u8; BLOCK], XtsAes256Error> {
    let mut chaining_value = [0u8; BLOCK];
    let mut out = [0u8; BLOCK];
    key.aes_cbc_decrypt(&mut chaining_value, block, &mut out)
        .map_err(|e| err(e, "AES block decrypt"))?;
    Ok(out)
}

/// Multiply `T` by α in GF(2^128) using IEEE 1619 byte order (little-endian).
fn gf128_mul_alpha(t: &mut [u8; BLOCK]) {
    let mut carry = 0u8;
    for byte in t.iter_mut() {
        let next_carry = *byte >> 7;
        *byte = (*byte << 1) | carry;
        carry = next_carry;
    }
    if carry != 0 {
        t[0] ^= 0x87;
    }
}

pub struct XtsAes256Inner {
    k1: Arc<AesExpandedKey>,
    k2: Arc<AesExpandedKey>,
}

pub struct XtsAes256EncCtxInner<'a> {
    k1: &'a AesExpandedKey,
    k2: &'a AesExpandedKey,
}

pub struct XtsAes256DecCtxInner<'a> {
    k1: &'a AesExpandedKey,
    k2: &'a AesExpandedKey,
}

impl XtsAes256Inner {
    pub fn new(key: &[u8; KEY_LEN], _data_unit_size: u32) -> Result<Self, XtsAes256Error> {
        let k1 = AesExpandedKey::new(&key[..HALF_KEY]).map_err(|e| err(e, "expanding K1"))?;
        let k2 = AesExpandedKey::new(&key[HALF_KEY..]).map_err(|e| err(e, "expanding K2"))?;
        Ok(Self {
            k1: Arc::new(k1),
            k2: Arc::new(k2),
        })
    }

    pub fn enc_ctx(&self) -> Result<XtsAes256EncCtxInner<'_>, XtsAes256Error> {
        Ok(XtsAes256EncCtxInner {
            k1: &self.k1,
            k2: &self.k2,
        })
    }

    pub fn dec_ctx(&self) -> Result<XtsAes256DecCtxInner<'_>, XtsAes256Error> {
        Ok(XtsAes256DecCtxInner {
            k1: &self.k1,
            k2: &self.k2,
        })
    }
}

fn initial_tweak(tweak: u128) -> [u8; BLOCK] {
    tweak.to_le_bytes()
}

fn check_block_aligned(data: &[u8]) -> Result<(), XtsAes256Error> {
    if data.len() < BLOCK || !data.len().is_multiple_of(BLOCK) {
        return Err(err(
            SymCryptError::WrongBlockSize,
            "data must be a non-zero multiple of 16 bytes",
        ));
    }
    Ok(())
}

impl XtsAes256EncCtxInner<'_> {
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        check_block_aligned(data)?;
        let mut t = aes_block_encrypt(self.k2, &initial_tweak(tweak))?;
        for chunk in data.chunks_exact_mut(BLOCK) {
            let mut block = [0u8; BLOCK];
            for i in 0..BLOCK {
                block[i] = chunk[i] ^ t[i];
            }
            let enc = aes_block_encrypt(self.k1, &block)?;
            for i in 0..BLOCK {
                chunk[i] = enc[i] ^ t[i];
            }
            gf128_mul_alpha(&mut t);
        }
        Ok(())
    }
}

impl XtsAes256DecCtxInner<'_> {
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        check_block_aligned(data)?;
        let mut t = aes_block_encrypt(self.k2, &initial_tweak(tweak))?;
        for chunk in data.chunks_exact_mut(BLOCK) {
            let mut block = [0u8; BLOCK];
            for i in 0..BLOCK {
                block[i] = chunk[i] ^ t[i];
            }
            let dec = aes_block_decrypt(self.k1, &block)?;
            for i in 0..BLOCK {
                chunk[i] = dec[i] ^ t[i];
            }
            gf128_mul_alpha(&mut t);
        }
        Ok(())
    }
}
