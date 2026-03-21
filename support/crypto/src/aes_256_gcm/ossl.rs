// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub struct Aes256GcmInner {
    enc: openssl::cipher_ctx::CipherCtx,
    dec: openssl::cipher_ctx::CipherCtx,
}

pub struct Aes256GcmCtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    enc: bool,
    _dummy: &'a (),
}

fn err(err: openssl::error::ErrorStack, op: &'static str) -> Aes256GcmError {
    Aes256GcmError(crate::BackendError(err, op))
}

impl Aes256GcmInner {
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256GcmError> {
        let mut enc = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating encrypt context"))?;
        enc.encrypt_init(
            Some(openssl::cipher::Cipher::aes_256_gcm()),
            Some(key),
            None,
        )
        .map_err(|e| err(e, "encrypt init"))?;
        let mut dec = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating decrypt context"))?;
        dec.decrypt_init(
            Some(openssl::cipher::Cipher::aes_256_gcm()),
            Some(key),
            None,
        )
        .map_err(|e| err(e, "decrypt init"))?;
        Ok(Aes256GcmInner { enc, dec })
    }

    pub fn ctx(&self, enc: bool) -> Result<Aes256GcmCtxInner<'_>, Aes256GcmError> {
        let mut ctx =
            openssl::cipher_ctx::CipherCtx::new().map_err(|e| err(e, "creating cipher context"))?;
        ctx.copy(if enc { &self.enc } else { &self.dec })
            .map_err(|e| err(e, "copying cipher context"))?;
        Ok(Aes256GcmCtxInner {
            ctx,
            enc,
            _dummy: &(),
        })
    }
}

impl Aes256GcmCtxInner<'_> {
    pub fn cipher(
        &mut self,
        iv: &[u8],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let mut output = vec![0u8; data.len()];
        if self.enc {
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
        } else {
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
        }
        Ok(output)
    }
}
