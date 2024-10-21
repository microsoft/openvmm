// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Internal types for performing asynchronous channel IO.

use inspect::Inspect;
use inspect_counters::Counter;
use pal_async::multi_waker::MultiWaker;
use std::task::Context;
use std::task::Poll;
use vmbus_channel::ChannelClosed;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::SignalVmbusChannel;
use vmbus_ring as ring;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingOffset;
use vmbus_ring::OutgoingRing;
use vmbus_ring::RingMem;

pub struct Core<M: RingMem> {
    signal: Box<dyn SignalVmbusChannel>,
    multi_waker: MultiWaker<2>,
    in_ring: IncomingRing<M>,
    out_ring: OutgoingRing<M>,
}

impl<M: RingMem> Inspect for Core<M> {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("incoming_ring", &self.in_ring)
            .field("outgoing_ring", &self.out_ring);
    }
}

impl<M: RingMem> Core<M> {
    pub fn new(channel: RawAsyncChannel<M>) -> Self {
        let RawAsyncChannel {
            in_ring,
            out_ring,
            signal,
        } = channel;
        Self {
            signal,
            multi_waker: MultiWaker::new(),
            in_ring,
            out_ring,
        }
    }

    pub fn in_ring(&self) -> &IncomingRing<M> {
        &self.in_ring
    }

    pub fn out_ring(&self) -> &OutgoingRing<M> {
        &self.out_ring
    }
}

#[derive(Debug)]
pub(crate) enum PollError {
    Ring(ring::Error),
    Closed,
}

impl<M: RingMem> Core<M> {
    fn poll_ready(&self, cx: &mut Context<'_>, for_outgoing: bool) -> Poll<Result<(), PollError>> {
        // Poll, wrapping the context with a multi waker context so that both
        // the incoming and outgoing tasks will be woken when there is a signal.
        self.multi_waker
            .poll_wrapped(cx, for_outgoing.into(), |cx| {
                self.signal
                    .poll_for_signal(cx)
                    .map_err(|ChannelClosed| PollError::Closed)
            })
    }
}

impl<M: RingMem> Core<M> {
    pub fn signal(&self) {
        self.signal.signal_remote();
    }
}

#[derive(Debug, Inspect)]
pub struct ReadState {
    #[inspect(flatten)]
    pub ptrs: ring::IncomingOffset,
    pub polls: Counter,
    pub signals: Counter,
    ready: bool,
    masked: bool,
}

impl ReadState {
    pub fn new(ptrs: ring::IncomingOffset) -> Self {
        Self {
            ptrs,
            polls: Counter::new(),
            signals: Counter::new(),
            ready: false,
            // It's safe to assume interrupts are initially masked, since
            // setting the mask is an optimization but clearing it is required
            // to avoid missing notifications.
            masked: true,
        }
    }

    /// Polls the incoming ring for readiness.
    pub fn poll_ready<M: RingMem>(
        &mut self,
        cx: &mut Context<'_>,
        core: &Core<M>,
    ) -> Poll<Result<(), PollError>> {
        while !self.ready {
            // The ring buffer is believed to be empty. Unmask interrupts before
            // double checking the ring buffer.
            if self.masked {
                core.in_ring.set_interrupt_mask(false);
                self.masked = false;
            } else {
                // Interrupts are not supposed to be masked at this point.
                // Detect ring control corruption here to avoid hard to diagnose
                // issues later.
                core.in_ring
                    .verify_interrupts_unmasked()
                    .map_err(PollError::Ring)?;
            }

            if core
                .in_ring
                .can_read(&mut self.ptrs)
                .map_err(PollError::Ring)?
            {
                // The ring has packets.
                //
                // N.B. There is no need to mask interrupts again until just
                // before packets are removed from the ring, since the opposite
                // endpoint will not signal until there is an empty-to-non-empty
                // transition.
                self.ready = true;
            } else {
                std::task::ready!(core.poll_ready(cx, false))?;
                self.polls.increment();
            }
        }
        Poll::Ready(Ok(()))
    }

    /// Clears the cached ready state. Should be called when the ring buffer is
    /// known to be empty.
    pub fn clear_ready(&mut self) {
        self.ready = false;
    }

    /// Clears the request for a wakeup when the ring is ready.
    ///
    /// Should be called just before removing packets from the ring so that the
    /// opposite endpoint does not signal the ring-non-empty condition
    /// unnecessarily.
    pub fn clear_poll<M: RingMem>(&mut self, core: &Core<M>) {
        if !self.masked {
            core.in_ring.set_interrupt_mask(true);
            self.masked = true;
        }
    }
}

#[derive(Debug, Inspect)]
pub struct WriteState {
    #[inspect(flatten)]
    pub ptrs: OutgoingOffset,
    pub signals: Counter,
    pub polls: Counter,
    ready: bool,
    pending_size: usize,
}

impl WriteState {
    pub fn new(ptrs: OutgoingOffset) -> Self {
        Self {
            ptrs,
            signals: Counter::new(),
            polls: Counter::new(),
            ready: false,
            pending_size: 0,
        }
    }

    /// Polls the outgoing ring for readiness to send `send_size` bytes.
    pub fn poll_ready<M: RingMem>(
        &mut self,
        cx: &mut Context<'_>,
        core: &Core<M>,
        send_size: usize,
    ) -> Poll<Result<(), PollError>> {
        while !self.ready {
            // The ring buffer is believed to be full. Set the pending send size
            // before double checking the ring buffer.
            if self.pending_size < send_size {
                // Since there is no rush to get data into a full ring,
                // delay the signal until at least 1/4 of the ring is
                // available (and until this packet fits) to avoid ping
                // ponging with the opposite endpoint.
                self.pending_size = send_size.max(core.out_ring().maximum_packet_size() / 4);
                core.out_ring
                    .set_pending_send_size(self.pending_size)
                    .map_err(PollError::Ring)?;
            }
            if core
                .out_ring
                .can_write(&mut self.ptrs, send_size)
                .map_err(PollError::Ring)?
            {
                self.ready = true;
                // Clear the pending send size now if it's larger than the
                // requested send size, since otherwise spurious interrupts may
                // arrive.
                if self.pending_size > send_size {
                    self.clear_poll(core);
                }
            } else {
                std::task::ready!(core.poll_ready(cx, true))?;
                self.polls.increment();
            }
        }
        Poll::Ready(Ok(()))
    }

    /// Clears the cached ready state. Should be called when the ring buffer is
    /// known to be full.
    pub fn clear_ready(&mut self) {
        self.ready = false;
    }

    /// Clears the request for a wakeup when the ring is ready.
    ///
    /// Should be called just before inserting packets into the ring so that the
    /// opposite endpoint does not signal the ring-non-full condition
    /// unnecessarily.
    pub fn clear_poll<M: RingMem>(&mut self, core: &Core<M>) {
        if self.pending_size != 0 {
            core.out_ring.set_pending_send_size(0).unwrap();
            self.pending_size = 0;
        }
    }
}
