// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! XTS-AES-256 implementation using SymCrypt.

use super::KEY_LEN;
use super::XtsAes256Error;
use symcrypt::cipher::xts::XtsAesKey;

pub struct XtsAes256Inner {
    key: XtsAesKey,
    data_unit_size: u32,
}

pub struct XtsAes256EncCtxInner<'a> {
    inner: &'a XtsAes256Inner,
}

pub struct XtsAes256DecCtxInner<'a> {
    inner: &'a XtsAes256Inner,
}

fn err(e: symcrypt::errors::SymCryptError, op: &'static str) -> XtsAes256Error {
    XtsAes256Error(crate::BackendError::SymCrypt(e, op))
}

impl XtsAes256Inner {
    pub fn new(key: &[u8; KEY_LEN], data_unit_size: u32) -> Result<Self, XtsAes256Error> {
        let key = XtsAesKey::new(key).map_err(|e| err(e, "expanding xts key"))?;
        Ok(Self {
            key,
            data_unit_size,
        })
    }

    pub fn enc_ctx(&self) -> Result<XtsAes256EncCtxInner<'_>, XtsAes256Error> {
        Ok(XtsAes256EncCtxInner { inner: self })
    }

    pub fn dec_ctx(&self) -> Result<XtsAes256DecCtxInner<'_>, XtsAes256Error> {
        Ok(XtsAes256DecCtxInner { inner: self })
    }
}

impl XtsAes256EncCtxInner<'_> {
    pub fn cipher(&mut self, tweak: u64, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        self.inner
            .key
            .encrypt_in_place(self.inner.data_unit_size as u64, tweak, data)
            .map_err(|e| err(e, "xts encrypt"))
    }
}

impl XtsAes256DecCtxInner<'_> {
    pub fn cipher(&mut self, tweak: u64, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        self.inner
            .key
            .decrypt_in_place(self.inner.data_unit_size as u64, tweak, data)
            .map_err(|e| err(e, "xts decrypt"))
    }
}
