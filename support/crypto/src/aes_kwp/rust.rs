// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES key wrap with padding (RFC 5649) using the `aes-kw` RustCrypto crate.

use super::AesKeyWrapError;
use aes_kw::KwpAes128;
use aes_kw::KwpAes192;
use aes_kw::KwpAes256;
use aes_kw::cipher::KeyInit;

pub enum AesKeyWrapInner {
    Aes128(KwpAes128),
    Aes192(KwpAes192),
    Aes256(KwpAes256),
}

pub struct AesKeyWrapCtxInner<'a>(&'a AesKeyWrapInner);

pub struct AesKeyUnwrapCtxInner<'a>(&'a AesKeyWrapInner);

impl AesKeyWrapInner {
    pub fn new(key: &[u8]) -> Result<Self, AesKeyWrapError> {
        Ok(match key.len() {
            16 => Self::Aes128(KwpAes128::new_from_slice(key).unwrap()),
            24 => Self::Aes192(KwpAes192::new_from_slice(key).unwrap()),
            32 => Self::Aes256(KwpAes256::new_from_slice(key).unwrap()),
            n => return Err(AesKeyWrapError::InvalidKeySize(n)),
        })
    }

    pub fn wrap_ctx(&self) -> Result<AesKeyWrapCtxInner<'_>, AesKeyWrapError> {
        Ok(AesKeyWrapCtxInner(self))
    }

    pub fn unwrap_ctx(&self) -> Result<AesKeyUnwrapCtxInner<'_>, AesKeyWrapError> {
        Ok(AesKeyUnwrapCtxInner(self))
    }
}

fn err(e: aes_kw::Error, op: &'static str) -> AesKeyWrapError {
    AesKeyWrapError::Backend(e.to_string(), op)
}

impl AesKeyWrapCtxInner<'_> {
    pub fn wrap(&mut self, payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        let mut out = vec![0u8; payload.len() + 16];
        let written_len = match self.0 {
            AesKeyWrapInner::Aes128(k) => k.wrap_key(payload, &mut out),
            AesKeyWrapInner::Aes192(k) => k.wrap_key(payload, &mut out),
            AesKeyWrapInner::Aes256(k) => k.wrap_key(payload, &mut out),
        }
        .map_err(|e| err(e, "wrapping key"))?
        .len();
        out.truncate(written_len);
        Ok(out)
    }
}

impl AesKeyUnwrapCtxInner<'_> {
    pub fn unwrap(&mut self, wrapped_payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        let mut out = vec![0u8; wrapped_payload.len() + 16];
        let written_len = match self.0 {
            AesKeyWrapInner::Aes128(k) => k.unwrap_key(wrapped_payload, &mut out),
            AesKeyWrapInner::Aes192(k) => k.unwrap_key(wrapped_payload, &mut out),
            AesKeyWrapInner::Aes256(k) => k.unwrap_key(wrapped_payload, &mut out),
        }
        .map_err(|e| err(e, "unwrapping key"))?
        .len();
        out.truncate(written_len);
        Ok(out)
    }
}
