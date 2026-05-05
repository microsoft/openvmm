// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use ::symcrypt::cipher::AesExpandedKey;

pub struct Aes256CbcInner {
    key: AesExpandedKey,
}

pub struct Aes256CbcEncCtxInner<'a> {
    key: &'a AesExpandedKey,
}

pub struct Aes256CbcDecCtxInner<'a> {
    key: &'a AesExpandedKey,
}

fn err(e: ::symcrypt::errors::SymCryptError, op: &'static str) -> Aes256CbcError {
    Aes256CbcError(crate::BackendError::SymCryptError(e, op))
}

impl Aes256CbcInner {
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256CbcError> {
        let expanded = AesExpandedKey::new(key).map_err(|e| err(e, "expanding AES key"))?;
        Ok(Aes256CbcInner { key: expanded })
    }

    pub fn enc_ctx(&self) -> Result<Aes256CbcEncCtxInner<'_>, Aes256CbcError> {
        Ok(Aes256CbcEncCtxInner { key: &self.key })
    }

    pub fn dec_ctx(&self) -> Result<Aes256CbcDecCtxInner<'_>, Aes256CbcError> {
        Ok(Aes256CbcDecCtxInner { key: &self.key })
    }
}

impl Aes256CbcEncCtxInner<'_> {
    pub fn cipher(&mut self, iv: &[u8; IV_LEN], data: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
        let mut chaining_value = *iv;
        let mut output = vec![0u8; data.len()];
        self.key
            .aes_cbc_encrypt(&mut chaining_value, data, &mut output)
            .map_err(|e| err(e, "encrypting data"))?;
        Ok(output)
    }
}

impl Aes256CbcDecCtxInner<'_> {
    pub fn cipher(&mut self, iv: &[u8; IV_LEN], data: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
        let mut chaining_value = *iv;
        let mut output = vec![0u8; data.len()];
        self.key
            .aes_cbc_decrypt(&mut chaining_value, data, &mut output)
            .map_err(|e| err(e, "decrypting data"))?;
        Ok(output)
    }
}
