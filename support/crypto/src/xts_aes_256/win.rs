// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Calling BCrypt APIs.
#![expect(unsafe_code)]

use super::*;
use std::sync::OnceLock;
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Foundation::NTSTATUS;
use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS;

pub struct XtsAes256Inner(Key);

pub struct XtsAes256CtxInner<'a> {
    key: &'a Key,
    enc: bool,
}

impl XtsAes256Inner {
    pub fn ctx(&self, enc: bool) -> Result<XtsAes256CtxInner<'_>, XtsAes256Error> {
        Ok(XtsAes256CtxInner { key: &self.0, enc })
    }
}

impl XtsAes256CtxInner<'_> {
    pub fn cipher(&self, tweak: &[u8; 16], data: &mut [u8]) -> Result<(), XtsAes256Error> {
        let mut iv = u64::try_from(u128::from_le_bytes(*tweak))
            .map_err(|_| {
                XtsAes256Error(crate::BackendError(
                    windows_result::Error::from_hresult(E_INVALIDARG),
                    "convert tweak",
                ))
            })?
            .to_le_bytes();

        if self.enc {
            self.key.encrypt(&mut iv, data)
        } else {
            self.key.decrypt(&mut iv, data)
        }
    }
}

static XTS_AES_256: OnceLock<AlgHandle> = OnceLock::new();

struct AlgHandle(BCRYPT_ALG_HANDLE);

// SAFETY: the handle can be sent across threads.
unsafe impl Send for AlgHandle {}
// SAFETY: the handle can be shared across threads.
unsafe impl Sync for AlgHandle {}

fn bcrypt_result(op: &'static str, status: NTSTATUS) -> Result<(), XtsAes256Error> {
    if status.is_ok() {
        Ok(())
    } else {
        Err(XtsAes256Error(crate::BackendError(
            windows_result::Error::from(status),
            op,
        )))
    }
}

struct Key(BCRYPT_KEY_HANDLE);

// SAFETY: the handle can be sent across threads.
unsafe impl Send for Key {}
// SAFETY: the handle can be shared across threads.
unsafe impl Sync for Key {}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: handle is valid and not aliased.
        unsafe {
            bcrypt_result(
                "destroy key",
                windows::Win32::Security::Cryptography::BCryptDestroyKey(self.0),
            )
            .unwrap();
        }
    }
}

impl Key {
    fn encrypt(&self, iv: &mut [u8], data: &mut [u8]) -> Result<(), XtsAes256Error> {
        // TODO: fix windows crate to allow aliased input and output, as
        // allowed by the API.
        let input = data.to_vec();
        let mut n = 0;
        // SAFETY: key and buffers are valid for the duration of the call.
        let status = unsafe {
            windows::Win32::Security::Cryptography::BCryptEncrypt(
                self.0,
                Some(&input),
                None,
                Some(iv),
                Some(data),
                &mut n,
                windows::Win32::Security::Cryptography::BCRYPT_FLAGS(0),
            )
        };
        bcrypt_result("encrypt", status)?;
        assert_eq!(n as usize, data.len());
        Ok(())
    }

    fn decrypt(&self, iv: &mut [u8], data: &mut [u8]) -> Result<(), XtsAes256Error> {
        // TODO: fix windows crate to allow aliased input and output, as
        // allowed by the API.
        let input = data.to_vec();
        let mut n = 0;
        // SAFETY: key and buffers are valid for the duration of the call.
        let status = unsafe {
            windows::Win32::Security::Cryptography::BCryptDecrypt(
                self.0,
                Some(&input),
                None,
                Some(iv),
                Some(data),
                &mut n,
                windows::Win32::Security::Cryptography::BCRYPT_FLAGS(0),
            )
        };
        bcrypt_result("decrypt", status)?;
        assert_eq!(n as usize, data.len());
        Ok(())
    }
}

pub fn xts_aes_256(key: &[u8], data_unit_size: u32) -> Result<XtsAes256Inner, XtsAes256Error> {
    let alg = if let Some(alg) = XTS_AES_256.get() {
        alg
    } else {
        let mut handle = BCRYPT_ALG_HANDLE::default();
        // SAFETY: no safety requirements.
        let status = unsafe {
            windows::Win32::Security::Cryptography::BCryptOpenAlgorithmProvider(
                &mut handle,
                windows::Win32::Security::Cryptography::BCRYPT_XTS_AES_ALGORITHM,
                windows::Win32::Security::Cryptography::MS_PRIMITIVE_PROVIDER,
                BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
            )
        };
        bcrypt_result("open algorithm provider", status)?;
        if let Err(AlgHandle(handle)) = XTS_AES_256.set(AlgHandle(handle)) {
            // SAFETY: handle is valid and not aliased.
            unsafe {
                bcrypt_result(
                    "close algorithm provider",
                    windows::Win32::Security::Cryptography::BCryptCloseAlgorithmProvider(handle, 0),
                )
                .unwrap();
            }
        }
        XTS_AES_256.get().unwrap()
    };
    let key = {
        let mut handle = BCRYPT_KEY_HANDLE::default();
        // SAFETY: the algorithm handle is valid.
        let status = unsafe {
            windows::Win32::Security::Cryptography::BCryptGenerateSymmetricKey(
                alg.0,
                &mut handle,
                None,
                key,
                0,
            )
        };
        bcrypt_result("generate symmetric key", status)?;
        Key(handle)
    };

    // SAFETY: the key handle is valid.
    let status = unsafe {
        windows::Win32::Security::Cryptography::BCryptSetProperty(
            BCRYPT_HANDLE(key.0.0),
            windows::Win32::Security::Cryptography::BCRYPT_MESSAGE_BLOCK_LENGTH,
            &data_unit_size.to_ne_bytes(),
            0,
        )
    };
    bcrypt_result("set message block length", status)?;

    Ok(XtsAes256Inner(key))
}
