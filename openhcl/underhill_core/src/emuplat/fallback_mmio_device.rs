// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Forwards unhandled MMIO accesses to the host. Hypercalls run in deferred tasks
//! on the current thread's executor so the device lock is released ASAP.

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use cvm_tracing::CVM_ALLOWED;
use hcl::ioctl::MshvHvcall;
use memory_range::MemoryRange;
use pal_async::task::SpawnLocal;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

const SLOW_HYPERCALL_THRESHOLD: Duration = Duration::from_secs(1);

/// A stub MMIO device that forwards unhandled MMIO accesses to the host.
pub(crate) struct FallbackMmioDevice {
    pub(crate) mmio_ranges: Option<Vec<MemoryRange>>,
    pub(crate) mshv_hvcall: Arc<MshvHvcall>,
    pub(crate) pending_mmios: Arc<AtomicUsize>,
}

struct HostMmioGuard(Arc<AtomicUsize>);

impl HostMmioGuard {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter.clone())
    }
}

impl Drop for HostMmioGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
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
        let request = HostMmioGuard::new(&self.pending_mmios);
        underhill_threadpool::Thread::current()
            .unwrap()
            .spawn_local("fallback-mmio-read", async move {
                let _request = request;
                let mut buffer = vec![!0; len];
                let start = Instant::now();
                let result = mshv_hvcall.mmio_read(addr, &mut buffer);
                let elapsed = start.elapsed();
                if elapsed > SLOW_HYPERCALL_THRESHOLD {
                    tracelimit::warn_ratelimited!(
                        CVM_ALLOWED,
                        addr,
                        ?elapsed,
                        threshold = ?SLOW_HYPERCALL_THRESHOLD,
                        "host MMIO read hypercall took too long"
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
        let request = HostMmioGuard::new(&self.pending_mmios);
        underhill_threadpool::Thread::current()
            .expect("fallback MMIO must run on an underhill thread")
            .spawn_local("fallback-mmio-write", async move {
                let _request = request;
                let start = Instant::now();
                let result = mshv_hvcall.mmio_write(addr, &data);
                let elapsed = start.elapsed();
                if elapsed > SLOW_HYPERCALL_THRESHOLD {
                    tracelimit::warn_ratelimited!(
                        CVM_ALLOWED,
                        addr,
                        ?elapsed,
                        threshold = ?SLOW_HYPERCALL_THRESHOLD,
                        "host MMIO write hypercall took too long"
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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn inspect_outstanding(counter: &Arc<AtomicUsize>) -> inspect::Node {
        inspect::inspect(
            "pending_host_mmios",
            inspect::adhoc(|req| {
                req.respond().field("pending_host_mmios", Some(counter));
            }),
        )
        .results()
    }

    #[test]
    fn inspect_request_lifetime() {
        let counter = Arc::new(AtomicUsize::new(0));
        assert_eq!(
            inspect_outstanding(&counter),
            inspect::Node::Value(0u64.into())
        );

        let read = HostMmioGuard::new(&counter);
        let write_counter = counter.clone();
        let write = HostMmioGuard::new(&write_counter);
        assert_eq!(
            inspect_outstanding(&counter),
            inspect::Node::Value(2u64.into())
        );

        // Completing on another thread must update the same inspected counter.
        std::thread::spawn(move || drop(read)).join().unwrap();
        assert_eq!(
            inspect_outstanding(&counter),
            inspect::Node::Value(1u64.into())
        );
        drop(write);
        assert_eq!(
            inspect_outstanding(&counter),
            inspect::Node::Value(0u64.into())
        );
    }

    #[test]
    fn inspect_cancelled_request() {
        let counter = Arc::new(AtomicUsize::new(0));
        let request = HostMmioGuard::new(&counter);
        let queued = async move {
            let _request = request;
        };
        assert_eq!(
            inspect_outstanding(&counter),
            inspect::Node::Value(1u64.into())
        );

        drop(queued);
        assert_eq!(
            inspect_outstanding(&counter),
            inspect::Node::Value(0u64.into())
        );
    }
}
