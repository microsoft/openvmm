// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Platform hooks required by the watchdog device.
#[async_trait::async_trait]
pub trait WatchdogPlatform: Send {
    /// Callback fired when the timer expires.
    async fn on_timeout(&mut self);

    // Check if the watchdog previously timed-out, clearing the bit in the
    // process.
    async fn read_and_clear_boot_status(&mut self) -> bool;
}

/// A simple implementation of [`WatchdogPlatform`], suitable for ephemeral VMs.
pub struct SimpleWatchdogPlatform {
    watchdog_status: bool,
    cb: Box<dyn Fn() + Send + Sync>,
}

impl SimpleWatchdogPlatform {
    pub fn new(on_timeout: Box<dyn Fn() + Send + Sync>) -> Self {
        SimpleWatchdogPlatform {
            watchdog_status: false,
            cb: on_timeout,
        }
    }
}

#[async_trait::async_trait]
impl WatchdogPlatform for SimpleWatchdogPlatform {
    async fn on_timeout(&mut self) {
        self.watchdog_status = true;
        (self.cb)()
    }

    async fn read_and_clear_boot_status(&mut self) -> bool {
        if self.watchdog_status {
            self.watchdog_status = false;
        }
        self.watchdog_status
    }
}
