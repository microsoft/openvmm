// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NetVSP test helpers.
//!
//! These are used both by unit tests and by the fuzzer.

#![allow(dead_code)]

use guestmem::GuestMemory;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use vmbus_channel::ChannelClosed;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::SignalVmbusChannel;
use vmbus_channel::gpadl::GpadlId;
use vmbus_channel::gpadl::GpadlMapView;
use vmbus_channel::gpadl_ring::AlignedGpadlView;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingRing;
use vmcore::interrupt::Interrupt;
use vmcore::slim_event::SlimEvent;

/// A [`SignalVmbusChannel`] implementation for test guest channels that
/// supports cooperative shutdown via a shared `done` flag.
pub struct EventWithDone {
    /// Interrupt to signal the remote (host) side.
    pub remote_interrupt: Interrupt,
    /// Event to wait on for local (guest) signals from the host.
    pub local_event: Arc<SlimEvent>,
    /// When set to true, `poll_for_signal` returns `ChannelClosed`.
    pub done: Arc<AtomicBool>,
}

impl SignalVmbusChannel for EventWithDone {
    fn signal_remote(&self) {
        self.remote_interrupt.deliver();
    }

    fn poll_for_signal(&self, cx: &mut Context<'_>) -> Poll<Result<(), ChannelClosed>> {
        if self.done.load(Ordering::Relaxed) {
            return Err(ChannelClosed).into();
        }
        self.local_event.poll_wait(cx).map(Ok)
    }
}

/// Create the incoming and outgoing rings for a guest-side test channel backed
/// by a GPADL.
pub fn make_test_guest_rings(
    mem: &GuestMemory,
    gpadl_map: &GpadlMapView,
    gpadl_id: GpadlId,
    ring_offset: u32,
) -> (IncomingRing<GpadlRingMem>, OutgoingRing<GpadlRingMem>) {
    let gpadl = AlignedGpadlView::new(gpadl_map.map(gpadl_id).unwrap()).unwrap();
    let (out_gpadl, in_gpadl) = match gpadl.split(ring_offset) {
        Ok(gpadls) => gpadls,
        Err(_) => panic!("Failed gpadl.split"),
    };
    (
        IncomingRing::new(GpadlRingMem::new(in_gpadl, mem).unwrap()).unwrap(),
        OutgoingRing::new(GpadlRingMem::new(out_gpadl, mem).unwrap()).unwrap(),
    )
}

/// Build a [`RawAsyncChannel`] backed by GPADL ring memory, suitable for
/// constructing a guest-side [`vmbus_async::queue::Queue`].
pub fn gpadl_test_guest_channel(
    mem: &GuestMemory,
    gpadl_map: &GpadlMapView,
    gpadl_id: GpadlId,
    ring_offset: u32,
    host_to_guest_event: Arc<SlimEvent>,
    guest_to_host_interrupt: Interrupt,
    done: Arc<AtomicBool>,
) -> RawAsyncChannel<GpadlRingMem> {
    let (in_ring, out_ring) = make_test_guest_rings(mem, gpadl_map, gpadl_id, ring_offset);
    RawAsyncChannel {
        in_ring,
        out_ring,
        signal: Box::new(EventWithDone {
            local_event: host_to_guest_event,
            remote_interrupt: guest_to_host_interrupt,
            done,
        }),
    }
}
