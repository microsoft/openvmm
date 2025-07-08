// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use vmcore::non_volatile_store::NonVolatileStore;
use watchdog_core::platform::WatchdogCallback;
use watchdog_core::platform::WatchdogPlatform;
use watchdog_vmgs_format::WatchdogVmgsFormatStore;
use watchdog_vmgs_format::WatchdogVmgsFormatStoreError;

/// An implementation of [`WatchdogPlatform`] for use with both the UEFI
/// watchdog and the Guest Watchdog in HvLite.
pub struct HvLiteWatchdogPlatform {
    /// The VMGS store used to persist the watchdog status.
    store: WatchdogVmgsFormatStore,
    /// Callbacks to execute when the watchdog times out.
    callbacks: Vec<Box<dyn WatchdogCallback>>,
}

impl HvLiteWatchdogPlatform {
    pub async fn new(
        store: Box<dyn NonVolatileStore>,
    ) -> Result<Self, WatchdogVmgsFormatStoreError> {
        Ok(HvLiteWatchdogPlatform {
            store: WatchdogVmgsFormatStore::new(store).await?,
            callbacks: Vec::new(),
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

        for callback in &self.callbacks {
            callback.on_timeout().await;
        }
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

    fn add_callback(&mut self, callback: Box<dyn WatchdogCallback>) {
        self.callbacks.push(callback);
    }
}
