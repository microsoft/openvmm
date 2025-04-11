// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of [`VmgsLogger`] that sends GET events to the host.

use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::EventLogId;
use vmgs::logger::VmgsLogger;

/// An implementation of [`VmgsLogger`].
pub struct OpenHclVmgsLogger {
    get_client: GuestEmulationTransportClient,
}

impl OpenHclVmgsLogger {
    pub fn new(get_client: GuestEmulationTransportClient) -> Self {
        Self { get_client }
    }
}

#[async_trait::async_trait]
impl VmgsLogger for OpenHclVmgsLogger {
    async fn log_event_fatal(&self, event_id: EventLogId) {
        self.get_client.event_log_fatal(event_id).await
    }
}
