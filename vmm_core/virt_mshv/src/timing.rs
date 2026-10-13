// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Timing diagnostics for MSHV guest memory registration.

use mshv_bindings::mshv_user_mem_region;
use mshv_ioctls::MshvError;
use mshv_ioctls::VmFd;

/// Registers user memory and emits the elapsed time for the actual ioctl.
pub(crate) fn map_user_memory(vmfd: &VmFd, region: mshv_user_mem_region) -> Result<(), MshvError> {
    let started = std::time::Instant::now();
    let result = vmfd.map_user_memory(region);
    tracing::info!(
        guest_pfn = region.guest_pfn,
        size = region.size,
        elapsed_us = started.elapsed().as_micros() as u64,
        success = result.is_ok(),
        "MSHV_SET_GUEST_MEMORY completed"
    );
    result
}
