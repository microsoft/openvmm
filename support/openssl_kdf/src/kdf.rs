// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// See also the LICENSE file in the root of the crate for additional copyright
// information.

use super::sys::EVP_KDF_CTX_free;
use super::sys::EVP_KDF_CTX_new;
use super::sys::EVP_KDF_derive;
use super::sys::EVP_KDF_fetch;
use super::sys::EVP_MD_get0_name;
use super::sys::EVP_KDF;
use super::sys::EVP_KDF_CTX;
use super::sys::OSSL_KDF_PARAM_DIGEST;
use super::sys::OSSL_KDF_PARAM_INFO;
use super::sys::OSSL_KDF_PARAM_KBKDF_USE_L;
use super::sys::OSSL_KDF_PARAM_KBKDF_USE_SEPARATOR;
use super::sys::OSSL_KDF_PARAM_KEY;
use super::sys::OSSL_KDF_PARAM_MAC;
use super::sys::OSSL_KDF_PARAM_MODE;
use super::sys::OSSL_KDF_PARAM_SALT;
use super::sys::OSSL_KDF_PARAM_SEED;
use crate::cvt;
use crate::cvt_cp;
use crate::cvt_p;
use crate::params::Params;
use crate::params::ParamsBuilder;
use crate::sys::EVP_KDF_free;
use openssl::error::ErrorStack;
use openssl::hash::MessageDigest;
use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::NulError;
use std::ptr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KdfError {
    #[error("null byte found in string")]
    NulError(#[from] NulError),
    #[error("no such KDF")]
    NoSuchKdf,
    #[error("openSSL error")]
    Ssl(#[from] ErrorStack),
}

pub trait KdfParams {
    fn kdf_name(&self) -> String;
    fn to_params(&self) -> Result<Params, KdfError>;
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Mode {
    Counter,
    Feedback,
}

const COUNTER: &CStr = c"counter";
const FEEDBACK: &CStr = c"feedback";

impl Mode {
    fn to_param(self) -> &'static CStr {
        use Mode::*;
        match self {
            Counter => COUNTER,
            Feedback => FEEDBACK,
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Mac {
    Hmac,
    Cmac,
}

const HMAC: &CStr = c"HMAC";
const CMAC: &CStr = c"CMAC";

impl Mac {
    fn to_param(self) -> &'static CStr {
        use Mac::*;
        match self {
            Hmac => HMAC,
            Cmac => CMAC,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Kbkdf {
    md: MessageDigest,
    mode: Mode,
    mac: Mac,
    salt: Vec<u8>,
    key: Vec<u8>,
    context: Vec<u8>,
    seed: Vec<u8>,
    use_l: bool,
    use_separator: bool,
}

impl Kbkdf {
    pub fn new(md: MessageDigest, salt: Vec<u8>, key: Vec<u8>) -> Kbkdf {
        let mode = Mode::Counter;
        let mac = Mac::Hmac;
        let use_l = true;
        let use_separator = true;
        let context = Vec::new();
        let seed = Vec::new();

        Kbkdf {
            md,
            salt,
            key,
            mode,
            context,
            seed,
            mac,
            use_l,
            use_separator,
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn set_mac(&mut self, mac: Mac) {
        self.mac = mac;
    }

    pub fn set_context(&mut self, context: Vec<u8>) {
        self.context = context;
    }

    pub fn set_seed(&mut self, seed: Vec<u8>) {
        self.seed = seed;
    }

    pub fn set_l(&mut self, l: bool) {
        self.use_l = l;
    }

    pub fn set_separator(&mut self, separator: bool) {
        self.use_separator = separator;
    }
}

impl KdfParams for Kbkdf {
    fn kdf_name(&self) -> String {
        String::from("KBKDF")
    }

    fn to_params(&self) -> Result<Params, KdfError> {
        let mut params = ParamsBuilder::with_capacity(8);
        // SAFETY: MessageDigest is guaranteed to be valid, and we immediately validate the return value.
        let md_name = unsafe { cvt_cp(EVP_MD_get0_name(self.md.as_ptr())) }?;
        // SAFETY: md_name has been validated to be non-null, and OpenSSL guarantees that it is valid.
        let md_name = unsafe { CStr::from_ptr(md_name) };

        params.add_string(OSSL_KDF_PARAM_DIGEST, md_name)?;
        params.add_string(OSSL_KDF_PARAM_MAC, self.mac.to_param())?;
        params.add_string(OSSL_KDF_PARAM_MODE, self.mode.to_param())?;
        params.add_slice(OSSL_KDF_PARAM_KEY, &self.key)?;
        if !self.salt.is_empty() {
            params.add_slice(OSSL_KDF_PARAM_SALT, &self.salt)?;
        }
        if !self.context.is_empty() {
            params.add_slice(OSSL_KDF_PARAM_INFO, &self.context)?;
        }
        if !self.seed.is_empty() {
            params.add_slice(OSSL_KDF_PARAM_SEED, &self.seed)?;
        }
        if self.use_l {
            params.add_i32(OSSL_KDF_PARAM_KBKDF_USE_L, 1)?;
        } else {
            params.add_i32(OSSL_KDF_PARAM_KBKDF_USE_L, 0)?;
        }

        if self.use_separator {
            params.add_i32(OSSL_KDF_PARAM_KBKDF_USE_SEPARATOR, 1)?;
        } else {
            params.add_i32(OSSL_KDF_PARAM_KBKDF_USE_SEPARATOR, 0)?;
        }

        Ok(params.build())
    }
}

pub fn derive<P: KdfParams>(kdf_params: P, output: &mut [u8]) -> Result<(), KdfError> {
    openssl_sys::init();

    let name = kdf_params.kdf_name();
    let name = CString::new(name.as_bytes())?;
    let name = name.as_bytes_with_nul();

    // SAFETY: Name is guaranteed to be valid, null is valid for the other two parameters, and we immediately validate the return value.
    let kdf = unsafe {
        let ptr = EVP_KDF_fetch(ptr::null_mut(), name.as_ptr().cast(), ptr::null());
        if ptr.is_null() {
            Err(KdfError::NoSuchKdf)
        } else {
            Ok(Kdf(ptr))
        }
    }?;

    let mut ctx = KdfContext::new(kdf)?;
    let mut params = kdf_params.to_params()?;

    // TODO: Check EVP_KDF_CTX_get_kdf_size ?
    // SAFETY: All parameters are guaranteed to be valid, and we immediately validate the return value.
    unsafe {
        cvt(EVP_KDF_derive(
            ctx.as_mut_ptr(),
            output.as_mut_ptr(),
            output.len(),
            params.as_ptr(),
        ))?
    };
    drop(params);

    Ok(())
}

// The pointer must be valid and allocated via EVP_KDF_fetch.
struct Kdf(*mut EVP_KDF);

impl Drop for Kdf {
    fn drop(&mut self) {
        // SAFETY: This type guarantees that the pointer is valid and allocated via EVP_KDF_fetch.
        unsafe { EVP_KDF_free(self.0) };
    }
}

struct KdfContext(*mut EVP_KDF_CTX);

impl KdfContext {
    fn new(kdf: Kdf) -> Result<Self, ErrorStack> {
        // SAFETY: kdf is guaranteed to be valid, and we immediately validate the return value.
        let ctx = unsafe { cvt_p(EVP_KDF_CTX_new(kdf.0))? };
        Ok(KdfContext(ctx))
    }
}

impl KdfContext {
    fn as_mut_ptr(&mut self) -> *mut EVP_KDF_CTX {
        self.0
    }
}

impl Drop for KdfContext {
    fn drop(&mut self) {
        // SAFETY: This type guarantees that the pointer is valid and allocated via EVP_KDF_CTX_new.
        unsafe { EVP_KDF_CTX_free(self.0) };
    }
}

#[cfg(test)]
mod tests {}
