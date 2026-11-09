// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait definition for chipset device state transitions.

#![forbid(unsafe_code)]

use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

/// What remains of a change of a device's host-input gate once
/// [`ChangeDeviceState::quiesce_input`] or
/// [`ChangeDeviceState::resume_input`] has made the change's synchronous
/// part: a future that completes with the change.
///
/// It does not borrow the device, so that the caller can release the device
/// before it awaits the change.
#[must_use = "a gate change is complete only once its wait completes"]
pub struct InputGateWait(Option<Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>>);

impl InputGateWait {
    /// A gate change that its synchronous part completed.
    pub fn done() -> Self {
        Self(None)
    }

    /// A gate change that completes when `wait` does.
    ///
    /// `wait` must not need the device, which the vCPUs can access while it
    /// runs: it waits for what the synchronous part started, such as the
    /// answer of the device's task to a request that the synchronous part
    /// sent.
    pub fn new(wait: impl 'static + Send + Future<Output = anyhow::Result<()>>) -> Self {
        Self(Some(Box::pin(wait)))
    }
}

impl Future for InputGateWait {
    type Output = anyhow::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.0 {
            None => Poll::Ready(Ok(())),
            Some(wait) => wait.as_mut().poll(cx),
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
    /// is a notification only: devices should kick off any tasks that need to
    /// run and return, without waiting. A device whose start can still fail
    /// once it returns reports that failure from [`Self::confirm_start`],
    /// which callers await before they consider the device started.
    ///
    // FUTURE: make this asynchronous if any device has a good reason to need
    // it. This may also require changing state units to wait for device start
    // to finish, which may require a bunch of other changes.
    fn start(&mut self);

    /// Waits for the start that [`Self::start`] began to finish, and fails if
    /// the device did not start.
    ///
    /// Callers call this after each [`Self::start`], before any other state
    /// change, and stop the device again if it fails. A device that fails
    /// keeps the state that it had before the start, so that stopping it
    /// leaves it as it was and a later start can try again.
    ///
    /// The default reports success, for devices whose start cannot fail.
    fn confirm_start(&mut self) -> impl Send + Future<Output = anyhow::Result<()>> {
        async { Ok(()) }
    }

    /// Stops accepting new host input before a snapshot vCPU boundary.
    ///
    /// The gate holds until [`Self::resume_input`]. Neither the guest's reset
    /// of the device nor [`Self::reset`] releases it, so the device's input
    /// stays gated for as long as its caller holds the gate.
    ///
    /// The vCPUs may run while the gate changes: the snapshot boundary gates
    /// host input before it stops them, and resumes a gate that failed before
    /// any of them stops. So a gate change has two parts. The caller makes
    /// this call, the synchronous part, holding the device as a vCPU's access
    /// to it does, so it must not wait. What remains to wait for, such as the
    /// answer of the device's task to a request that this call sent, it
    /// returns as an [`InputGateWait`], which does not borrow the device and
    /// which the caller awaits without holding it. A vCPU's access to the
    /// device thus comes before the synchronous part or after it, and is
    /// served in either case.
    fn quiesce_input(&mut self) -> InputGateWait {
        InputGateWait::done()
    }

    /// Resumes host input after a failed snapshot transaction.
    ///
    /// The vCPUs may run here too, so this makes the change's synchronous
    /// part and returns what remains to wait for, as [`Self::quiesce_input`]
    /// does.
    fn resume_input(&mut self) -> InputGateWait {
        InputGateWait::done()
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
    /// A host-input gate that [`Self::quiesce_input`] closed stays closed.
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

    /// Advances guest-visible device time after snapshot restore.
    fn advance_time(
        &mut self,
        _duration: Duration,
    ) -> impl Send + Future<Output = anyhow::Result<()>> {
        async { Ok(()) }
    }

    /// Returns whether [`Self::advance_time`] does any work for this device,
    /// because it keeps guest-visible time of its own instead of deriving its
    /// deadlines from VM time. Only such devices receive the advance after a
    /// restore, so a device that implements [`Self::advance_time`] must
    /// return `true`.
    fn advances_time(&self) -> bool {
        false
    }
}
