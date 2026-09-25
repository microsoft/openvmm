// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Queue quiesce.
//!
//! [`Queue::quiesce`](crate::Queue::quiesce) finishes the work that a queue
//! already accepted without accepting new work, and reports the completions
//! that the endpoint still holds in a [`QueueQuiesceResult`].
//! [`Queue::resume`](crate::Queue::resume) resumes host RX admission after a
//! rolled-back quiesce.

/// Completion ownership retained by an endpoint after quiesce.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueQuiesceResult {
    /// RX completions available through [`Queue::rx_poll`](crate::Queue::rx_poll).
    pub rx_ready: usize,
    /// TX completions available through [`Queue::tx_poll`](crate::Queue::tx_poll).
    pub tx_ready: usize,
}
