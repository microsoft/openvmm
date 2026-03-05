// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code for managing socket readiness.

use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use crate::interest::PollInterestSet;
use crate::waker::WakerList;
use pal::windows::Overlapped;
use pal::windows::SendSyncRawHandle;
use pal::windows::afd;
use pal::windows::status_to_error;
use parking_lot::Mutex;
use std::cell::UnsafeCell;
use std::os::windows::prelude::*;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use winapi::shared::ntdef::NT_SUCCESS;
use winapi::shared::ntdef::NTSTATUS;
use winapi::shared::ntstatus::STATUS_CANCELLED;
use winapi::shared::ntstatus::STATUS_PENDING;
use winapi::shared::ntstatus::STATUS_SUCCESS;
use winapi::um::ioapiset::CancelIoEx;
use winapi::um::minwinbase::OVERLAPPED;

pub fn make_poll_handle_info(handle: RawHandle, events: PollEvents) -> afd::PollHandleInfo {
    let mut afd_events = afd::POLL_ABORT | afd::POLL_CONNECT_FAIL;
    if events.has_in() {
        afd_events |= afd::POLL_RECEIVE | afd::POLL_ACCEPT | afd::POLL_DISCONNECT;
    }
    if events.has_out() {
        afd_events |= afd::POLL_SEND;
    }
    if events.has_pri() {
        afd_events |= afd::POLL_RECEIVE_EXPEDITED;
    }
    if events.has_rdhup() {
        afd_events |= afd::POLL_DISCONNECT;
    }
    afd::PollHandleInfo {
        handle: SendSyncRawHandle(handle),
        events: afd_events,
        status: STATUS_PENDING,
    }
}

pub fn parse_poll_handle_info(info: &afd::PollHandleInfo) -> PollEvents {
    let mut revents = PollEvents::EMPTY;
    // N.B. info.status may be an error code for
    // POLL_CONNECT_FAIL. This error should be
    // retrievable by calling connect again, so we do
    // not need to return it or store it anywhere.

    if info.events & afd::POLL_ABORT != 0 {
        revents |= PollEvents::IN | PollEvents::HUP;
    }
    if info.events
        & (afd::POLL_RECEIVE | afd::POLL_ACCEPT | afd::POLL_DISCONNECT | afd::POLL_CONNECT_FAIL)
        != 0
    {
        revents |= PollEvents::IN;
    }
    if info.events & (afd::POLL_SEND | afd::POLL_CONNECT_FAIL) != 0 {
        revents |= PollEvents::OUT;
    }
    if info.events & afd::POLL_CONNECT_FAIL != 0 {
        revents |= PollEvents::ERR;
    }
    if info.events & afd::POLL_RECEIVE_EXPEDITED != 0 {
        revents |= PollEvents::PRI;
    }
    if info.events & afd::POLL_DISCONNECT != 0 {
        revents |= PollEvents::RDHUP;
    }

    revents
}

#[derive(Debug)]
pub struct AfdSocketReady {
    op: Arc<AfdSocketReadyOp>,
}

pub trait AfdHandle {
    fn handle(&self) -> RawHandle;

    fn ref_io(&self) -> RawHandle;

    /// # Safety
    ///
    /// Must only be called when an IO operation started with `ref_io` will not
    /// complete asynchronously (either it has completed synchronously or will
    /// never be issued).
    unsafe fn deref_io(&self);
}

#[repr(C)]
#[derive(Debug)]
struct AfdSocketReadyOp {
    overlapped: Overlapped, // must be first so that this type can be cast from *mut OVERLAPPED
    socket: RawSocket,
    poll_info: KernelBuffer<PollInfoInput>,
    inner: Mutex<AfdSocketReadyInner>,
}

#[repr(transparent)]
#[derive(Debug)]
struct KernelBuffer<T>(UnsafeCell<T>);

/// SAFETY: the buffer is `Sync` if the contents are `Sync`.
unsafe impl<T: Sync> Sync for KernelBuffer<T> {}

#[repr(C)]
#[derive(Debug, Default)]
struct PollInfoInput {
    header: afd::PollInfo,
    data: afd::PollHandleInfo,
}

#[derive(Debug)]
struct AfdSocketReadyInner {
    interests: PollInterestSet,
    in_flight_events: PollEvents,
    cancelled: bool,
}

impl AfdSocketReady {
    pub fn new(socket: RawSocket) -> Self {
        Self {
            op: Arc::new(AfdSocketReadyOp {
                overlapped: Overlapped::new(),
                socket,
                poll_info: KernelBuffer(UnsafeCell::new(Default::default())),
                inner: Mutex::new(AfdSocketReadyInner {
                    interests: PollInterestSet::default(),
                    in_flight_events: PollEvents::EMPTY,
                    cancelled: false,
                }),
            }),
        }
    }

    pub fn poll_socket_ready(
        &mut self,
        cx: &mut Context<'_>,
        afd_handle: &impl AfdHandle,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        let mut wakers = WakerList::default();
        // Hold the lock across all issuing and cancelling of IO in order to
        // synchronize with the IO completion thread. This avoids race
        // conditions where the IO completion thread tries to issue a poll IO
        // for a socket that has just been closed (which can cause an invalid
        // handle access), or where cancelling an IO happens concurrently with
        // issuing an IO (which can cause a hang).
        //
        // There are probably a few cases where a shorter lock hold time is
        // possible, but it's far from clear that doing so would be a win.
        let mut inner = self.op.inner.lock();
        let r = loop {
            match inner.interests.poll_ready(cx, slot, events) {
                Poll::Ready(events) => break Poll::Ready(events),
                Poll::Pending => {
                    if events & inner.in_flight_events == events || inner.cancelled {
                        // An IO with the appropriate events is already in
                        // flight, or one will be issued soon.
                        break Poll::Pending;
                    } else if inner.in_flight_events.is_empty() {
                        // The IO is not in flight.
                        let events_to_poll = inner.interests.events_to_poll();
                        if self.op.issue_io(&mut inner, afd_handle, events_to_poll) {
                            self.op.io_complete(
                                &mut inner,
                                afd_handle,
                                STATUS_SUCCESS,
                                &mut wakers,
                            );
                        } else {
                            break Poll::Pending;
                        }
                    } else {
                        // An IO is already in flight but with the wrong events.
                        // Cancel it--it will be reissued with the right events
                        // when it completes.
                        inner.cancelled = true;
                        self.op.cancel_io(afd_handle);
                    }
                }
            }
        };
        wakers.wake();
        r
    }

