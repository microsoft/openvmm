// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-queue virtio device trait (`VirtioDevice`) and object-safe wrapper
//! (`DynVirtioDevice`).

use crate::DEFAULT_QUEUE_SIZE;
use crate::DeviceTraits;
use crate::QueueResources;
use crate::queue::QueueState;
use crate::spec::VirtioDeviceFeatures;
use guestmem::MappedMemoryRegion;
use inspect::InspectMut;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Per-queue virtio device trait. Ergonomic async fn — not object-safe.
///
/// Devices implement this trait. The blanket impl converts any
/// `VirtioDevice` into a `DynVirtioDevice` for use behind `Box<dyn>`.
pub trait VirtioDevice: InspectMut + Send {
    /// Device identity and capabilities.
    fn traits(&self) -> DeviceTraits;

    /// The queue size for the given queue index.
    ///
    /// This is the initial value the transport advertises to the guest
    /// (e.g. via `QUEUE_NUM_MAX` on MMIO, or `QUEUE_SIZE` on PCI). The
    /// transport does not enforce this as a per-device cap; the only hard
    /// limit is [`crate::MAX_QUEUE_SIZE`].
    ///
    /// Must be a power of two, >0, and ≤ [`crate::MAX_QUEUE_SIZE`]. The
    /// transport validates these invariants at construction time.
    ///
    /// `queue_index` must be less than `traits().max_queues`. The caller
    /// is responsible for bounds checking; implementations may panic on
    /// out-of-range indices.
    ///
    /// Override to provide per-device or per-queue sizes. The default
    /// returns [`DEFAULT_QUEUE_SIZE`] (256).
    fn queue_size(&self, _queue_index: u16) -> u16 {
        DEFAULT_QUEUE_SIZE
    }

    /// Read device-specific config registers.
    fn read_registers_u32(&mut self, offset: u16) -> impl Future<Output = u32> + Send;

    /// Write device-specific config registers.
    fn write_registers_u32(&mut self, offset: u16, val: u32) -> impl Future<Output = ()> + Send;

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
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Stop a single queue and return its state.
    ///
    /// `idx` is in `0..DeviceTraits::max_queues`. The caller will never
    /// pass an index outside that range.
    ///
    /// Returns the queue's `QueueState` on completion, or `None` if the
    /// queue was not active.
    ///
    /// This must be idempotent: calling it on a queue that was never
    /// started (or has already been stopped) must return `None`
    /// immediately. Transports rely on this during reset/disable by
    /// iterating all queue indices, not just active ones.
    fn stop_queue(&mut self, idx: u16) -> impl Future<Output = Option<QueueState>> + Send;

    /// Reset device-internal state to initial values.
    ///
    /// Called after all queues have been stopped on guest-initiated reset.
    /// Default: no-op.
    fn reset(&mut self) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Whether the device supports save/restore.
    ///
    /// Devices that return `false` will cause the transport's `save()` to
    /// fail with `SaveError::NotSupported`. Devices with host-side session
    /// state that cannot be serialized (e.g., virtio-9p, virtiofs) should
    /// leave this as `false`.
    fn supports_save_restore(&self) -> bool {
        false
    }
}

/// Object-safe wrapper for [`VirtioDevice`].
///
/// Uses boxed futures instead of `async fn` for object safety. The blanket
/// impl converts any `T: VirtioDevice` into a `DynVirtioDevice`.
///
/// The device task, backend server, and resolver hold `Box<dyn DynVirtioDevice>`.
pub trait DynVirtioDevice: InspectMut + Send {
    /// Device identity and capabilities.
    fn traits(&self) -> DeviceTraits;

    /// The queue size for the given queue index.
    fn queue_size(&self, queue_index: u16) -> u16;

    /// Read device-specific config registers.
    fn read_registers_u32(&mut self, offset: u16)
    -> Pin<Box<dyn Future<Output = u32> + Send + '_>>;

    /// Write device-specific config registers.
    fn write_registers_u32(
        &mut self,
        offset: u16,
        val: u32,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Provide the shared memory region to the device.
    fn set_shared_memory_region(
        &mut self,
        region: &Arc<dyn MappedMemoryRegion>,
    ) -> anyhow::Result<()>;

    /// Start a single queue.
    fn start_queue<'a>(
        &'a mut self,
        idx: u16,
        resources: QueueResources,
        features: &'a VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    /// Stop a single queue and return its state.
    fn stop_queue(
        &mut self,
        idx: u16,
    ) -> Pin<Box<dyn Future<Output = Option<QueueState>> + Send + '_>>;

    /// Reset device-internal state.
    fn reset(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Whether the device supports save/restore.
    fn supports_save_restore(&self) -> bool;
}

impl<T: VirtioDevice> DynVirtioDevice for T {
    fn traits(&self) -> DeviceTraits {
        VirtioDevice::traits(self)
    }

    fn queue_size(&self, queue_index: u16) -> u16 {
        VirtioDevice::queue_size(self, queue_index)
    }

    fn read_registers_u32(
        &mut self,
        offset: u16,
    ) -> Pin<Box<dyn Future<Output = u32> + Send + '_>> {
        Box::pin(VirtioDevice::read_registers_u32(self, offset))
    }

    fn write_registers_u32(
        &mut self,
        offset: u16,
        val: u32,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(VirtioDevice::write_registers_u32(self, offset, val))
    }

    fn set_shared_memory_region(
        &mut self,
        region: &Arc<dyn MappedMemoryRegion>,
    ) -> anyhow::Result<()> {
        VirtioDevice::set_shared_memory_region(self, region)
    }

    fn start_queue<'a>(
        &'a mut self,
        idx: u16,
        resources: QueueResources,
        features: &'a VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(VirtioDevice::start_queue(
            self,
            idx,
            resources,
            features,
            initial_state,
        ))
    }

    fn stop_queue(
        &mut self,
        idx: u16,
    ) -> Pin<Box<dyn Future<Output = Option<QueueState>> + Send + '_>> {
        Box::pin(VirtioDevice::stop_queue(self, idx))
    }

    fn reset(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(VirtioDevice::reset(self))
    }

    fn supports_save_restore(&self) -> bool {
        VirtioDevice::supports_save_restore(self)
    }
}
