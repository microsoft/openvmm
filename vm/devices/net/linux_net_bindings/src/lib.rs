// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// C API bingings based on /usr/include/linux/if.h and
// /usr/include/linux/if_tun.h.

#![cfg(unix)]
// UNSAFETY: bindgen generated code.
#![expect(unsafe_code)]

use nix::ioctl_write_ptr_bad;
use nix::request_code_write;
use std::os::raw::c_int;

// Generated using:
//
// bindgen --no-layout-tests --with-derive-default --wrap-unsafe-ops /usr/include/linux/if.h
#[allow(non_camel_case_types)]
#[allow(dead_code)]
#[allow(non_upper_case_globals)]
#[allow(clippy::missing_safety_doc)]
#[allow(clippy::undocumented_unsafe_blocks)]
#[allow(clippy::ref_as_ptr)]
#[allow(clippy::ptr_as_ptr)]
pub mod gen_if;

// Generated using:
//
// bindgen --no-layout-tests --with-derive-default --wrap-unsafe-ops /usr/include/linux/if_tun.h
#[allow(non_camel_case_types)]
#[allow(dead_code)]
#[allow(non_upper_case_globals)]
#[allow(clippy::missing_safety_doc)]
#[allow(clippy::undocumented_unsafe_blocks)]
#[allow(clippy::ref_as_ptr)]
#[allow(clippy::ptr_as_ptr)]
pub mod gen_if_tun;

// #define TUNSETIFF     _IOW('T', 202, int)
ioctl_write_ptr_bad!(
    tun_set_iff,
    request_code_write!(b'T', 202, size_of::<c_int>()),
    gen_if::ifreq
);
