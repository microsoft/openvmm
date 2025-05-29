// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The definition of [`TpmLogger`] trait that enables TPM implementation
//! to send log events to an external logger.

use std::sync::Arc;
use tpm_resources::TpmLoggerKind;
use vm_resource::CanResolveTo;

/// Events for [`TpmLogger`].
pub enum TpmLogEvent {
    /// Failed to renew AK cert
    AkCertRenewalFailed,
    /// Failed to change TPM seeds
    IdentityChangeFailed,
    /// Invalid PPI or NVRAM state
    InvalidState,
}

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
    /// Send an event with the given id to the host and flush.
    async fn log_event_and_flush(&self, event: TpmLogEvent);

    /// Send an event with the given id to the host without flushing.
    // TODO: This call is needed for the non-async context (callback of
    // `PollDevice::poll_device` for AK cert requests). Remove the function
    // once we do not have this constraint.
    fn log_event(&self, event: TpmLogEvent);
}

#[async_trait::async_trait]
impl TpmLogger for Option<Arc<dyn TpmLogger>> {
    async fn log_event_and_flush(&self, event: TpmLogEvent) {
        if let Some(logger) = self {
            logger.log_event_and_flush(event).await;
        }
    }

    fn log_event(&self, event: TpmLogEvent) {
        if let Some(logger) = self {
            logger.log_event(event);
        }
    }
}
