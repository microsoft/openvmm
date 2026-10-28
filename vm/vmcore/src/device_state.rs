// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait definition for chipset device state transitions.

#![forbid(unsafe_code)]

use crate::save_restore::RestoreError;
use crate::save_restore::SaveError;
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

    /// Prepares the device to be saved.
    ///
    /// Callers must ensure that the device is in a stopped state before calling
    /// this method, and must call it immediately before the device's
    /// synchronous `save` method
    /// ([`SaveRestore::save`](crate::save_restore::SaveRestore::save) or
    /// [`ProtobufSaveRestore::save`](crate::save_restore::ProtobufSaveRestore::save)).
    /// If this fails, the save fails, and `save` is not called.
    ///
    /// Devices that must do asynchronous or fallible work to produce their
    /// saved state, such as devices whose state is owned by an asynchronous
    /// task, can do that work here and keep the result for `save`.
    ///
    /// The default implementation does nothing. Wrappers that forward `save`
    /// to an inner device must forward this method too, or the inner device's
    /// implementation will be skipped.
    fn prepare_save(&mut self) -> impl Send + Future<Output = Result<(), SaveError>> {
        async { Ok(()) }
    }

    /// Finishes restoring the device.
    ///
    /// Callers must ensure that the device is in a stopped state before calling
    /// this method, and must call it immediately after the device's
    /// synchronous `restore` method
    /// ([`SaveRestore::restore`](crate::save_restore::SaveRestore::restore) or
    /// [`ProtobufSaveRestore::restore`](crate::save_restore::ProtobufSaveRestore::restore))
    /// succeeds. If this fails, the restore fails.
    ///
    /// Devices that must do asynchronous or fallible work to apply their saved
    /// state, such as devices whose state is owned by an asynchronous task, can
    /// keep the state in `restore` and do that work here.
    ///
    /// The default implementation does nothing. Wrappers that forward
    /// `restore` to an inner device must forward this method too, or the inner
    /// device's implementation will be skipped.
    fn finish_restore(&mut self) -> impl Send + Future<Output = Result<(), RestoreError>> {
        async { Ok(()) }
    }
}
