// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub struct Aes256GcmInner {
    key: [u8; KEY_LEN],
}

pub struct Aes256GcmEncCtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    _dummy: &'a (),
}

pub struct Aes256GcmDecCtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    _dummy: &'a (),
}

fn err(err: openssl::error::ErrorStack, op: &'static str) -> Aes256GcmError {
    Aes256GcmError(crate::BackendError(err, op))
}

impl Aes256GcmInner {
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256GcmError> {
        Ok(Aes256GcmInner { key: *key })
    }

    pub fn enc_ctx(&self) -> Result<Aes256GcmEncCtxInner<'_>, Aes256GcmError> {
        let mut ctx = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating encrypt context"))?;
        ctx.encrypt_init(
            Some(openssl::cipher::Cipher::aes_256_gcm()),
            Some(&self.key),
            None,
        )
        .map_err(|e| err(e, "encrypt init"))?;
        Ok(Aes256GcmEncCtxInner { ctx, _dummy: &() })
    }

    pub fn dec_ctx(&self) -> Result<Aes256GcmDecCtxInner<'_>, Aes256GcmError> {
        let mut ctx = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating decrypt context"))?;
        ctx.decrypt_init(
            Some(openssl::cipher::Cipher::aes_256_gcm()),
            Some(&self.key),
            None,
        )
        .map_err(|e| err(e, "decrypt init"))?;
        Ok(Aes256GcmDecCtxInner { ctx, _dummy: &() })
    }
}

impl Aes256GcmEncCtxInner<'_> {
    pub fn cipher(
        &mut self,
        iv: &[u8; IV_LEN],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let mut output = vec![0u8; data.len()];
        self.ctx
            .encrypt_init(None, None, Some(iv))
            .map_err(|e| err(e, "setting iv for encryption"))?;
        let count = self
            .ctx
            .cipher_update(data, Some(&mut output))
            .map_err(|e| err(e, "encrypting data"))?;
        let rest = self
            .ctx
            .cipher_final(&mut output[count..])
            .map_err(|e| err(e, "finalizing encryption"))?;
        output.truncate(count + rest);
        self.ctx
            .tag(tag)
            .map_err(|e| err(e, "getting authentication tag"))?;
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
        let mut output = vec![0u8; data.len()];
        self.ctx
            .decrypt_init(None, None, Some(iv))
            .map_err(|e| err(e, "setting iv for decryption"))?;
        let count = self
            .ctx
            .cipher_update(data, Some(&mut output))
            .map_err(|e| err(e, "decrypting data"))?;
        self.ctx
            .set_tag(tag)
            .map_err(|e| err(e, "setting authentication tag"))?;
        let rest = self
            .ctx
            .cipher_final(&mut output[count..])
            .map_err(|e| err(e, "finalizing decryption"))?;
        output.truncate(count + rest);
        Ok(output)
    }
}
