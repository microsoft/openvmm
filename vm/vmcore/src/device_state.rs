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

    /// Pauses guest-request processing in a save-consistent state.
    ///
    /// After this returns, the device must not process additional guest requests
    /// in software. It
    /// should be in a stable state where it can be saved without losing data
    /// (if it implements the appropriate trait).
    ///
    /// Stop is not a backend drain or a guarantee that DMA has ceased.
    /// Issued I/O may remain owned by the device across pause. Save/restore
    /// must account for such requests; start resumes their processing, while
    /// terminal teardown must finish them before releasing their resources.
    ///
    /// Assigned hardware that cannot be paused losslessly may remain active
    /// until teardown. Such a device must not claim save/restore support without
    /// a way to quiesce hardware and capture its state.
    ///
    /// Callers must ensure that the device is in a started state before calling
    /// this method.
    fn stop(&mut self) -> impl Send + Future<Output = ()>;

    /// Resets the device state to its initial state, for a fresh boot.
    ///
    /// Callers must ensure that the device is in a stopped state before calling
    /// this method.
    fn reset(&mut self) -> impl Send + Future<Output = ()>;

    /// Consumes the stopped device, optionally performing asynchronous cleanup.
    ///
    /// This is called when a device is removed or the VM chipset is torn down.
    /// Devices that own host resources (for example, open disk
    /// files) in background tasks should release them here so that the
    /// resources are closed by the time removal completes, rather than at
    /// some later point when the background tasks are reaped.
    ///
    /// Callers must ensure that the device is stopped and guest access is revoked
    /// before calling this method. The device must not subsequently be started,
    /// reset, or restored.
    ///
    /// Implementations must finish issued I/O and release resources held by
    /// background tasks before returning. Assigned hardware that cannot be
    /// quiesced without losing state during stop must be quiesced here.
    /// Dropping issued I/O futures is not generally safe and can abort the process.
    ///
    /// The default performs no asynchronous cleanup; it only drops the device.
    /// Devices owning background work or issued I/O need an implementation that
    /// settles that work before their fields are dropped. The default does not
    /// establish that such resources have been released.
    ///
    /// The returned future must remain owned and be driven to completion;
    /// dropping it can discard cancellation-unsafe issued I/O.
    fn teardown(self) -> impl Send + Future<Output = ()>
    where
        Self: Sized + Send,
    {
        async move {
            drop(self);
        }
    }
}
