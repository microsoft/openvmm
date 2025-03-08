// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Vmbus channel offer support and related functionality.

#![forbid(unsafe_code)]

pub mod bus;
pub mod channel;
pub mod gpadl;
pub mod gpadl_ring;
pub mod offer;
pub mod resources;
pub mod simple;

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use vmbus_ring::FlatRingMem;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingRing;
use vmbus_ring::RingMem;
use vmcore::slim_event::SlimEvent;

/// An object to use to communicate notifications with the remote endpoint of a
/// vmbus channel.
///
/// When dropped, this may (or may not) signal to the remote endpoint that the
/// channel is being closed.
pub trait SignalVmbusChannel: Send + Sync {
    /// Signals the remote endpoint that there is work to do: either the
    /// outgoing ring buffer has transitioned from empty to non-empty, or the
    /// incoming ring buffer now has enough space for the remote endpoint's
    /// pending send.
    fn signal_remote(&self);

    /// Poll the channel for a ring buffer signal or for being closed.
    ///
    /// If the remote endpoint has closed the channel, then this can return
    /// [`ChannelClosed`].
    fn poll_for_signal(&self, cx: &mut Context<'_>) -> Poll<Result<(), ChannelClosed>>;
}

/// Error returned from [`SignalVmbusChannel::poll_for_signal`] indicating the channel
/// has been closed by the other endpoint.
pub struct ChannelClosed;

/// Creates a pair of connected channels. Useful for testing.
pub fn connected_async_channels(
    ring_size: usize,
) -> (RawAsyncChannel<FlatRingMem>, RawAsyncChannel<FlatRingMem>) {
    #[derive(Default)]
    struct EventWithDoneInner {
        event: SlimEvent,
        done: AtomicBool,
    }

    struct SignalInProc {
        local: Arc<EventWithDoneInner>,
        remote: Arc<EventWithDoneInner>,
        close_on_drop: bool,
    }

    impl SignalVmbusChannel for SignalInProc {
        fn signal_remote(&self) {
            self.remote.event.signal();
        }

        fn poll_for_signal(&self, cx: &mut Context<'_>) -> Poll<Result<(), ChannelClosed>> {
            if self.local.done.load(Ordering::Relaxed) {
                return Err(ChannelClosed).into();
            }
            self.local.event.poll_wait(cx).map(Ok)
        }
    }

    impl Drop for SignalInProc {
        fn drop(&mut self) {
            if self.close_on_drop {
                self.remote.done.store(true, Ordering::Relaxed);
                self.remote.event.signal();
            }
        }
    }

    let host_notify = SignalInProc {
        local: Default::default(),
        remote: Default::default(),
        close_on_drop: false,
    };
    let guest_notify = SignalInProc {
        local: host_notify.remote.clone(),
        remote: host_notify.local.clone(),
        close_on_drop: true,
    };
    let (host_in_ring, guest_out_ring) = make_ring_pair(ring_size);
    let (guest_in_ring, host_out_ring) = make_ring_pair(ring_size);

    let host = RawAsyncChannel {
        in_ring: host_in_ring,
        out_ring: host_out_ring,
        signal: Box::new(host_notify),
    };
    let guest = RawAsyncChannel {
        in_ring: guest_in_ring,
        out_ring: guest_out_ring,
        signal: Box::new(guest_notify),
    };
    (host, guest)
}

fn make_ring_pair(size: usize) -> (IncomingRing<FlatRingMem>, OutgoingRing<FlatRingMem>) {
    let flat_mem = FlatRingMem::new(size);
    let ring1 = IncomingRing::new(flat_mem.clone()).unwrap();
    let ring2 = OutgoingRing::new(flat_mem).unwrap();
    (ring1, ring2)
}

/// The resources for a bidirectional vmbus channel.
pub struct RawAsyncChannel<M: RingMem> {
    /// The incoming ring buffer.
    pub in_ring: IncomingRing<M>,
    /// The outgoing ring buffer.
    pub out_ring: OutgoingRing<M>,
    /// The object to use to communicate notifications with the remote endpoint.
    pub signal: Box<dyn SignalVmbusChannel>,
}
