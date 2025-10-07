// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// See also the LICENSE file in the root of the crate for additional copyright
// information.

//! Crate wrapping the openssl 3.0 KDF APIs.
//!
//! This crate can be removed once these capabilities are added to the upstream
//! openssl crate. They are included here instead of in a fork of the openssl
//! crate to avoid unnecessary forking.

// Currently this crate is only consumed by other cfg(unix) crates.
#![cfg(unix)]
#![expect(missing_docs)]
// UNSAFETY: Calls into openssl.
#![expect(unsafe_code)]

pub mod kdf;
pub mod params;
mod sys;

use libc::c_int;
use openssl::error::ErrorStack;

fn cvt_p<T>(r: *mut T) -> Result<*mut T, ErrorStack> {
    if r.is_null() {
        Err(ErrorStack::get())
    } else {
        Ok(r)
    }
}

fn cvt_cp<T>(r: *const T) -> Result<*const T, ErrorStack> {
    if r.is_null() {
        Err(ErrorStack::get())
    } else {
        Ok(r)
    }
}

fn cvt(r: c_int) -> Result<c_int, ErrorStack> {
    if r <= 0 {
        Err(ErrorStack::get())
    } else {
        Ok(r)
    }
}
