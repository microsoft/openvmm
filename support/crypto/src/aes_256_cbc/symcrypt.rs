// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use ::symcrypt::cipher::AesExpandedKey;
use ::symcrypt::errors::SymCryptError;
use std::sync::Arc;

fn err(e: SymCryptError, op: &'static str) -> Aes256CbcError {
    Aes256CbcError(crate::BackendError(e, op))
}

pub struct Aes256CbcInner {
    key: Arc<AesExpandedKey>,
}

pub struct Aes256CbcEncCtxInner<'a> {
    key: &'a AesExpandedKey,
}

pub struct Aes256CbcDecCtxInner<'a> {
    key: &'a AesExpandedKey,
}

fn iv(iv: &[u8], op: &'static str) -> Result<[u8; 16], Aes256CbcError> {
    iv.try_into()
        .map_err(|_| err(SymCryptError::WrongDataSize, op))
}

impl Aes256CbcInner {
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256CbcError> {
        let expanded = AesExpandedKey::new(key).map_err(|e| err(e, "expanding AES key"))?;
        Ok(Aes256CbcInner {
            key: Arc::new(expanded),
        })
    }

    pub fn enc_ctx(&self) -> Result<Aes256CbcEncCtxInner<'_>, Aes256CbcError> {
        Ok(Aes256CbcEncCtxInner { key: &self.key })
    }

    pub fn dec_ctx(&self) -> Result<Aes256CbcDecCtxInner<'_>, Aes256CbcError> {
        Ok(Aes256CbcDecCtxInner { key: &self.key })
    }
}

impl Aes256CbcEncCtxInner<'_> {
    pub fn cipher(&mut self, iv_bytes: &[u8], data: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
        let mut chaining_value = iv(iv_bytes, "setting iv for encryption")?;
        let mut output = vec![0u8; data.len()];
        self.key
            .aes_cbc_encrypt(&mut chaining_value, data, &mut output)
            .map_err(|e| err(e, "encrypting data"))?;
        Ok(output)
    }
}

impl Aes256CbcDecCtxInner<'_> {
    pub fn cipher(&mut self, iv_bytes: &[u8], data: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
        let mut chaining_value = iv(iv_bytes, "setting iv for decryption")?;
        let mut output = vec![0u8; data.len()];
        self.key
            .aes_cbc_decrypt(&mut chaining_value, data, &mut output)
            .map_err(|e| err(e, "decrypting data"))?;
        Ok(output)
    }
}
