// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The definition of `VmgsLogger` trait that enables VMGS implementation
//! to send log events to an external logger.

use std::sync::Arc;

/// List of events for `VmgsLogger`.
pub enum VmgsLogEvent {
    /// Data store access failure.
    AccessFailed,
}

/// A trait for sending log event to the host.
#[async_trait::async_trait]
pub trait VmgsLogger: Send + Sync {
    /// Send a fatal event with the given id to the host.
    async fn log_event_fatal(&self, event: VmgsLogEvent);
}

#[async_trait::async_trait]
impl VmgsLogger for Option<Arc<dyn VmgsLogger>> {
    async fn log_event_fatal(&self, event: VmgsLogEvent) {
        if let Some(logger) = self {
            logger.log_event_fatal(event).await;
        }
    }
}
