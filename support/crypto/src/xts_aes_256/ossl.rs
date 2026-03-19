// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub struct XtsAes256Inner {
    enc: openssl::cipher_ctx::CipherCtx,
    dec: openssl::cipher_ctx::CipherCtx,
}

pub struct XtsAes256CtxInner<'a> {
    ctx: openssl::cipher_ctx::CipherCtx,
    enc: bool,
    _dummy: &'a (),
}

fn err(err: openssl::error::ErrorStack, op: &'static str) -> XtsAes256Error {
    XtsAes256Error(crate::BackendError(err, op))
}

impl XtsAes256Inner {
    pub fn new(key: &[u8], _data_unit_size: u32) -> Result<Self, XtsAes256Error> {
        let mut enc = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating encrypt context"))?;
        enc.encrypt_init(
            Some(openssl::cipher::Cipher::aes_256_xts()),
            Some(key),
            None,
        )
        .map_err(|e| err(e, "encrypt init"))?;
        let mut dec = openssl::cipher_ctx::CipherCtx::new()
            .map_err(|e| err(e, "creating decrypt context"))?;
        dec.decrypt_init(
            Some(openssl::cipher::Cipher::aes_256_xts()),
            Some(key),
            None,
        )
        .map_err(|e| err(e, "decrypt init"))?;
        Ok(XtsAes256Inner { enc, dec })
    }

    pub fn ctx(&self, enc: bool) -> Result<XtsAes256CtxInner<'_>, XtsAes256Error> {
        let mut ctx =
            openssl::cipher_ctx::CipherCtx::new().map_err(|e| err(e, "creating cipher context"))?;
        ctx.copy(if enc { &self.enc } else { &self.dec })
            .map_err(|e| err(e, "copying cipher context"))?;
        Ok(XtsAes256CtxInner {
            ctx,
            enc,
            _dummy: &(),
        })
    }
}

impl XtsAes256CtxInner<'_> {
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        let iv = tweak.to_le_bytes();
        if self.enc {
            self.ctx
                .encrypt_init(None, None, Some(&iv))
                .map_err(|e| err(e, "encryption"))?;
        } else {
            self.ctx
                .decrypt_init(None, None, Some(&iv))
                .map_err(|e| err(e, "decryption"))?;
        }
        self.ctx
            .cipher_update_inplace(data, data.len())
            .map_err(|e| err(e, "cipher update"))?;
        Ok(())
    }
}
