// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use ::symcrypt::cipher::BlockCipherType;
use ::symcrypt::errors::SymCryptError;
use ::symcrypt::gcm::GcmExpandedKey;

pub struct Aes256GcmInner {
    key: GcmExpandedKey,
}

pub struct Aes256GcmEncCtxInner<'a> {
    key: &'a GcmExpandedKey,
}

pub struct Aes256GcmDecCtxInner<'a> {
    key: &'a GcmExpandedKey,
}

fn err(e: SymCryptError, op: &'static str) -> Aes256GcmError {
    Aes256GcmError(crate::BackendError(e, op))
}

fn nonce(iv: &[u8], op: &'static str) -> Result<[u8; 12], Aes256GcmError> {
    iv.try_into()
        .map_err(|_| err(SymCryptError::WrongNonceSize, op))
}

impl Aes256GcmInner {
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256GcmError> {
        let key = GcmExpandedKey::new(key, BlockCipherType::AesBlock)
            .map_err(|e| err(e, "expanding gcm key"))?;
        Ok(Self { key })
    }

    pub fn enc_ctx(&self) -> Result<Aes256GcmEncCtxInner<'_>, Aes256GcmError> {
        Ok(Aes256GcmEncCtxInner { key: &self.key })
    }

    pub fn dec_ctx(&self) -> Result<Aes256GcmDecCtxInner<'_>, Aes256GcmError> {
        Ok(Aes256GcmDecCtxInner { key: &self.key })
    }
}

impl Aes256GcmEncCtxInner<'_> {
    pub fn cipher(
        &mut self,
        iv: &[u8],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let nonce = nonce(iv, "setting iv for encryption")?;
        let mut output = data.to_vec();
        self.key.encrypt_in_place(&nonce, &[], &mut output, tag);
        Ok(output)
    }
}

impl Aes256GcmDecCtxInner<'_> {
    pub fn cipher(
        &mut self,
        iv: &[u8],
        data: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let nonce = nonce(iv, "setting iv for decryption")?;
        let mut output = data.to_vec();
        self.key
            .decrypt_in_place(&nonce, &[], &mut output, tag)
            .map_err(|e| err(e, "decrypting data"))?;
        Ok(output)
    }
}
