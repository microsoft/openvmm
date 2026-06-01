// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES-256-GCM implementation using Windows Bcrypt APIs.

use super::*;
use crate::win::*;
use std::sync::LazyLock;
use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS;
use zerocopy::IntoBytes;

static AES_256_GCM: LazyLock<Result<AlgHandle, Aes256GcmError>> = LazyLock::new(|| {
    const CHAINING_MODE: &[u16] = wchar::wchz!("ChainingModeGCM");
    let mut handle = BCRYPT_ALG_HANDLE::default();

    // SAFETY: Errors are handled before the handle is used, and the handle is closed on drop.
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
        .map_err(|e| err(e, "setting GCM Property"))?;

        Ok(handle)
    }
});

fn err(err: windows_result::Error, op: &'static str) -> Aes256GcmError {
    Aes256GcmError(crate::BackendError(err, op))
}

pub struct Aes256GcmInner {
    key: KeyHandle,
}

pub struct Aes256GcmEncCtxInner<'a> {
    key: &'a KeyHandle,
}

pub struct Aes256GcmDecCtxInner<'a> {
    key: &'a KeyHandle,
}

impl Aes256GcmInner {
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256GcmError> {
        let mut handle = BCRYPT_KEY_HANDLE::default();
        // SAFETY: the algorithm handle is valid.
        let key = unsafe {
            windows::Win32::Security::Cryptography::BCryptImportKey(
                AES_256_GCM.as_ref().map_err(|e| e.clone())?.0,
                None,
                windows::Win32::Security::Cryptography::BCRYPT_KEY_DATA_BLOB,
                &mut handle,
                None,
                KeyBlob32::new(key).as_bytes(),
                0,
            )
        }
        .ok()
        .map(|()| KeyHandle(handle))
        .map_err(|e| err(e, "importing key"))?;

        Ok(Aes256GcmInner { key })
    }

    pub fn enc_ctx(&self) -> Result<Aes256GcmEncCtxInner<'_>, Aes256GcmError> {
        Ok(Aes256GcmEncCtxInner { key: &self.key })
    }

    pub fn dec_ctx(&self) -> Result<Aes256GcmDecCtxInner<'_>, Aes256GcmError> {
        Ok(Aes256GcmDecCtxInner { key: &self.key })
    }
}

impl Aes256GcmEncCtxInner<'_> {
    pub fn cipher(
        &mut self,
        iv: &[u8; IV_LEN],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let mut crypted_len = 0;
        let mut iv_buffer = iv.to_vec();
        let mut nonce_buffer = iv.to_vec();
        let mut crypted_data = vec![0; data.len()];

        let mut auth_mode = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
            cbSize: size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
            dwInfoVersion: windows::Win32::Security::Cryptography::BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
            pbNonce: nonce_buffer.as_mut_ptr(),
            cbNonce: nonce_buffer.len() as u32,
            pbTag: tag.as_mut_ptr(),
            cbTag: tag.len() as u32,
            ..Default::default()
        };

        // SAFETY: key and buffers are valid for the duration of the call
        unsafe {
            windows::Win32::Security::Cryptography::BCryptEncrypt(
                self.key.0,
                Some(data),
                Some(std::ptr::from_mut(&mut auth_mode).cast()),
                Some(&mut iv_buffer),
                Some(&mut crypted_data),
                &mut crypted_len,
                windows::Win32::Security::Cryptography::BCRYPT_FLAGS(0),
            )
            .ok()
            .map_err(|e| err(e, "encrypt"))
        }?;
        assert_eq!(crypted_len as usize, data.len());
        Ok(crypted_data)
    }
}

impl Aes256GcmDecCtxInner<'_> {
    pub fn cipher(
        &mut self,
        iv: &[u8; IV_LEN],
        data: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let mut crypted_len = 0;
        let mut iv_buffer = iv.to_vec();
        let mut nonce_buffer = iv.to_vec();
        let mut crypted_data = vec![0; data.len()];
        let mut tag_buffer = tag.to_vec();

        let mut auth_mode = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
            cbSize: size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
            dwInfoVersion: windows::Win32::Security::Cryptography::BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
            pbNonce: nonce_buffer.as_mut_ptr(),
            cbNonce: nonce_buffer.len() as u32,
            pbTag: tag_buffer.as_mut_ptr(),
            cbTag: tag_buffer.len() as u32,
            ..Default::default()
        };

        // SAFETY: key and buffers are valid for the duration of the call
        unsafe {
            windows::Win32::Security::Cryptography::BCryptDecrypt(
                self.key.0,
                Some(data),
                Some(std::ptr::from_mut(&mut auth_mode).cast()),
                Some(&mut iv_buffer),
                Some(&mut crypted_data),
                &mut crypted_len,
                windows::Win32::Security::Cryptography::BCRYPT_FLAGS(0),
            )
            .ok()
            .map_err(|e| err(e, "decrypt"))
        }?;
        assert_eq!(crypted_len as usize, data.len());
        Ok(crypted_data)
    }
}
