// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod test_helpers;

crate::tmk_tests! {
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
