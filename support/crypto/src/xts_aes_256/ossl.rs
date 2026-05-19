// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! XTS-AES-256 implementation using OpenSSL.

use super::*;

pub struct XtsAes256Inner {
    key: [u8; KEY_LEN],
}

pub struct XtsAes256EncCtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    _dummy: &'a (),
}

pub struct XtsAes256DecCtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    _dummy: &'a (),
}

fn err(err: openssl::error::ErrorStack, op: &'static str) -> XtsAes256Error {
    XtsAes256Error(crate::BackendError(err, op))
}

impl XtsAes256Inner {
    pub fn new(key: &[u8; KEY_LEN], _data_unit_size: u32) -> Result<Self, XtsAes256Error> {
        Ok(XtsAes256Inner { key: *key })
    }

    pub fn enc_ctx(&self) -> Result<XtsAes256EncCtxInner<'_>, XtsAes256Error> {
        let mut ctx = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating encrypt context"))?;
        ctx.encrypt_init(
            Some(openssl::cipher::Cipher::aes_256_xts()),
            Some(&self.key),
            None,
        )
        .map_err(|e| err(e, "encrypt init"))?;
        Ok(XtsAes256EncCtxInner { ctx, _dummy: &() })
    }

    pub fn dec_ctx(&self) -> Result<XtsAes256DecCtxInner<'_>, XtsAes256Error> {
        let mut ctx = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating decrypt context"))?;
        ctx.decrypt_init(
            Some(openssl::cipher::Cipher::aes_256_xts()),
            Some(&self.key),
            None,
        )
        .map_err(|e| err(e, "decrypt init"))?;
        Ok(XtsAes256DecCtxInner { ctx, _dummy: &() })
    }
}

impl XtsAes256EncCtxInner<'_> {
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        let iv = tweak.to_le_bytes();
        self.ctx
            .encrypt_init(None, None, Some(&iv))
            .map_err(|e| err(e, "encryption"))?;
        self.ctx
            .cipher_update_inplace(data, data.len())
            .map_err(|e| err(e, "cipher update"))?;
        Ok(())
    }
}

impl XtsAes256DecCtxInner<'_> {
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        let iv = tweak.to_le_bytes();
        self.ctx
            .decrypt_init(None, None, Some(&iv))
            .map_err(|e| err(e, "decryption"))?;
        self.ctx
            .cipher_update_inplace(data, data.len())
            .map_err(|e| err(e, "cipher update"))?;
        Ok(())
    }
}
