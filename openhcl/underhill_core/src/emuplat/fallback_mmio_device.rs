// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use cvm_tracing::CVM_ALLOWED;
use hcl::ioctl::MshvHvcall;
use memory_range::MemoryRange;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

// Represents a stub MMIO device that handles unhandled MMIO accesses by
// forwarding them to the host on the blocking thread pool.
pub(crate) struct FallbackMmioDevice {
    pub(crate) mmio_ranges: Option<Vec<MemoryRange>>,
    pub(crate) mshv_hvcall: Arc<MshvHvcall>,
}

impl FallbackMmioDevice {
    fn is_allowed(&self, addr: u64, data_len: usize) -> bool {
        self.mmio_ranges.as_ref().is_none_or(|v| {
            v.iter().any(|range| {
                range.contains_addr(addr) && range.contains_addr(addr + data_len as u64 - 1)
            })
        })
    }
}

impl MmioIntercept for FallbackMmioDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        if !self.is_allowed(addr, data.len()) {
            data.fill(!0);
            return IoResult::Ok;
        }

        let (deferred, token) = defer_read();
        let mshv_hvcall = self.mshv_hvcall.clone();
        let len = data.len();
        blocking::unblock(move || {
            let mut buffer = vec![!0; len];
            let start = Instant::now();
            let result = mshv_hvcall.mmio_read(addr, &mut buffer);
            let elapsed = start.elapsed();
            if elapsed > Duration::from_secs(1) {
                tracelimit::warn_ratelimited!(
                    CVM_ALLOWED,
                    addr,
                    ?elapsed,
                    "host MMIO read hypercall took longer than 1 second"
                );
            }
            if let Err(err) = result {
                tracelimit::error_ratelimited!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    "failed host MMIO read"
                );
            }
            deferred.complete(&buffer);
        })
        .detach();

        IoResult::Defer(token)
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        if !self.is_allowed(addr, data.len()) {
            return IoResult::Ok;
        }

        let (deferred, token) = defer_write();
        let mshv_hvcall = self.mshv_hvcall.clone();
        let data = data.to_vec();
        blocking::unblock(move || {
            let start = Instant::now();
            let result = mshv_hvcall.mmio_write(addr, &data);
            let elapsed = start.elapsed();
            if elapsed > Duration::from_secs(1) {
                tracelimit::warn_ratelimited!(
                    CVM_ALLOWED,
                    addr,
                    ?elapsed,
                    "host MMIO write hypercall took longer than 1 second"
                );
            }
            if let Err(err) = result {
                tracelimit::error_ratelimited!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    "failed host MMIO write"
                );
            }
            deferred.complete();
        })
        .detach();

        IoResult::Defer(token)
    }
}

impl ChipsetDevice for FallbackMmioDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}
