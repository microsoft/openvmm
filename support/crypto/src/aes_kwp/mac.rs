// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES key wrap with padding (RFC 5649) implementation using CommonCrypto
//! on macOS. CommonCrypto's `CCSymmetricKeyWrap` only implements RFC 3394
//! (no padding), so we use `CCCrypt` in ECB mode and implement RFC 5649
//! on top.

use super::AesKeyWrapError;
use super::AesKeyWrapErrorInner;
use super::kwp;
use crate::mac::OsStatusCode;
use std::ffi::c_void;

const K_CC_ENCRYPT: u32 = 0;
const K_CC_DECRYPT: u32 = 1;
const K_CC_ALGORITHM_AES: u32 = 0;
const K_CC_OPTION_ECB_MODE: u32 = 2;

// CommonCrypto ships in libSystem.dylib, which is auto-linked on macOS, so
// no explicit `#[link]` attribute is required.
unsafe extern "C" {
    fn CCCrypt(
        op: u32,
        alg: u32,
        options: u32,
        key: *const c_void,
        key_length: usize,
        iv: *const c_void,
        data_in: *const c_void,
        data_in_length: usize,
        data_out: *mut c_void,
        data_out_available: usize,
        data_out_moved: *mut usize,
    ) -> i32;
}

fn err(status: i32, op: &'static str) -> AesKeyWrapError {
    AesKeyWrapError(AesKeyWrapErrorInner::Backend(
        crate::BackendError::OsStatus(OsStatusCode(status), op),
    ))
}

fn ecb_block(
    key: &[u8],
    op: u32,
    block: [u8; kwp::AES_BLOCK_LEN],
) -> Result<[u8; kwp::AES_BLOCK_LEN], AesKeyWrapError> {
    let mut out = [0u8; kwp::AES_BLOCK_LEN];
    let mut out_len: usize = 0;
    // SAFETY: pointers/lengths are valid for the duration of the call.
    let status = unsafe {
        CCCrypt(
            op,
            K_CC_ALGORITHM_AES,
            K_CC_OPTION_ECB_MODE,
            key.as_ptr().cast(),
            key.len(),
            std::ptr::null(),
            block.as_ptr().cast(),
            block.len(),
            out.as_mut_ptr().cast(),
            out.len(),
            &mut out_len,
        )
    };
    if status != 0 {
        return Err(err(
            status,
            if op == K_CC_ENCRYPT {
                "AES-ECB encrypt"
            } else {
                "AES-ECB decrypt"
            },
        ));
    }
    Ok(out)
}

pub struct AesKeyWrapInner {
    key: Vec<u8>,
}

pub struct AesKeyWrapCtxInner<'a> {
    key: &'a [u8],
}

pub struct AesKeyUnwrapCtxInner<'a> {
    key: &'a [u8],
}

impl AesKeyWrapInner {
    pub fn new(key: &[u8]) -> Result<Self, AesKeyWrapError> {
        match key.len() {
            16 | 24 | 32 => {}
            n => {
                return Err(AesKeyWrapError(AesKeyWrapErrorInner::InvalidKeySize(n)));
            }
        }
        Ok(AesKeyWrapInner { key: key.to_vec() })
    }

    pub fn wrap_ctx(&self) -> Result<AesKeyWrapCtxInner<'_>, AesKeyWrapError> {
        Ok(AesKeyWrapCtxInner { key: &self.key })
    }

    pub fn unwrap_ctx(&self) -> Result<AesKeyUnwrapCtxInner<'_>, AesKeyWrapError> {
        Ok(AesKeyUnwrapCtxInner { key: &self.key })
    }
}

impl AesKeyWrapCtxInner<'_> {
    pub fn wrap(&mut self, payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        kwp::wrap(payload, |block| ecb_block(self.key, K_CC_ENCRYPT, block))
    }
}

impl AesKeyUnwrapCtxInner<'_> {
    pub fn unwrap(&mut self, wrapped: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        match kwp::unwrap(wrapped, |block| ecb_block(self.key, K_CC_DECRYPT, block))? {
            Some(v) => Ok(v),
            // -4304 == kCCDecodeError
            None => Err(err(-4304, "AES key unwrap integrity check")),
        }
    }
}
