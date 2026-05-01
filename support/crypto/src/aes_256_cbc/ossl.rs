// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub struct Aes256CbcInner {
    key: [u8; KEY_LEN],
}

pub struct Aes256CbcEncCtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    _dummy: &'a (),
}

pub struct Aes256CbcDecCtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    _dummy: &'a (),
}

fn err(err: openssl::error::ErrorStack, op: &'static str) -> Aes256CbcError {
    Aes256CbcError(crate::BackendError(err, op))
}

impl Aes256CbcInner {
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256CbcError> {
        Ok(Aes256CbcInner { key: *key })
    }

    pub fn enc_ctx(&self) -> Result<Aes256CbcEncCtxInner<'_>, Aes256CbcError> {
        let mut ctx = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating encrypt context"))?;
        ctx.encrypt_init(
            Some(openssl::cipher::Cipher::aes_256_cbc()),
            Some(&self.key),
            None,
        )
        .map_err(|e| err(e, "encrypt init"))?;
        ctx.set_padding(false);
        Ok(Aes256CbcEncCtxInner { ctx, _dummy: &() })
    }

    pub fn dec_ctx(&self) -> Result<Aes256CbcDecCtxInner<'_>, Aes256CbcError> {
        let mut ctx = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating decrypt context"))?;
        ctx.decrypt_init(
            Some(openssl::cipher::Cipher::aes_256_cbc()),
            Some(&self.key),
            None,
        )
        .map_err(|e| err(e, "decrypt init"))?;
        ctx.set_padding(false);
        Ok(Aes256CbcDecCtxInner { ctx, _dummy: &() })
    }
}

impl Aes256CbcEncCtxInner<'_> {
    pub fn cipher(&mut self, iv: &[u8; IV_LEN], data: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
        let mut output =
            vec![0u8; data.len() + openssl::cipher::Cipher::aes_256_cbc().block_size()]; // block size padding room
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
        Ok(output)
    }
}

impl Aes256CbcDecCtxInner<'_> {
    pub fn cipher(&mut self, iv: &[u8; IV_LEN], data: &[u8]) -> Result<Vec<u8>, Aes256CbcError> {
        let mut output =
            vec![0u8; data.len() + openssl::cipher::Cipher::aes_256_cbc().block_size()]; // block size padding room
        self.ctx
            .decrypt_init(None, None, Some(iv))
            .map_err(|e| err(e, "setting iv for decryption"))?;
        let count = self
            .ctx
            .cipher_update(data, Some(&mut output))
            .map_err(|e| err(e, "decrypting data"))?;
        let rest = self
            .ctx
            .cipher_final(&mut output[count..])
            .map_err(|e| err(e, "finalizing decryption"))?;
        output.truncate(count + rest);
        Ok(output)
    }
}