    pub fn clear_socket_ready(&mut self, slot: InterestSlot) {
        self.op.inner.lock().interests.clear_ready(slot)
    }

    pub fn teardown(&mut self, afd_handle: &impl AfdHandle) {
        let mut inner = self.op.inner.lock();
        inner.interests.clear_all();
        if !inner.in_flight_events.is_empty() {
            drop(inner);
            self.op.cancel_io(afd_handle);
        }
    }

    /// Reports an AFD IO completion.
    ///
    /// # Safety
    /// Must be called only when an IO has completed for `overlapped`, which
    /// must be a pointer to the `overlapped` field of an `AfdSocketReadyOp`.
    /// This must have been started with a call to `AfdSocketReadyOp::issue_io`.
    pub unsafe fn io_complete(
        afd_handle: &impl AfdHandle,
        overlapped: *mut OVERLAPPED,
        wakers: &mut WakerList,
    ) {
        let op_ptr = overlapped.cast::<AfdSocketReadyOp>();
        // SAFETY: caller ensures `overlapped` is a valid pointer to the
        // `overlapped` field of an `AfdSocketReadyOp` that has completed.
        let op = unsafe { &*op_ptr };
        let (status, _) = op.overlapped.io_status().expect("io should be done");
        let mut inner = op.inner.lock();
        // SAFETY: Now that the lock is held, the issuer is guaranteed to have
        // incremented the reference count, so it is safe to take ownership of
        // the reference.
        let op = unsafe { Arc::from_raw(op_ptr) };
        op.io_complete(&mut inner, afd_handle, status, wakers);
        // Drop the lock before `op`, since dropping `op` may free the
        // `AfdSocketReadyOp` (and the mutex it contains) if this is the last
        // reference.
        drop(inner);
    }
}

impl AfdSocketReadyOp {
    /// Returns whether the IO completed synchronously.
    #[must_use]
    fn issue_io(
        self: &Arc<Self>,
        inner: &mut AfdSocketReadyInner,
        afd_handle: &impl AfdHandle,
        events: PollEvents,
    ) -> bool {
        inner.in_flight_events = events;
        // SAFETY: there is no IO in flight, so we have exclusive access to the
        // poll info buffer.
        let poll_info = unsafe { &mut *self.poll_info.0.get() };
        *poll_info = PollInfoInput {
            header: afd::PollInfo {
                timeout: i64::MAX,
                number_of_handles: 1,
                exclusive: 0,
            },
            data: make_poll_handle_info(self.socket as RawHandle, events),
        };

        let len = size_of_val(poll_info);
        // SAFETY: the buffers are valid and owned for the lifetime of the
        // operation, and the handles are valid for the lifetime of the call.
        let done = unsafe {
            afd::poll(
                afd_handle.ref_io(),
                &mut poll_info.header,
                len,
                self.overlapped.as_ptr(),
            )
        };

        if done {
            // SAFETY: the IO completed synchronously, so the IO reference is no
            // longer in use.
            unsafe { afd_handle.deref_io() };
            true
        } else {
            // The IO will drop a reference, so increment it here. Note that
            // this is safe to do even though the IO may complete immediately,
            // because the IO completion callback takes the inner lock that this
            // thread currently holds.
            let _proof = inner;
            let _ = Arc::into_raw(self.clone());
            false
        }
    }

    fn cancel_io(&self, afd_handle: &impl AfdHandle) {
        // SAFETY: no safety requirements.
        unsafe {
            CancelIoEx(afd_handle.handle(), self.overlapped.as_ptr());
        }
    }

    /// The inner lock must be held across this call. This is necessary both to
    /// ensure that the IO can be cancelled by another thread, and to ensure
    /// that the socket handle is still valid at the time of the call.
    fn io_complete(
        self: &Arc<Self>,
        inner: &mut AfdSocketReadyInner,
        afd_handle: &impl AfdHandle,
        mut status: NTSTATUS,
        wakers: &mut WakerList,
    ) {
        loop {
            let revents = if NT_SUCCESS(status) {
                // SAFETY: there is no IO in flight, so we have exclusive access to the
                // poll info buffer.
                let poll_info = unsafe { &mut *self.poll_info.0.get() };
                assert_eq!(poll_info.header.number_of_handles, 1);
                parse_poll_handle_info(&poll_info.data)
            } else {
                assert_eq!(
                    status,
                    STATUS_CANCELLED,
                    "unexpected afd poll failure: {}",
                    status_to_error(status)
                );
                PollEvents::EMPTY
            };

            inner.interests.wake_ready(revents, wakers);
            inner.in_flight_events = PollEvents::EMPTY;
            inner.cancelled = false;
            let next_events = inner.interests.events_to_poll();
            if next_events.is_empty() {
                break;
            }
            if self.issue_io(inner, afd_handle, next_events) {
                status = STATUS_SUCCESS;
            } else {
                break;
            }
        }
    }
}
