// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// See also the LICENSE file in the root of the crate for additional copyright
// information.

use super::ossl_typ::OSSL_PARAM;
use libc::c_char;
use libc::c_int;
use libc::c_long;
use libc::c_uint;
use libc::c_ulong;
use libc::c_void;
use libc::size_t;
// https://github.com/rust-lang/libc/issues/1848
#[cfg_attr(target_env = "musl", allow(deprecated))]
use libc::time_t;
use openssl_sys::BIGNUM;
use std::ffi::CStr;

unsafe extern "C" {
    pub fn OSSL_PARAM_get_int(p: *const OSSL_PARAM, val: *mut c_int) -> c_int;
    pub fn OSSL_PARAM_get_uint(p: *const OSSL_PARAM, val: *mut c_uint) -> c_int;
    pub fn OSSL_PARAM_get_long(p: *const OSSL_PARAM, val: *mut c_long) -> c_int;
    pub fn OSSL_PARAM_get_ulong(p: *const OSSL_PARAM, val: *mut c_ulong) -> c_int;
    pub fn OSSL_PARAM_get_int32(p: *const OSSL_PARAM, val: *mut i32) -> c_int;
    pub fn OSSL_PARAM_get_uint32(p: *const OSSL_PARAM, val: *mut u32) -> c_int;
    pub fn OSSL_PARAM_get_int64(p: *const OSSL_PARAM, val: *mut i64) -> c_int;
    pub fn OSSL_PARAM_get_uint64(p: *const OSSL_PARAM, val: *mut u64) -> c_int;
    pub fn OSSL_PARAM_get_size_t(p: *const OSSL_PARAM, val: *mut size_t) -> c_int;
    // https://github.com/rust-lang/libc/issues/1848
    #[cfg_attr(target_env = "musl", allow(deprecated))]
    pub fn OSSL_PARAM_get_time_t(p: *const OSSL_PARAM, val: *mut time_t) -> c_int;

    pub fn OSSL_PARAM_set_int(p: *mut OSSL_PARAM, val: c_int) -> c_int;
    pub fn OSSL_PARAM_set_uint(p: *mut OSSL_PARAM, val: c_uint) -> c_int;
    pub fn OSSL_PARAM_set_long(p: *mut OSSL_PARAM, val: c_long) -> c_int;
    pub fn OSSL_PARAM_set_ulong(p: *mut OSSL_PARAM, val: c_ulong) -> c_int;
    pub fn OSSL_PARAM_set_int32(p: *mut OSSL_PARAM, val: i32) -> c_int;
    pub fn OSSL_PARAM_set_uint32(p: *mut OSSL_PARAM, val: u32) -> c_int;
    pub fn OSSL_PARAM_set_int64(p: *mut OSSL_PARAM, val: i64) -> c_int;
    pub fn OSSL_PARAM_set_uint64(p: *mut OSSL_PARAM, val: u64) -> c_int;
    pub fn OSSL_PARAM_set_size_t(p: *mut OSSL_PARAM, val: size_t) -> c_int;
    // https://github.com/rust-lang/libc/issues/1848
    #[cfg_attr(target_env = "musl", allow(deprecated))]
    pub fn OSSL_PARAM_set_time_t(p: *mut OSSL_PARAM, val: time_t) -> c_int;

    pub fn OSSL_PARAM_get_BN(p: *const OSSL_PARAM, val: *mut *mut BIGNUM) -> c_int;
    pub fn OSSL_PARAM_set_BN(p: *mut OSSL_PARAM, val: *const *mut BIGNUM) -> c_int;

    pub fn OSSL_PARAM_get_utf8_string(
        p: *const OSSL_PARAM,
        val: *mut *mut c_char,
        max_len: size_t,
    ) -> c_int;
    pub fn OSSL_PARAM_set_utf8_string(p: *mut OSSL_PARAM, val: *const c_char) -> c_int;

    pub fn OSSL_PARAM_get_octet_string(
        p: *const OSSL_PARAM,
        val: *mut *mut c_void,
        max_len: size_t,
        used_len: *mut size_t,
    ) -> c_int;
    pub fn OSSL_PARAM_set_octet_string(
        p: *mut OSSL_PARAM,
        val: *const c_void,
        len: size_t,
    ) -> c_int;

    pub fn OSSL_PARAM_get_utf8_ptr(p: *const OSSL_PARAM, val: *mut *const c_char) -> c_int;
    pub fn OSSL_PARAM_set_utf8_ptr(p: *mut OSSL_PARAM, val: *const c_char) -> c_int;

    pub fn OSSL_PARAM_get_octet_ptr(
        p: *const OSSL_PARAM,
        val: *mut *const c_void,
        used_len: *const size_t,
    ) -> c_int;
    pub fn OSSL_PARAM_set_octet_ptr(
        p: *mut OSSL_PARAM,
        val: *const c_void,
        used_len: size_t,
    ) -> c_int;

    pub fn OSSL_PARAM_get_utf8_string_ptr(p: *const OSSL_PARAM, val: *mut *const c_char) -> c_int;
    pub fn OSSL_PARAM_get_octet_string_ptr(
        p: *const OSSL_PARAM,
        val: *mut *const c_void,
        used_len: *mut size_t,
    ) -> c_int;

    pub fn OSSL_PARAM_construct_int(key: *const c_char, buf: *mut c_int) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_uint(key: *const c_char, buf: *mut c_uint) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_long(key: *const c_char, buf: *mut c_long) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_ulong(key: *const c_char, buf: *mut c_ulong) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_int32(key: *const c_char, buf: *mut i32) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_uint32(key: *const c_char, buf: *mut u32) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_int64(key: *const c_char, buf: *mut i64) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_uint64(key: *const c_char, buf: *mut u64) -> OSSL_PARAM;

    pub fn OSSL_PARAM_construct_size_t(key: *const c_char, buf: *mut size_t) -> OSSL_PARAM;
    // https://github.com/rust-lang/libc/issues/1848
    #[cfg_attr(target_env = "musl", allow(deprecated))]
    pub fn OSSL_PARAM_construct_time_t(key: *const c_char, buf: *mut time_t) -> OSSL_PARAM;

    pub fn OSSL_PARAM_construct_utf8_string(
        key: *const c_char,
        buf: *mut c_char,
        bsize: size_t,
    ) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_utf8_ptr(
        key: *const c_char,
        buf: *mut *mut c_char,
        bsize: size_t,
    ) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_octet_string(
        key: *const c_char,
        buf: *mut c_void,
        bsize: size_t,
    ) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_octet_ptr(
        key: *const c_char,
        buf: *mut *mut c_void,
        bsize: size_t,
    ) -> OSSL_PARAM;
    pub fn OSSL_PARAM_construct_end() -> OSSL_PARAM;
}

pub const OSSL_ALG_PARAM_DIGEST: &CStr = c"digest";
pub const OSSL_ALG_PARAM_CIPHER: &CStr = c"cipher";
pub const OSSL_ALG_PARAM_ENGINE: &CStr = c"engine";
pub const OSSL_ALG_PARAM_MAC: &CStr = c"mac";
pub const OSSL_ALG_PARAM_PROPERTIES: &CStr = c"properties";
