// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The definition of [`VmgsLogger`] trait that enables VMGS implementation
//! to send log events to the host.

use get_protocol::EventLogId;

/// A trait for sending log event to the host.
#[async_trait::async_trait]
pub trait VmgsLogger: Send + Sync {
    /// Send a fatal event with the given id to the host.
    async fn log_event_fatal(&self, event_id: EventLogId);
}
