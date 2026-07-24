// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod hv_error_vp_start;
#[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
pub mod hv_memory_protect_read;
#[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
pub mod hv_memory_protect_write;
pub mod hv_processor;
#[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
pub mod hv_register_intercept;
#[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
pub mod hv_tpm_read_cvm;
#[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
pub mod hv_tpm_write_cvm;
pub mod test_helpers;

crate::opentmk_tests! {
    ctx: crate::platform::hyperv::ctx::HvTestCtx,
    tests: {
        hv_error_vp_start,
        hv_processor,
        #[cfg(nightly)]
        hv_memory_protect_read,
        #[cfg(nightly)]
        hv_memory_protect_write,
        #[cfg(nightly)]
        #[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
        hv_register_intercept,
        #[cfg(nightly)]
        #[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
        hv_tpm_read_cvm,
        #[cfg(nightly)]
        #[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
        hv_tpm_write_cvm,
    },
}
