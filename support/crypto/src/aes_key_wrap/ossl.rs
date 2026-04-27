// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::AesKeyWrapError;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> AesKeyWrapError {
    AesKeyWrapError::Backend(crate::BackendError(err, op))
}

fn openssl_cipher(key_len: usize) -> Result<&'static openssl::cipher::CipherRef, AesKeyWrapError> {
    match key_len {
        16 => Ok(openssl::cipher::Cipher::aes_128_wrap_pad()),
        24 => Ok(openssl::cipher::Cipher::aes_192_wrap_pad()),
        32 => Ok(openssl::cipher::Cipher::aes_256_wrap_pad()),
        key_size => Err(AesKeyWrapError::InvalidKeySize(key_size)),
    }
}

pub struct AesKeyWrapInner {
    key: Vec<u8>,
}

pub struct AesKeyWrapCtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    _dummy: &'a (),
}

pub struct AesKeyUnwrapCtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    _dummy: &'a (),
}

impl AesKeyWrapInner {
    pub fn new(key: &[u8]) -> Result<Self, AesKeyWrapError> {
        // Validate key size early.
        openssl_cipher(key.len())?;
        Ok(AesKeyWrapInner { key: key.to_vec() })
    }

    pub fn wrap_ctx(&self) -> Result<AesKeyWrapCtxInner<'_>, AesKeyWrapError> {
        let cipher = openssl_cipher(self.key.len())?;
        let mut ctx =
            openssl::cipher_ctx::CipherCtx::new().map_err(|e| err(e, "creating wrap context"))?;
        ctx.set_flags(openssl::cipher_ctx::CipherCtxFlags::FLAG_WRAP_ALLOW);
        ctx.encrypt_init(Some(cipher), Some(&self.key), None)
            .map_err(|e| err(e, "wrap init"))?;
        Ok(AesKeyWrapCtxInner { ctx, _dummy: &() })
    }

    pub fn unwrap_ctx(&self) -> Result<AesKeyUnwrapCtxInner<'_>, AesKeyWrapError> {
        let cipher = openssl_cipher(self.key.len())?;
        let mut ctx =
            openssl::cipher_ctx::CipherCtx::new().map_err(|e| err(e, "creating unwrap context"))?;
        ctx.set_flags(openssl::cipher_ctx::CipherCtxFlags::FLAG_WRAP_ALLOW);
        ctx.decrypt_init(Some(cipher), Some(&self.key), None)
            .map_err(|e| err(e, "unwrap init"))?;
        Ok(AesKeyUnwrapCtxInner { ctx, _dummy: &() })
    }
}

impl AesKeyWrapCtxInner<'_> {
    pub fn wrap(&mut self, payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        let padding = 8 - payload.len() % 8;
        let mut output = vec![0; payload.len() + padding + 16];
        let count = self
            .ctx
            .cipher_update(payload, Some(&mut output))
            .map_err(|e| err(e, "wrapping key"))?;
        // DEVNOTE: Skip the `cipher_final()`, which is effectively a no-op for this operation
        // according to OpenSSL implementation.
        output.truncate(count);
        Ok(output)
    }
}

impl AesKeyUnwrapCtxInner<'_> {
    pub fn unwrap(&mut self, wrapped_payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        let mut output = vec![0; wrapped_payload.len() + 16];
        let count = self
            .ctx
            .cipher_update(wrapped_payload, Some(&mut output))
            .map_err(|e| err(e, "unwrapping key"))?;
        // DEVNOTE: Skip the `cipher_final()`, which is effectively a no-op for this operation
        // according to OpenSSL implementation.
        output.truncate(count);
        Ok(output)
    }
}
