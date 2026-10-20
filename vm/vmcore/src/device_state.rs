// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait definition for chipset device state transitions.

#![forbid(unsafe_code)]

use std::future::Future;

/// Trait for transitioning device state.
pub trait ChangeDeviceState {
    /// Starts a device, allowing it to interact with the guest asynchronously.
    ///
    /// For example, a device might process work queues that reside in guest
    /// memory on a separate thread.
    ///
    /// Callers must ensure that the device is in a stopped state before calling
    /// this method.
    ///
    /// This is a synchronous method instead of an asynchronous one because it
    /// is a notification only--callers do not need to wait for the device to
    /// finish starting to consider the VM started. Devices should kick off any
    /// tasks that need to run any return, without waiting.
    ///
    // FUTURE: make this asynchronous if any device has a good reason to need
    // it. This may also require changing state units to wait for device start
    // to finish, which may require a bunch of other changes.
    fn start(&mut self);

    /// Stops accepting new host input before a snapshot vCPU boundary.
    ///
    /// [`ChangeDeviceState::resume_input`] may be called even if this method
    /// returns an error or its completion is not observed.
    ///
    /// For chipset devices backed by `ArcMutexChipsetDeviceUnit`, this hook
    /// runs with the chipset device lock held while vCPUs may still issue
    /// MMIO/PIO. Guest accesses wait for the hook to return. The hook must
    /// therefore complete promptly and must not await work that requires the
    /// same device lock, which would deadlock.
    fn quiesce_input(&mut self) -> impl Send + Future<Output = anyhow::Result<()>> {
        async { Ok(()) }
    }

    /// Resumes host input after a failed snapshot transaction.
    ///
    /// This must be safe whether or not [`ChangeDeviceState::quiesce_input`]
    /// completed and idempotent across repeated recovery attempts.
    ///
    /// On `ArcMutexChipsetDeviceUnit`, this also runs with the chipset device
    /// lock held while vCPUs may access the device. It must complete promptly
    /// and must not await work that requires that lock.
    fn resume_input(&mut self) -> impl Send + Future<Output = anyhow::Result<()>> {
        async { Ok(()) }
    }

    /// Stops a device's asynchronous work.
    ///
    /// After this returns, the device must not process any additional work. It
    /// should be in a stable state where it can be saved without losing data
    /// (if it implements the appropriate trait).
    ///
    /// Callers must ensure that the device is in a started state before calling
    /// this method.
    fn stop(&mut self) -> impl Send + Future<Output = ()>;

    /// Resets the device state to its initial state, for a fresh boot.
    ///
    /// Callers must ensure that the device is in a stopped state before calling
    /// this method.
    fn reset(&mut self) -> impl Send + Future<Output = ()>;
}
