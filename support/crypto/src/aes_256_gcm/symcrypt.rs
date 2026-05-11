// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
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

fn err(e: ::symcrypt::errors::SymCryptError, op: &'static str) -> Aes256GcmError {
    Aes256GcmError(crate::BackendError::SymCrypt(e, op))
}

impl Aes256GcmInner {
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256GcmError> {
        let key = GcmExpandedKey::new(key, ::symcrypt::cipher::BlockCipherType::AesBlock)
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
        iv: &[u8; IV_LEN],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let mut output = data.to_vec();
        self.key.encrypt_in_place(iv, &[], &mut output, tag);
        Ok(output)
    }
}

impl Aes256GcmDecCtxInner<'_> {
    pub fn cipher(
        &mut self,
        iv: &[u8; IV_LEN],
        data: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let mut output = data.to_vec();
        self.key
            .decrypt_in_place(iv, &[], &mut output, tag)
            .map_err(|e| err(e, "decrypting data"))?;
        Ok(output)
    }
}
