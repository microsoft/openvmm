// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Quiesce of a Consomme queue.
//!
//! A quiesced queue keeps transmitting guest frames, but it drops its posted
//! RX buffers, ignores new ones, and stops polling the Consomme stack and the
//! control channels until it is resumed.

use crate::ConsommeQueue;
use net_backend::BufferAccess;
use std::task::Poll;

impl ConsommeQueue {
    /// Implements `quiesce`.
    pub(crate) fn quiesce_queue(
        &mut self,
        pool: &mut dyn BufferAccess,
    ) -> anyhow::Result<net_backend::quiesce::QueueQuiesceResult> {
        self.input_quiesced = true;
        self.process_tx(pool);
        self.state.rx_avail.clear();
        Ok(net_backend::quiesce::QueueQuiesceResult {
            rx_ready: self.state.rx_ready.len(),
            tx_ready: self.state.tx_ready.len(),
        })
    }

    /// Implements `poll_ready` while the queue is quiesced: reports the
    /// pending completions without polling the stack or the control channels.
    pub(crate) fn poll_ready_quiesced(&self) -> Poll<()> {
        if !self.state.tx_ready.is_empty() || !self.state.rx_ready.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}
