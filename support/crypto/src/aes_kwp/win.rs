// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES key wrap with padding (RFC 5649) implementation using Windows
//! BCrypt APIs. BCrypt does not expose KWP directly, so this implements
//! RFC 5649 on top of a single-block AES-ECB primitive.

use super::AesKeyWrapError;
use super::AesKeyWrapErrorInner;
use super::kwp;
use crate::win::*;
use std::sync::LazyLock;
use windows::Win32::Foundation::NTE_BAD_DATA;
use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_FLAGS;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS;
use zerocopy::IntoBytes;

static AES_ECB: LazyLock<Result<AlgHandle, AesKeyWrapError>> = LazyLock::new(|| {
    const CHAINING_MODE: &[u16] = wchar::wchz!("ChainingModeECB");
    let mut handle = BCRYPT_ALG_HANDLE::default();

    // SAFETY: errors are handled before the handle is used; the handle is
    // closed on drop via `AlgHandle`.
    unsafe {
        let handle = windows::Win32::Security::Cryptography::BCryptOpenAlgorithmProvider(
            &mut handle,
            windows::Win32::Security::Cryptography::BCRYPT_AES_ALGORITHM,
            None,
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .ok()
        .map(|()| AlgHandle(handle))
        .map_err(|e| err(e, "open algorithm provider"))?;

        windows::Win32::Security::Cryptography::BCryptSetProperty(
            handle.0.into(),
            windows::Win32::Security::Cryptography::BCRYPT_CHAINING_MODE,
            CHAINING_MODE.as_bytes(),
            0,
        )
        .ok()
        .map_err(|e| err(e, "setting ECB property"))?;

        Ok(handle)
    }
});

fn err(err: windows_result::Error, op: &'static str) -> AesKeyWrapError {
    AesKeyWrapError(AesKeyWrapErrorInner::Backend(crate::BackendError(err, op)))
}

fn import_key(key: &[u8]) -> Result<KeyHandle, AesKeyWrapError> {
    use windows::Win32::Security::Cryptography as wc;
    // Build a BCRYPT_KEY_DATA_BLOB inline so we can support 16/24/32-byte keys.
    let mut blob = Vec::with_capacity(12 + key.len());
    blob.extend_from_slice(wc::BCRYPT_KEY_DATA_BLOB_MAGIC.to_ne_bytes().as_slice());
    blob.extend_from_slice(wc::BCRYPT_KEY_DATA_BLOB_VERSION1.to_ne_bytes().as_slice());
    blob.extend_from_slice((key.len() as u32).to_ne_bytes().as_slice());
    blob.extend_from_slice(key);

    let mut handle = BCRYPT_KEY_HANDLE::default();
    // SAFETY: algorithm handle is valid; the blob lives for the duration of the call.
    unsafe {
        wc::BCryptImportKey(
            AES_ECB.as_ref().map_err(|e| e.clone())?.0,
            None,
            wc::BCRYPT_KEY_DATA_BLOB,
            &mut handle,
            None,
            &blob,
            0,
        )
        .ok()
        .map(|()| KeyHandle(handle))
        .map_err(|e| err(e, "importing key"))
    }
}

fn ecb_block(
    key: &KeyHandle,
    op: u32,
    block: [u8; kwp::AES_BLOCK_LEN],
) -> Result<[u8; kwp::AES_BLOCK_LEN], AesKeyWrapError> {
    let mut out = [0u8; kwp::AES_BLOCK_LEN];
    let mut out_len = 0u32;
    // SAFETY: key/buffers are valid for the duration of the call.
    let res = unsafe {
        if op == 0 {
            windows::Win32::Security::Cryptography::BCryptEncrypt(
                key.0,
                Some(&block),
                None,
                None,
                Some(&mut out),
                &mut out_len,
                BCRYPT_FLAGS(0),
            )
        } else {
            windows::Win32::Security::Cryptography::BCryptDecrypt(
                key.0,
                Some(&block),
                None,
                None,
                Some(&mut out),
                &mut out_len,
                BCRYPT_FLAGS(0),
            )
        }
    };
    res.ok().map_err(|e| {
        err(
            e,
            if op == 0 {
                "AES-ECB encrypt"
            } else {
                "AES-ECB decrypt"
            },
        )
    })?;
    Ok(out)
}

pub struct AesKeyWrapInner {
    key: KeyHandle,
}

pub struct AesKeyWrapCtxInner<'a> {
    key: &'a KeyHandle,
}

pub struct AesKeyUnwrapCtxInner<'a> {
    key: &'a KeyHandle,
}

impl AesKeyWrapInner {
    pub fn new(key: &[u8]) -> Result<Self, AesKeyWrapError> {
        match key.len() {
            16 | 24 | 32 => {}
            n => {
                return Err(AesKeyWrapError(AesKeyWrapErrorInner::InvalidKeySize(n)));
            }
        }
        Ok(AesKeyWrapInner {
            key: import_key(key)?,
        })
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
        kwp::wrap(payload, |block| ecb_block(self.key, 0, block))
    }
}

impl AesKeyUnwrapCtxInner<'_> {
    pub fn unwrap(&mut self, wrapped: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        match kwp::unwrap(wrapped, |block| ecb_block(self.key, 1, block))? {
            Some(v) => Ok(v),
            None => Err(err(
                windows_result::Error::from_hresult(NTE_BAD_DATA),
                "AES key unwrap integrity check",
            )),
        }
    }
}
