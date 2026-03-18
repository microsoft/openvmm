// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: calling bcrypt APIs
#![expect(unsafe_code)]

use super::*;
use std::sync::LazyLock;
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS;

#[derive(Clone)]
struct AlgHandle(BCRYPT_ALG_HANDLE);
// SAFETY: the handle can be sent across threads.
unsafe impl Send for AlgHandle {}
// SAFETY: the handle can be shared across threads.
unsafe impl Sync for AlgHandle {}

pub struct XtsAes256Inner {
    key: Key,
}

pub struct XtsAes256CtxInner<'a> {
    key: &'a Key,
    enc: bool,
}

struct Key(BCRYPT_KEY_HANDLE);
// SAFETY: the handle can be sent across threads.
unsafe impl Send for Key {}
// SAFETY: the handle can be shared across threads.
unsafe impl Sync for Key {}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: handle is valid and not aliased
        let _ = unsafe { windows::Win32::Security::Cryptography::BCryptDestroyKey(self.0) };
    }
}

static XTS_AES_256: LazyLock<Result<AlgHandle, XtsAes256Error>> = LazyLock::new(|| {
    let mut handle = BCRYPT_ALG_HANDLE::default();
    // SAFETY: no safety requirements
    unsafe {
        windows::Win32::Security::Cryptography::BCryptOpenAlgorithmProvider(
            &mut handle,
            windows::Win32::Security::Cryptography::BCRYPT_XTS_AES_ALGORITHM,
            windows::Win32::Security::Cryptography::MS_PRIMITIVE_PROVIDER,
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
    }
    .ok()
    .map_err(|e| err(e, "open algorithm provider"))
    .map(|_| AlgHandle(handle))
});

fn err(err: windows_result::Error, op: &'static str) -> XtsAes256Error {
    XtsAes256Error(crate::BackendError(err, op))
}

impl XtsAes256Inner {
    pub fn new(key: &[u8], data_unit_size: u32) -> Result<Self, XtsAes256Error> {
        let key = {
            let mut handle = BCRYPT_KEY_HANDLE::default();
            // SAFETY: the algorithm handle is valid.
            unsafe {
                windows::Win32::Security::Cryptography::BCryptGenerateSymmetricKey(
                    (*XTS_AES_256).clone()?.0,
                    &mut handle,
                    None,
                    key,
                    0,
                )
            }
            .ok()
            .map_err(|e| err(e, "generate symmetric key"))?;
            Key(handle)
        };

        // SAFETY: the key handle is valid.
        unsafe {
            windows::Win32::Security::Cryptography::BCryptSetProperty(
                BCRYPT_HANDLE(key.0.0),
                windows::Win32::Security::Cryptography::BCRYPT_MESSAGE_BLOCK_LENGTH,
                &data_unit_size.to_ne_bytes(),
                0,
            )
        }
        .ok()
        .map_err(|e| err(e, "set message block length"))?;

        Ok(XtsAes256Inner { key })
    }

    pub fn ctx(&self, enc: bool) -> Result<XtsAes256CtxInner<'_>, XtsAes256Error> {
        Ok(XtsAes256CtxInner {
            key: &self.key,
            enc,
        })
    }
}

impl XtsAes256CtxInner<'_> {
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), XtsAes256Error> {
        // BCrypt only supports 64-bit tweaks, internally padding out the high 8
        // bytes with zeroes. (Why?) This is fine for our purposes but it's a
        // bit annoying.
        let mut iv = u64::try_from(tweak)
            .map_err(|_| XtsAes256Error(crate::BackendError(E_INVALIDARG.into(), "convert tweak")))?
            .to_le_bytes();

        // TODO: fix windows crate to allow aliased input and output, as
        // allowed by the API.
        let input = data.to_vec();
        let mut n = 0;

        // SAFETY: key and buffers are valid for the duration of the calls
        unsafe {
            if self.enc {
                windows::Win32::Security::Cryptography::BCryptEncrypt(
                    self.key.0,
                    Some(&input),
                    None,
                    Some(&mut iv),
                    Some(data),
                    &mut n,
                    windows::Win32::Security::Cryptography::BCRYPT_FLAGS(0),
                )
                .ok()
                .map_err(|e| err(e, "encrypt"))
            } else {
                windows::Win32::Security::Cryptography::BCryptDecrypt(
                    self.key.0,
                    Some(&input),
                    None,
                    Some(&mut iv),
                    Some(data),
                    &mut n,
                    windows::Win32::Security::Cryptography::BCRYPT_FLAGS(0),
                )
                .ok()
                .map_err(|e| err(e, "decrypt"))
            }
        }?;
        assert_eq!(n as usize, data.len());
        Ok(())
    }
}
