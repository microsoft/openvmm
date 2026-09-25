// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Quiesce of a TAP queue.
//!
//! A quiesced queue drops its free RX buffers and stops reading from the TAP
//! interface, finishes writing its pending packet, and reports the
//! completions that it still holds.

use crate::TapQueue;
use anyhow::Context as _;
use net_backend::quiesce::QueueQuiesceResult;
use std::task::Poll;

impl TapQueue {
    /// Implements `quiesce`.
    pub(crate) async fn quiesce_queue(&mut self) -> anyhow::Result<QueueQuiesceResult> {
        self.input_quiesced = true;
        self.inner.rx_free.clear();
        std::future::poll_fn(|cx| {
            self.poll_pending_tx(cx);
            if self.tx.pending.is_none() || self.tx.error.is_some() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        if let Some(error) = self.tx.error.take() {
            self.inner.rx_ready.clear();
            return Err(error).context("failed to quiesce TAP transmit queue");
        }
        Ok(QueueQuiesceResult {
            rx_ready: self.inner.rx_ready.len(),
            tx_ready: self.tx.ready.len(),
        })
    }
}
