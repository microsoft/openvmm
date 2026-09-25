// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Timing diagnostics for MSHV guest memory registration.

use crate::MshvIsolationState;
use mshv_bindings::mshv_user_mem_region;
use mshv_ioctls::VmFd;

impl MshvIsolationState {
    /// Maps user memory like `map_user_memory` and emits an
    /// `MSHV_SET_GUEST_MEMORY completed` event with the elapsed time.
    pub(crate) fn map_memory_timed(
        &self,
        vmfd: &VmFd,
        region: mshv_user_mem_region,
    ) -> anyhow::Result<bool> {
        let started = std::time::Instant::now();
        let result = self.map_user_memory(vmfd, region);
        tracing::info!(
            elapsed_us = started.elapsed().as_micros() as u64,
            success = result.is_ok(),
            "MSHV_SET_GUEST_MEMORY completed"
        );
        result
    }
}
