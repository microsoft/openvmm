// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait definition for chipset device state transitions.

#![forbid(unsafe_code)]

use std::future::Future;
use std::time::Duration;

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

    /// Starts a device and waits until startup has either completed or failed.
    ///
    /// The default preserves the notification-only behavior of [`Self::start`].
    /// Devices that restore process-local workers should override this so a
    /// restore failure is reported before dependent units start.
    fn start_fallible(&mut self) -> impl Send + Future<Output = anyhow::Result<()>> {
        self.start();
        async { Ok(()) }
    }

    /// Stops accepting new host input before a snapshot vCPU boundary.
    fn quiesce_input(&mut self) -> impl Send + Future<Output = anyhow::Result<()>> {
        async { Ok(()) }
    }

    /// Resumes host input after a failed snapshot transaction.
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

    /// Advances guest-visible device time after snapshot restore.
    fn advance_time(
        &mut self,
        _duration: Duration,
    ) -> impl Send + Future<Output = anyhow::Result<()>> {
        async { Ok(()) }
    }
}
