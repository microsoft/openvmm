// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// C API bingings based on /usr/include/linux/if.h and
// /usr/include/linux/if_tun.h.

#![expect(missing_docs)]
#![cfg(unix)]
// UNSAFETY: bindgen generated code.
#![expect(unsafe_code)]

use nix::ioctl_read_bad;
use nix::ioctl_write_int_bad;
use nix::ioctl_write_ptr_bad;
use nix::request_code_read;
use nix::request_code_write;
use std::os::raw::c_int;

// Generated using:
//
// bindgen --no-layout-tests --with-derive-default --wrap-unsafe-ops --no-doc-comments /usr/include/linux/if.h
#[expect(non_camel_case_types)]
#[expect(non_upper_case_globals)]
#[expect(clippy::missing_safety_doc)]
#[expect(clippy::undocumented_unsafe_blocks)]
#[expect(clippy::ref_as_ptr)]
#[expect(clippy::ptr_as_ptr)]
pub mod gen_if;

// Generated using:
//
// bindgen --no-layout-tests --with-derive-default --wrap-unsafe-ops --no-doc-comments /usr/include/linux/if_tun.h
#[expect(non_camel_case_types)]
#[expect(clippy::missing_safety_doc)]
#[expect(clippy::undocumented_unsafe_blocks)]
#[expect(clippy::ref_as_ptr)]
#[expect(clippy::ptr_as_ptr)]
pub mod gen_if_tun;

// #define TUNSETIFF     _IOW('T', 202, int)
ioctl_write_ptr_bad!(
    tun_set_iff,
    request_code_write!(b'T', 202, size_of::<c_int>()),
    gen_if::ifreq
);

// #define TUNSETOFFLOAD _IOW('T', 208, unsigned int)
// Note: the kernel reads the offload flags directly from the ioctl arg,
// not via copy_from_user, so we must pass the value, not a pointer.
ioctl_write_int_bad!(
    tun_set_offload,
    request_code_write!(b'T', 208, size_of::<c_int>())
);

// #define TUNGETIFF _IOR('T', 210, unsigned int)
ioctl_read_bad!(
    tun_get_iff,
    request_code_read!(b'T', 210, size_of::<c_int>()),
    gen_if::ifreq
);

// #define TUNGETVNETHDRSZ _IOR('T', 215, int)
ioctl_read_bad!(
    tun_get_vnet_hdr_sz,
    request_code_read!(b'T', 215, size_of::<c_int>()),
    c_int
);

// #define TUNSETVNETHDRSZ _IOW('T', 216, int)
ioctl_write_ptr_bad!(
    tun_set_vnet_hdr_sz,
    request_code_write!(b'T', 216, size_of::<c_int>()),
    c_int
);
