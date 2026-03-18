// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-queue virtio device trait (`VirtioDevice`).

use crate::DeviceTraits;
use crate::QueueResources;
use crate::queue::QueueState;
use crate::spec::VirtioDeviceFeatures;
use guestmem::MappedMemoryRegion;
use inspect::InspectMut;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

/// Per-queue virtio device trait. Replaces the device-level
/// `VirtioDevice::enable()` / `poll_disable()` with per-queue start/stop.
pub trait VirtioDevice: InspectMut + Send {
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
    ///
    /// This must be idempotent: calling it on a queue that was never
    /// started (or has already been stopped) must return
    /// `Poll::Ready(None)` immediately. Transports rely on this during
    /// reset/disable by iterating all queue indices, not just active ones.
    fn poll_stop_queue(&mut self, cx: &mut Context<'_>, idx: u16) -> Poll<Option<QueueState>>;

    /// Reset device-internal state to initial values.
    ///
    /// Called after all queues have been stopped on guest-initiated reset.
    /// Default: no-op.
    fn reset(&mut self) {}

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
