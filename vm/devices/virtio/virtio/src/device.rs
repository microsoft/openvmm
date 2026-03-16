// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-queue virtio device trait (`VirtioDeviceV2`) and adapter to the
//! legacy `VirtioDevice` trait.

use crate::DeviceTraits;
use crate::QueueResources;
use crate::Resources;
use crate::VirtioDevice;
use crate::queue::QueueState;
use crate::spec::VirtioDeviceFeatures;
use guestmem::MappedMemoryRegion;
use inspect::InspectMut;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

/// Per-queue virtio device trait. Replaces the device-level
/// `VirtioDevice::enable()` / `poll_disable()` with per-queue start/stop.
pub trait VirtioDeviceV2: InspectMut + Send {
    /// Device identity and capabilities.
    fn traits(&self) -> DeviceTraits;

    /// Read device-specific config registers.
    fn read_registers_u32(&mut self, offset: u16) -> u32;

    /// Write device-specific config registers.
    fn write_registers_u32(&mut self, offset: u16, val: u32);

    /// Provide the shared memory region to the device.
    ///
    /// Called before `start_queue` when the device advertises a shared
    /// memory region (e.g., virtio-pmem, virtio-fs with DAX). Corresponds
    /// to `VHOST_USER_GET_SHARED_MEMORY_REGIONS` in the vhost-user protocol.
    ///
    /// Default: no-op.
    fn set_shared_memory_region(
        &mut self,
        _region: &Arc<dyn MappedMemoryRegion>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Start a single queue.
    ///
    /// Called when a queue becomes active — either because the guest set
    /// DRIVER_OK (transport starts all enabled queues), or a vhost-user
    /// frontend activated a specific queue.
    ///
    /// `idx` is in `0..DeviceTraits::max_queues`. The caller will never
    /// pass an index outside that range.
    ///
    /// `initial_state` provides restored queue indices for save/restore
    /// or vhost-user `SET_VRING_BASE`. If `None`, the queue starts fresh
    /// (indices at 0).
    fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()>;

    /// Stop a single queue and return its state.
    ///
    /// `idx` is in `0..DeviceTraits::max_queues`. The caller will never
    /// pass an index outside that range.
    ///
    /// Returns the queue's `QueueState` on completion, or `None` if the
    /// queue was not active.
    fn poll_stop_queue(&mut self, cx: &mut Context<'_>, idx: u16) -> Poll<Option<QueueState>>;

    /// Reset device-internal state to initial values.
    ///
    /// Called after all queues have been stopped on guest-initiated reset.
    /// Default: no-op.
    fn reset(&mut self) {}
}

/// Adapter that wraps a [`VirtioDeviceV2`] to implement the legacy
/// [`VirtioDevice`] trait.
pub struct VirtioDeviceAdapter<T> {
    device: T,
    /// Indices of queues that were started, in order.
    active_queues: Vec<u16>,
    /// Features from the last `enable()` call.
    features: Option<VirtioDeviceFeatures>,
    /// Index into `active_queues` for `poll_disable` progress.
    disable_index: usize,
}

impl<T: VirtioDeviceV2> VirtioDeviceAdapter<T> {
    pub fn new(device: T) -> Self {
        Self {
            device,
            active_queues: Vec::new(),
            features: None,
            disable_index: 0,
        }
    }
}

impl<T: VirtioDeviceV2> InspectMut for VirtioDeviceAdapter<T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.device.inspect_mut(req);
    }
}

