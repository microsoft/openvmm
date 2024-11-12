// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// See also the LICENSE file in the root of the crate for additional copyright
// information.

use super::ossl_typ::EVP_KDF;
use super::ossl_typ::EVP_KDF_CTX;
use super::ossl_typ::OSSL_LIB_CTX;
use super::ossl_typ::OSSL_PARAM;
use libc::c_char;
use libc::c_int;
use libc::c_uchar;
use libc::size_t;
use openssl_sys::EVP_MD;

#[allow(clippy::upper_case_acronyms)]
pub enum KDF {}

pub enum KDF_CTX {}

unsafe extern "C" {
    pub fn EVP_MD_get0_name(md: *const EVP_MD) -> *const c_char;
}

pub const OSSL_PARAM_INTEGER: c_uchar = 1;
pub const OSSL_PARAM_UNSIGNED_INTEGER: c_uchar = 2;
pub const OSSL_PARAM_REAL: c_uchar = 3;
pub const OSSL_PARAM_UTF8_STRING: c_uchar = 4;
pub const OSSL_PARAM_OCTET_STRING: c_uchar = 5;
pub const OSSL_PARAM_UTF8_PTR: c_uchar = 6;
pub const OSSL_PARAM_OCTET_PTR: c_uchar = 7;

unsafe extern "C" {
    pub fn EVP_KDF_fetch(
        libctx: *mut OSSL_LIB_CTX,
        algorithm: *const c_char,
        properties: *const c_char,
    ) -> *mut EVP_KDF;
    pub fn EVP_KDF_free(kdf: *mut EVP_KDF);
    pub fn EVP_KDF_CTX_new(kdf: *const EVP_KDF) -> *mut EVP_KDF_CTX;
    pub fn EVP_KDF_CTX_free(kdf: *mut EVP_KDF_CTX);

    pub fn EVP_KDF_CTX_set_params(ctx: *mut EVP_KDF, params: OSSL_PARAM);

    pub fn EVP_KDF_CTX_reset(ctx: *mut EVP_KDF_CTX);
    pub fn EVP_KDF_CTX_get_kdf_size(ctx: *mut EVP_KDF_CTX) -> size_t;
    pub fn EVP_KDF_CTX_kdf(ctx: *mut EVP_KDF_CTX) -> *const EVP_KDF;
    pub fn EVP_KDF_derive(
        ctx: *mut EVP_KDF_CTX,
        out: *mut u8,
        n: size_t,
        params: *const OSSL_PARAM,
    ) -> c_int;
}
