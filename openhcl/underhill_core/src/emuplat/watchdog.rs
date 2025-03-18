// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use vmcore::non_volatile_store::NonVolatileStore;
use watchdog_core::platform::WatchdogPlatform;
use watchdog_vmgs_format::WatchdogVmgsFormatStore;
use watchdog_vmgs_format::WatchdogVmgsFormatStoreError;

/// An implementation of [`WatchdogPlatform`] for use with both the UEFI
/// watchdog and the Guest Watchdog in Underhill.
pub struct UnderhillWatchdog {
    store: WatchdogVmgsFormatStore,
    get: guest_emulation_transport::GuestEmulationTransportClient,
    hook: Box<dyn WatchdogTimeout>,
}

#[async_trait::async_trait]
pub trait WatchdogTimeout: Send + Sync {
    async fn on_timeout(&self);
}

impl UnderhillWatchdog {
    pub async fn new(
        store: Box<dyn NonVolatileStore>,
        get: guest_emulation_transport::GuestEmulationTransportClient,
        hook: Box<dyn WatchdogTimeout>,
    ) -> Result<Self, WatchdogVmgsFormatStoreError> {
        Ok(UnderhillWatchdog {
            store: WatchdogVmgsFormatStore::new(store).await?,
            get,
            hook,
        })
    }
}

#[async_trait::async_trait]
impl WatchdogPlatform for UnderhillWatchdog {
    async fn on_timeout(&mut self) {
        let res = self.store.set_boot_failure().await;
        if let Err(e) = res {
            tracing::error!(
                error = &e as &dyn std::error::Error,
                "error persisting watchdog status"
            );
        }

        // FUTURE: consider emitting different events for the UEFI watchdog vs.
        // the guest watchdog
        self.get
            .event_log_fatal(get_protocol::EventLogId::WATCHDOG_TIMEOUT_RESET)
            .await;

        self.hook.on_timeout().await
    }

    async fn read_and_clear_boot_status(&mut self) -> bool {
        let res = self.store.read_and_clear_boot_status().await;
        match res {
            Ok(status) => status,
            Err(e) => {
                tracing::error!(
                    error = &e as &dyn std::error::Error,
                    "error reading watchdog status"
                );
                // assume no failure
                false
            }
        }
    }
}
