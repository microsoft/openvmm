// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod hv_error_vp_start;
#[cfg(target_arch = "x86_64")]
pub mod hv_memory_protect_read;
#[cfg(target_arch = "x86_64")]
pub mod hv_memory_protect_write;
pub mod hv_processor;
#[cfg(target_arch = "x86_64")]
pub mod hv_register_intercept;
#[cfg(target_arch = "x86_64")]
pub mod hv_tpm_read_cvm;
#[cfg(target_arch = "x86_64")]
pub mod hv_tpm_write_cvm;

crate::opentmk_tests! {
    ctx: opentmk_core::platform::hyperv::ctx::HvTestCtx,
    tests: {
        hv_error_vp_start,
        hv_processor,
        #[cfg(target_arch = "x86_64")]
        hv_memory_protect_read,
        #[cfg(target_arch = "x86_64")]
        hv_memory_protect_write,
        #[cfg(target_arch = "x86_64")]
        hv_register_intercept,
        #[cfg(target_arch = "x86_64")]
        hv_tpm_read_cvm,
        #[cfg(target_arch = "x86_64")]
        hv_tpm_write_cvm,
    },
}
