// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES-256-GCM authenticated encryption and decryption.

#![cfg(with_encryption)]

use thiserror::Error;

/// Error returned by AES-256-GCM operations.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Aes256GcmError(Box<Aes256GcmErrorInner>);

#[derive(Debug, Error)]
enum Aes256GcmErrorInner {
    #[cfg(all(unix, feature = "ossl"))]
    #[error("openssl error during {1}")]
    OpenSsl(#[source] openssl::error::ErrorStack, &'static str),
    #[cfg(all(windows, feature = "win"))]
    #[error("bcrypt error during {1}")]
    BCrypt(#[source] windows_result::Error, &'static str),
}

/// Encrypt `data` with AES-256-GCM.
///
/// Writes the authentication tag into `tag`. Returns the ciphertext.
pub fn encrypt(
    key: &[u8],
    iv: &[u8],
    data: &[u8],
    tag: &mut [u8],
) -> Result<Vec<u8>, Aes256GcmError> {
    sys::encrypt(key, iv, data, tag)
}

/// Decrypt `data` with AES-256-GCM.
///
/// Verifies the authentication `tag`. Returns the plaintext.
pub fn decrypt(key: &[u8], iv: &[u8], data: &[u8], tag: &[u8]) -> Result<Vec<u8>, Aes256GcmError> {
    sys::decrypt(key, iv, data, tag)
}

#[cfg(all(unix, feature = "ossl"))]
mod sys {
    use super::*;

    pub fn encrypt(
        key: &[u8],
        iv: &[u8],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        openssl::symm::encrypt_aead(
            openssl::symm::Cipher::aes_256_gcm(),
            key,
            Some(iv),
            &[],
            data,
            tag,
        )
        .map_err(|e| Aes256GcmError(Box::new(Aes256GcmErrorInner::OpenSsl(e, "encryption"))))
    }

    pub fn decrypt(
        key: &[u8],
        iv: &[u8],
        data: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        openssl::symm::decrypt_aead(
            openssl::symm::Cipher::aes_256_gcm(),
            key,
            Some(iv),
            &[],
            data,
            tag,
        )
        .map_err(|e| Aes256GcmError(Box::new(Aes256GcmErrorInner::OpenSsl(e, "decryption"))))
    }
}

#[cfg(all(windows, feature = "win"))]
mod sys {
    // UNSAFETY: Calling BCrypt APIs.
    #![expect(unsafe_code)]

    use super::*;
    use std::marker::PhantomData;
    use windows::Win32::Security::Cryptography::BCRYPT_AES_ALGORITHM;
    use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
    use windows::Win32::Security::Cryptography::BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO;
    use windows::Win32::Security::Cryptography::BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION;
    use windows::Win32::Security::Cryptography::BCRYPT_CHAINING_MODE;
    use windows::Win32::Security::Cryptography::BCRYPT_KEY_DATA_BLOB;
    use windows::Win32::Security::Cryptography::BCRYPT_KEY_DATA_BLOB_MAGIC;
    use windows::Win32::Security::Cryptography::BCRYPT_KEY_DATA_BLOB_VERSION1;
    use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
    use windows::Win32::Security::Cryptography::BCryptCloseAlgorithmProvider;
    use windows::Win32::Security::Cryptography::BCryptDecrypt;
    use windows::Win32::Security::Cryptography::BCryptDestroyKey;
    use windows::Win32::Security::Cryptography::BCryptEncrypt;
    use windows::Win32::Security::Cryptography::BCryptImportKey;
    use windows::Win32::Security::Cryptography::BCryptOpenAlgorithmProvider;
    use windows::Win32::Security::Cryptography::BCryptSetProperty;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    const KEY_SIZE: usize = 32;

    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout)]
    struct KeyBlob {
        header_magic: u32,
        header_version: u32,
        key_len: u32,
        key: [u8; KEY_SIZE],
    }

    impl KeyBlob {
        fn new(key: &[u8]) -> Result<Self, Aes256GcmError> {
            let key_a: [u8; KEY_SIZE] = key.try_into().map_err(|_| {
                Aes256GcmError(Box::new(Aes256GcmErrorInner::BCrypt(
                    windows_result::Error::from_win32(windows::Win32::Foundation::WIN32_ERROR(
                        0x80070057, // E_INVALIDARG
                    )),
                    "invalid key length",
                )))
            })?;

            Ok(KeyBlob {
                header_magic: BCRYPT_KEY_DATA_BLOB_MAGIC,
                header_version: BCRYPT_KEY_DATA_BLOB_VERSION1,
                key_len: KEY_SIZE as u32,
                key: key_a,
            })
        }
    }

