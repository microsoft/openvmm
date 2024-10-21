// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// See also the LICENSE file in the root of the crate for additional copyright
// information.

use libc::c_char;
use libc::c_uint;
use libc::c_void;
use libc::size_t;
use std::ptr;

pub enum EVP_KDF {}
#[repr(C)]
pub struct EVP_KDF_CTX {
    pub ameth: *const EVP_KDF,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct OSSL_PARAM {
    pub key: *const c_char,
    pub data_type: c_uint,
    pub data: *mut c_void,
    pub data_size: size_t,
    pub return_size: size_t,
}
pub const OSSL_PARAM_END: OSSL_PARAM = OSSL_PARAM {
    key: ptr::null(),
    data_type: 0,
    data: ptr::null_mut(),
    data_size: 0,
    return_size: 0,
};

pub enum OSSL_LIB_CTX {}
