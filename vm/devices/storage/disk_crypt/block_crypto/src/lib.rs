// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cryptography primitives for disk encryption.

#![warn(missing_docs)]

#[cfg(windows)]
use bcrypt as sys;
#[cfg(unix)]
use ossl as sys;
use thiserror::Error;

/// AES-XTS-256 encryption/decryption.
pub struct AesXts256(sys::AesXts256);

/// An error for cryptographic operations.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(sys::Error);

impl AesXts256 {
    /// The required key length for the algorithm.
    ///
    /// Note that an AES-XTS key contains two AES keys, each of which is 256 bits.
    pub const KEY_LEN: usize = 64;

    /// Creates a new AES-XTS-256 encryption/decryption context.
    pub fn new(key: &[u8; Self::KEY_LEN], data_unit_size: u32) -> Result<Self, Error> {
        Ok(sys::aes_xts_256(key, data_unit_size)
            .map(AesXts256)
            .map_err(Error)?)
    }

    /// Returns a context for encrypting data.
    pub fn encrypt(&self) -> Result<AesXts256Ctx<'_>, Error> {
        Ok(AesXts256Ctx(self.0.ctx(true).map_err(Error)?))
    }

    /// Returns a context for decrypting data.
    pub fn decrypt(&self) -> Result<AesXts256Ctx<'_>, Error> {
        Ok(AesXts256Ctx(self.0.ctx(false).map_err(Error)?))
    }
}

/// Context for AES-XTS-256 encryption/decryption.
pub struct AesXts256Ctx<'a>(sys::AesXts256Ctx<'a>);

impl AesXts256Ctx<'_> {
    /// Encrypts or decrypts `data` using the provided `tweak`.
    pub fn cipher(&mut self, tweak: u128, data: &mut [u8]) -> Result<(), Error> {
        self.0.cipher(&tweak.to_le_bytes(), data).map_err(Error)?;
        Ok(())
    }
}

#[cfg(unix)]
mod ossl {
    pub struct NonStreamingCipher {
        enc: openssl::cipher_ctx::CipherCtx,
        dec: openssl::cipher_ctx::CipherCtx,
    }

    pub struct NonStreamingCipherCtx<'a> {
        ctx: openssl::cipher_ctx::CipherCtx,
        enc: bool,
        _dummy: &'a (),
    }

    pub type Error = openssl::error::ErrorStack;

    pub type AesXts256 = NonStreamingCipher;
    pub type AesXts256Ctx<'a> = NonStreamingCipherCtx<'a>;

    pub fn aes_xts_256(key: &[u8], _data_unit_size: u32) -> Result<AesXts256, Error> {
        let mut enc = openssl::cipher_ctx::CipherCtx::new()?;
        enc.encrypt_init(
            Some(openssl::cipher::Cipher::aes_256_xts()),
            Some(key),
            None,
        )?;
        let mut dec = openssl::cipher_ctx::CipherCtx::new()?;
        dec.decrypt_init(
            Some(openssl::cipher::Cipher::aes_256_xts()),
            Some(key),
            None,
        )?;
        Ok(NonStreamingCipher { enc, dec })
    }

    impl NonStreamingCipher {
        pub fn ctx(&self, enc: bool) -> Result<NonStreamingCipherCtx<'_>, Error> {
            let mut ctx = openssl::cipher_ctx::CipherCtx::new()?;
            ctx.copy(if enc { &self.enc } else { &self.dec })?;
            Ok(NonStreamingCipherCtx {
                ctx,
                enc,
                _dummy: &(),
            })
        }
    }

    impl NonStreamingCipherCtx<'_> {
        pub fn cipher(&mut self, iv: &[u8], data: &mut [u8]) -> Result<(), Error> {
            if self.enc {
                self.ctx.encrypt_init(None, None, Some(iv))?;
            } else {
                self.ctx.decrypt_init(None, None, Some(iv))?;
            }
            self.ctx.cipher_update_inplace(data, data.len())?;
            Ok(())
        }
    }
}

#[cfg(windows)]
mod bcrypt {
    // UNSAFETY: calling bcrypt APIs
    #![allow(unsafe_code)]

    use std::sync::OnceLock;
    use thiserror::Error;
    use windows::Win32::Foundation::RtlNtStatusToDosError;
    use windows::Win32::Foundation::NTSTATUS;
    use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
    use windows::Win32::Security::Cryptography::BCRYPT_HANDLE;
    use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
    use windows::Win32::Security::Cryptography::BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS;

    #[derive(Debug, Error)]
    #[error("{op} failed")]
    pub struct Error {
        op: &'static str,
        #[source]
        err: std::io::Error,
    }

    pub struct AesXts256(Key);

    pub struct AesXts256Ctx<'a> {
        key: &'a Key,
        enc: bool,
    }

    impl AesXts256 {
        pub fn ctx(&self, enc: bool) -> Result<AesXts256Ctx<'_>, Error> {
            Ok(AesXts256Ctx { key: &self.0, enc })
        }
    }

    impl AesXts256Ctx<'_> {
        pub fn cipher(&self, tweak: &[u8; 16], data: &mut [u8]) -> Result<(), Error> {
            // BCrypt only supports 64-bit tweaks, internally padding out the high 8
            // bytes with zeroes. (Why?) This is fine for our purposes but it's a
            // bit annoying to shuffle things around.
            let mut iv = u64::try_from(u128::from_le_bytes(*tweak))
                .map_err(|_| Error {
                    op: "convert tweak",
                    err: std::io::ErrorKind::InvalidInput.into(),
                })?
                .to_le_bytes();

            if self.enc {
                self.key.encrypt(&mut iv, data)
            } else {
                self.key.decrypt(&mut iv, data)
            }
        }
    }

    static AES_XTS_256: OnceLock<AlgHandle> = OnceLock::new();

    struct AlgHandle(BCRYPT_ALG_HANDLE);

    // SAFETY: the handle can be sent across threads.
    unsafe impl Send for AlgHandle {}
    // SAFETY: the handle can be shared across threads.
    unsafe impl Sync for AlgHandle {}

    fn bcrypt_result(op: &'static str, status: NTSTATUS) -> Result<(), Error> {
        if status.is_ok() {
            Ok(())
        } else {
            // SAFETY: no preconditions for this call.
            let err = unsafe { RtlNtStatusToDosError(status) };
            Err(Error {
                op,
                err: std::io::Error::from_raw_os_error(err as i32),
            })
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
        fn encrypt(&self, iv: &mut [u8], data: &mut [u8]) -> Result<(), Error> {
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

        fn decrypt(&self, iv: &mut [u8], data: &mut [u8]) -> Result<(), Error> {
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

    pub fn aes_xts_256(key: &[u8], data_unit_size: u32) -> Result<AesXts256, Error> {
        let alg = if let Some(alg) = AES_XTS_256.get() {
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
            if let Err(AlgHandle(handle)) = AES_XTS_256.set(AlgHandle(handle)) {
                // SAFETY: handle is valid and not aliased.
                unsafe {
                    bcrypt_result(
                        "close algorithm provider",
                        windows::Win32::Security::Cryptography::BCryptCloseAlgorithmProvider(
                            handle, 0,
                        ),
                    )
                    .unwrap();
                }
            }
            AES_XTS_256.get().unwrap()
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
                BCRYPT_HANDLE(key.0 .0),
                windows::Win32::Security::Cryptography::BCRYPT_MESSAGE_BLOCK_LENGTH,
                &data_unit_size.to_ne_bytes(),
                0,
            )
        };
        bcrypt_result("set message block length", status)?;

        Ok(AesXts256(key))
    }
}
