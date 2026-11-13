// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait definition for chipset device state transitions.

#![forbid(unsafe_code)]

use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

/// The completion of a device's input hook, such as
/// [`ChangeDeviceState::quiesce_input`].
///
/// An input hook runs synchronously, with exclusive access to the device. The
/// work that it must wait for, such as a request to a device task, completes in
/// this future instead, which the caller awaits after it gives up that access.
#[must_use = "an input hook is complete only once its completion is awaited"]
pub struct InputHookCompletion(Option<Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>>);

impl InputHookCompletion {
    /// Returns the completion of a hook that has nothing left to wait for.
    pub fn ready() -> Self {
        Self(None)
    }

    /// Returns a completion that waits for `future`.
    pub fn new(future: impl 'static + Send + Future<Output = anyhow::Result<()>>) -> Self {
        Self(Some(Box::pin(future)))
    }
}

impl Future for InputHookCompletion {
    type Output = anyhow::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.get_mut().0 {
            Some(future) => future.as_mut().poll(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

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
    /// This runs with exclusive access to the device, which for a chipset
    /// device means with its lock held while vCPUs may still access the device,
    /// so it must not block. The work that stopping input must wait for, such
    /// as a request to a device task, belongs in the returned completion, which
    /// the caller awaits after it releases the device. Meanwhile, the device
    /// keeps serving vCPUs and being polled.
    ///
    /// [`ChangeDeviceState::resume_input`] may be called even if the completion
    /// fails or is not awaited.
    fn quiesce_input(&mut self) -> InputHookCompletion {
        InputHookCompletion::ready()
    }

    /// Resumes host input after a failed snapshot transaction.
    ///
    /// This must be safe whether or not [`ChangeDeviceState::quiesce_input`]
    /// completed and idempotent across repeated recovery attempts. Like that
    /// hook, it must not block, and the caller awaits its completion after it
    /// releases the device.
    fn resume_input(&mut self) -> InputHookCompletion {
        InputHookCompletion::ready()
    }

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
