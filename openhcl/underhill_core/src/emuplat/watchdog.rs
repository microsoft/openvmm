// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An implementation of [`WatchdogPlatform`] for use with the Underhill
//! environment.
//!
//! This implementation wraps over [`BaseWatchdogPlatform`] in
//! [watchdog_core::platform::BaseWatchdogPlatform], providing additional
//! functionality specific to the Underhill environment.

use vmcore::non_volatile_store::NonVolatileStore;
use watchdog_core::platform::BaseWatchdogPlatform;
use watchdog_core::platform::WatchdogCallback;
use watchdog_core::platform::WatchdogPlatform;
use watchdog_vmgs_format::WatchdogVmgsFormatStoreError;

/// An implementation of [`WatchdogPlatform`] for the Underhill environment.
pub struct UnderhillWatchdogPlatform {
    /// The base watchdog platform implementation
    base: BaseWatchdogPlatform,
    /// Handle to the guest emulation transport client
    get: guest_emulation_transport::GuestEmulationTransportClient,
}

impl UnderhillWatchdogPlatform {
    pub async fn new(
        store: Box<dyn NonVolatileStore>,
        get: guest_emulation_transport::GuestEmulationTransportClient,
    ) -> Result<Self, WatchdogVmgsFormatStoreError> {
        Ok(UnderhillWatchdogPlatform {
            base: BaseWatchdogPlatform::new(store).await?,
            get,
        })
    }
}

#[async_trait::async_trait]
impl WatchdogPlatform for UnderhillWatchdogPlatform {
    async fn on_timeout(&mut self) {
        // Call the parent implementation first
        self.base.on_timeout().await;

        // FUTURE: consider emitting different events for the UEFI watchdog vs.
        // the guest watchdog
        //
        // NOTE: This must be done last to ensure that all callbacks
        // have been executed before we log the event.
        self.get
            .event_log_fatal(get_protocol::EventLogId::WATCHDOG_TIMEOUT_RESET)
            .await;
    }

    async fn read_and_clear_boot_status(&mut self) -> bool {
        self.base.read_and_clear_boot_status().await
    }

    fn add_callback(&mut self, callback: Box<dyn WatchdogCallback>) {
        self.base.add_callback(callback);
    }
}
