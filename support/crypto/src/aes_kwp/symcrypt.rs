// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES key wrap with padding (RFC 5649) implementation using SymCrypt.

use super::AesKeyWrapError;
use super::AesKeyWrapErrorInner;
use symcrypt::cipher::aes_kw::AesKwpKey;

fn err(e: symcrypt::errors::SymCryptError, op: &'static str) -> AesKeyWrapError {
    AesKeyWrapError(AesKeyWrapErrorInner::Backend(
        crate::BackendError::SymCrypt(e, op),
    ))
}

pub struct AesKeyWrapInner {
    key: AesKwpKey,
}

pub struct AesKeyWrapCtxInner<'a> {
    key: &'a AesKwpKey,
}

pub struct AesKeyUnwrapCtxInner<'a> {
    key: &'a AesKwpKey,
}

impl AesKeyWrapInner {
    pub fn new(key: &[u8]) -> Result<Self, AesKeyWrapError> {
        match key.len() {
            16 | 24 | 32 => {}
            key_size => {
                return Err(AesKeyWrapError(AesKeyWrapErrorInner::InvalidKeySize(
                    key_size,
                )));
            }
        }
        let key = AesKwpKey::new(key).map_err(|e| err(e, "expanding kwp key"))?;
        Ok(AesKeyWrapInner { key })
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
        self.key
            .encrypt(payload)
            .map_err(|e| err(e, "wrapping key"))
    }
}

impl AesKeyUnwrapCtxInner<'_> {
    pub fn unwrap(&mut self, wrapped_payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        self.key
            .decrypt(wrapped_payload)
            .map_err(|e| err(e, "unwrapping key"))
    }
}