    #[repr(transparent)]
    struct CipherModeInfoRef<'a>(BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO, PhantomData<&'a [u8]>);

    impl<'a> CipherModeInfoRef<'a> {
        pub fn new(nonce: &'a mut [u8], tag: &'a mut [u8]) -> Self {
            Self(
                BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
                    cbSize: size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
                    dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
                    cbNonce: nonce.len() as u32,
                    pbNonce: nonce.as_mut_ptr(),
                    cbTag: tag.len() as u32,
                    pbTag: tag.as_mut_ptr(),
                    ..Default::default()
                },
                PhantomData,
            )
        }

        fn get_padding_info(&self) -> *const BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
            &self.0
        }
    }

    struct KeyWrapper {
        key: BCRYPT_KEY_HANDLE,
    }

    impl Drop for KeyWrapper {
        fn drop(&mut self) {
            // SAFETY: KeyWrapper guarantees that the key handle is valid
            unsafe {
                let _ret = BCryptDestroyKey(self.key);
            }
        }
    }

    struct AlgWrapper {
        alg: BCRYPT_ALG_HANDLE,
    }

    impl Drop for AlgWrapper {
        fn drop(&mut self) {
            // SAFETY: AlgWrapper guarantees that the algorithm handle is valid
            unsafe {
                let _ret = BCryptCloseAlgorithmProvider(self.alg, 0);
            }
        }
    }

    fn import_bcrypt_key(key: &[u8]) -> Result<KeyWrapper, Aes256GcmError> {
        let mut alg = Default::default();
        let mut bcrypt_key: BCRYPT_KEY_HANDLE = Default::default();
        let key_blob = KeyBlob::new(key)?;
        let chaining_mode = "ChainingModeGCM\0".encode_utf16().collect::<Vec<u16>>();

        // SAFETY: Algorithm and key handles are properly destroyed when exiting scope
        unsafe {
            let alg_handle = {
                BCryptOpenAlgorithmProvider(
                    &mut alg,
                    BCRYPT_AES_ALGORITHM,
                    None,
                    Default::default(),
                )
                .ok()
                .map_err(|e| {
                    Aes256GcmError(Box::new(Aes256GcmErrorInner::BCrypt(
                        e,
                        "opening algorithm provider",
                    )))
                })?;
                AlgWrapper { alg }
            };

            BCryptSetProperty(
                alg_handle.alg.into(),
                BCRYPT_CHAINING_MODE,
                chaining_mode.as_bytes(),
                Default::default(),
            )
            .ok()
            .map_err(|e| {
                Aes256GcmError(Box::new(Aes256GcmErrorInner::BCrypt(
                    e,
                    "setting GCM Property",
                )))
            })?;

            BCryptImportKey(
                alg_handle.alg,
                None,
                BCRYPT_KEY_DATA_BLOB,
                &mut bcrypt_key,
                None,
                key_blob.as_bytes(),
                Default::default(),
            )
            .ok()
            .map_err(|e| {
                Aes256GcmError(Box::new(Aes256GcmErrorInner::BCrypt(e, "importing key")))
            })?;
            Ok(KeyWrapper { key: bcrypt_key })
        }
    }

    pub fn encrypt(
        key: &[u8],
        iv: &[u8],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let mut encrypted_len = 0;
        let mut iv_buffer = iv.to_vec();
        let mut nonce_buffer = iv.to_vec();

        let encrypt_key = import_bcrypt_key(key)?;
        let auth_mode = CipherModeInfoRef::new(&mut nonce_buffer, tag);

        let mut encrypted_data = vec![0; data.len()];
        // SAFETY: CipherModeInfoRef ensures the nonce and tag pointers maintain their lifetime
        unsafe {
            BCryptEncrypt(
                encrypt_key.key,
                Some(data),
                Some(auth_mode.get_padding_info().cast()),
                Some(&mut iv_buffer),
                Some(&mut encrypted_data),
                &mut encrypted_len,
                Default::default(),
            )
            .ok()
            .map_err(|e| {
                Aes256GcmError(Box::new(Aes256GcmErrorInner::BCrypt(
                    e,
                    "performing encryption",
                )))
            })?;
        }
        assert_eq!(encrypted_len as usize, encrypted_data.len());
        Ok(encrypted_data)
    }

    pub fn decrypt(
        key: &[u8],
        iv: &[u8],
        data: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        let mut decrypted_len = 0;
        let mut auth_tag: Vec<u8> = tag.to_vec();
        let mut iv_buffer: Vec<u8> = iv.to_vec();
        let mut nonce_buffer: Vec<u8> = iv.to_vec();

        let decrypt_key = import_bcrypt_key(key)?;
        let auth_mode = CipherModeInfoRef::new(&mut nonce_buffer, &mut auth_tag);

        let mut decrypted_data = vec![0; data.len()];
        // SAFETY: CipherModeInfoRef ensures the nonce and tag pointers maintain their lifetime
        unsafe {
            BCryptDecrypt(
                decrypt_key.key,
                Some(data),
                Some(auth_mode.get_padding_info().cast()),
                Some(&mut iv_buffer),
                Some(&mut decrypted_data),
                &mut decrypted_len,
                Default::default(),
            )
            .ok()
            .map_err(|e| {
                Aes256GcmError(Box::new(Aes256GcmErrorInner::BCrypt(
                    e,
                    "performing decryption",
                )))
            })?;
        }
        assert_eq!(decrypted_len as usize, decrypted_data.len());
        Ok(decrypted_data)
    }
}
