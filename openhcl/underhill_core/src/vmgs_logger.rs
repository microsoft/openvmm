// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of [`VmgsLogger`] that sends GET events to the host.

use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::EventLogId;
use vmgs::logger::VmgsLogEvent;
use vmgs::logger::VmgsLogger;

/// An implementation of [`VmgsLogger`].
pub struct GetVmgsLogger {
    get_client: GuestEmulationTransportClient,
}

impl GetVmgsLogger {
    pub fn new(get_client: GuestEmulationTransportClient) -> Self {
        Self { get_client }
    }
}

#[async_trait::async_trait]
impl VmgsLogger for GetVmgsLogger {
    async fn log_event_fatal(&self, event: VmgsLogEvent) {
        let event_id = match event {
            VmgsLogEvent::AccessFailed => EventLogId::VMGS_ACCESS_FAILED,
        };

        self.get_client.event_log_fatal(event_id).await
    }
}