impl<T: 'static + VirtioDeviceV2> VirtioDevice for VirtioDeviceAdapter<T> {
    fn traits(&self) -> DeviceTraits {
        self.device.traits()
    }

    fn read_registers_u32(&mut self, offset: u16) -> u32 {
        self.device.read_registers_u32(offset)
    }

    fn write_registers_u32(&mut self, offset: u16, val: u32) {
        self.device.write_registers_u32(offset, val);
    }

    fn enable(&mut self, resources: Resources) -> anyhow::Result<()> {
        let features = resources.features;
        if let Some(region) = &resources.shared_memory_region {
            self.device.set_shared_memory_region(region)?;
        }
        let mut started = Vec::new();
        for (i, qr) in resources.queues.into_iter().enumerate() {
            let idx = i as u16;
            if !qr.params.enable {
                continue;
            }
            if let Err(e) = self.device.start_queue(idx, qr, &features, None) {
                // Best-effort stop of already-started queues.
                // TODO: reliably stop once this code is embedded in the transports.
                for &started_idx in &started {
                    let _ = self.device.poll_stop_queue(
                        &mut Context::from_waker(std::task::Waker::noop()),
                        started_idx,
                    );
                }
                self.active_queues.clear();
                self.features = None;
                return Err(e);
            }
            started.push(idx);
        }
        self.active_queues = started;
        self.features = Some(features);
        self.disable_index = 0;
        Ok(())
    }

    fn poll_disable(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while self.disable_index < self.active_queues.len() {
            let idx = self.active_queues[self.disable_index];
            ready!(self.device.poll_stop_queue(cx, idx));
            self.disable_index += 1;
        }
        self.device.reset();
        self.active_queues.clear();
        self.disable_index = 0;
        Poll::Ready(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::QueueParams;
    use inspect::InspectMut;
    use std::cell::RefCell;
    use std::future::poll_fn;
    use test_with_tracing::test;
    use vmcore::interrupt::Interrupt;

    #[derive(Default)]
    struct MockCalls {
        start_queue: Vec<(u16, bool)>,
        stop_queue: Vec<u16>,
        reset_count: usize,
    }

    struct MockDevice {
        calls: RefCell<MockCalls>,
    }

    impl MockDevice {
        fn new() -> Self {
            Self {
                calls: RefCell::new(MockCalls::default()),
            }
        }
    }

    impl InspectMut for MockDevice {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.respond();
        }
    }

    impl VirtioDeviceV2 for MockDevice {
        fn traits(&self) -> DeviceTraits {
            DeviceTraits::default()
        }

        fn read_registers_u32(&mut self, _offset: u16) -> u32 {
            0
        }

        fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

        fn start_queue(
            &mut self,
            idx: u16,
            resources: QueueResources,
            _features: &VirtioDeviceFeatures,
            _initial_state: Option<QueueState>,
        ) -> anyhow::Result<()> {
            self.calls
                .borrow_mut()
                .start_queue
                .push((idx, resources.params.enable));
            Ok(())
        }

        fn poll_stop_queue(&mut self, _cx: &mut Context<'_>, idx: u16) -> Poll<Option<QueueState>> {
            self.calls.borrow_mut().stop_queue.push(idx);
            Poll::Ready(None)
        }

        fn reset(&mut self) {
            self.calls.borrow_mut().reset_count += 1;
        }
    }

    fn make_resources(enable_flags: &[bool]) -> Resources {
        Resources {
            features: VirtioDeviceFeatures::new(),
            queues: enable_flags
                .iter()
                .map(|&enable| QueueResources {
                    params: QueueParams {
                        size: 16,
                        enable,
                        desc_addr: 0,
                        avail_addr: 0,
                        used_addr: 0,
                    },
                    notify: Interrupt::from_fn(|| {}),
                    event: pal_event::Event::new(),
                })
                .collect(),
            shared_memory_region: None,
            shared_memory_size: 0,
        }
    }

    #[test]
    fn enable_starts_enabled_queues() {
        let device = MockDevice::new();
        let mut adapter = VirtioDeviceAdapter::new(device);
        adapter.enable(make_resources(&[true, true])).unwrap();
        let calls = adapter.device.calls.borrow();
        assert_eq!(calls.start_queue.len(), 2);
        assert_eq!(calls.start_queue[0].0, 0);
        assert_eq!(calls.start_queue[1].0, 1);
    }

    #[test]
    fn enable_skips_disabled_queues() {
        let device = MockDevice::new();
        let mut adapter = VirtioDeviceAdapter::new(device);
        adapter.enable(make_resources(&[false, false])).unwrap();
        let calls = adapter.device.calls.borrow();
        assert_eq!(calls.start_queue.len(), 0);
    }

    #[test]
    fn poll_disable_stops_all_and_resets() {
        let device = MockDevice::new();
        let mut adapter = VirtioDeviceAdapter::new(device);
        adapter.enable(make_resources(&[true, true])).unwrap();

        let result = poll_fn(|cx| adapter.poll_disable(cx));
        futures::executor::block_on(result);

        let calls = adapter.device.calls.borrow();
        assert_eq!(calls.stop_queue.len(), 2);
        assert_eq!(calls.stop_queue[0], 0);
        assert_eq!(calls.stop_queue[1], 1);
        assert_eq!(calls.reset_count, 1);
    }
}
