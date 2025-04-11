// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The definition of [`TpmLogger`] trait that enables TPM implementation
//! to send log events to the host.

use get_protocol::EventLogId;
use std::sync::Arc;
use tpm_resources::TpmLoggerKind;
use vm_resource::CanResolveTo;

impl CanResolveTo<ResolvedTpmLogger> for TpmLoggerKind {
    // Workaround for async_trait not supporting GATs with missing lifetimes.
    type Input<'a> = &'a ();
}

/// A resolved tpm logger resource.
pub struct ResolvedTpmLogger(pub Arc<dyn TpmLogger>);

impl<T: 'static + TpmLogger> From<T> for ResolvedTpmLogger {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
    }
}

/// A trait for sending log event to the host.
#[async_trait::async_trait]
pub trait TpmLogger: Send + Sync {
    /// Send a fatal event with the given id to the host.
    async fn log_event_fatal(&self, event_id: EventLogId);

    /// Send an event with the given id to the host.
    fn log_event(&self, event_id: EventLogId);
}
