// Copyright (C) Microsoft Corporation. All rights reserved.

use vmcore::non_volatile_store::NonVolatileStore;
use watchdog_core::platform::WatchdogPlatform;
use watchdog_vmgs_format::WatchdogVmgsFormatStore;
use watchdog_vmgs_format::WatchdogVmgsFormatStoreError;

/// An implementation of [`WatchdogPlatform`] for use with both the UEFI
/// watchdog and the Guest Watchdog in HvLite.
pub struct HvLiteWatchdogPlatform {
    store: WatchdogVmgsFormatStore,
    on_timeout: Box<dyn Fn() + Send + Sync>,
}

impl HvLiteWatchdogPlatform {
    pub async fn new(
        store: Box<dyn NonVolatileStore>,
        on_timeout: Box<dyn Fn() + Send + Sync>,
    ) -> Result<Self, WatchdogVmgsFormatStoreError> {
        Ok(HvLiteWatchdogPlatform {
            store: WatchdogVmgsFormatStore::new(store).await?,
            on_timeout,
        })
    }
}

#[async_trait::async_trait]
impl WatchdogPlatform for HvLiteWatchdogPlatform {
    async fn on_timeout(&mut self) {
        let res = self.store.set_boot_failure().await;
        if let Err(e) = res {
            tracing::error!(
                error = &e as &dyn std::error::Error,
                "error persisting watchdog status"
            );
        }

        (self.on_timeout)()
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
