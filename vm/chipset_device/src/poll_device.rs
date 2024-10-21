// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device poll services.
//!
//! These services are used to allow an otherwise synchronous device to run work
//! asynchronously. A device can register to have a poll function called
//! whenever an associated waker is woken. The poll function is passed a context
//! that can be used to poll futures.
//!
//! This provides an alternative to managing a separate asynchronous task for
//! the device. It simplifies start/stop management (because the poll function
//! is never called while the device is stopped), and it simplifies object
//! lifetimes and synchronization (since the poll function is called with `&mut
//! self`, so it has full access to the device's state).

use std::task::Context;

/// Implemented by devices which register themselves to be polled whenever the
/// associated waker is called.
pub trait PollDevice {
    /// Poll the device for asynchronous work.
    ///
    /// This is called asynchronously whenever the device enters the running
    /// state, and then whenever [`wake_by_ref`](std::task::Waker::wake_by_ref)
    /// is called on the waker passed in `cx.waker()`.
    ///
    /// The device will only be polled while in the running state. If the device
    /// is stopped, any wake events will be delayed until the device runs again.
    fn poll_device(&mut self, cx: &mut Context<'_>);
}
