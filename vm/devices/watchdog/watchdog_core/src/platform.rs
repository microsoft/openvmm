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

    /// Add a callback, which executes when the watchdog times out
    fn add_callback(&mut self, cb: Box<dyn Fn() + Send + Sync>);
}

/// A simple implementation of [`WatchdogPlatform`], suitable for ephemeral VMs.
pub struct SimpleWatchdogPlatform {
    /// Whether the watchdog has timed out or not.
    watchdog_status: bool,
    /// Callbacks to execute when the watchdog times out.
    callbacks: Vec<Box<dyn Fn() + Send + Sync>>,
}

impl SimpleWatchdogPlatform {
    pub fn new(on_timeout: Box<dyn Fn() + Send + Sync>) -> Self {
        SimpleWatchdogPlatform {
            watchdog_status: false,
            callbacks: vec![on_timeout],
        }
    }
}

#[async_trait::async_trait]
impl WatchdogPlatform for SimpleWatchdogPlatform {
    async fn on_timeout(&mut self) {
        self.watchdog_status = true;
        for cb in &self.callbacks {
            (cb)();
        }
    }

    async fn read_and_clear_boot_status(&mut self) -> bool {
        if self.watchdog_status {
            self.watchdog_status = false;
        }
        self.watchdog_status
    }

    fn add_callback(&mut self, cb: Box<dyn Fn() + Send + Sync>) {
        self.callbacks.push(cb);
    }
}
