// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Platform abstraction layer for watchdog timer devices.
//!
//! This module defines the interface between generic watchdog device implementations
//! (like UEFI watchdog) and the specific host environment where they run (OpenVMM,
//! OpenHCL, etc). The abstraction allows watchdog devices to remain platform-agnostic
//! while enabling each platform to handle timeouts appropriately - whether that's
//! logging, generating crash dumps, resetting VMs, or sending interrupts.
//!
//! The key traits are:
//! - [`WatchdogCallback`]: Implement this to define custom timeout behavior
//! - [`WatchdogPlatform`]: The interface that watchdog devices use to handle timeouts

/// Trait for responding to watchdog timeouts.
///
/// Implement this trait whenever you want to respond to watchdog timeouts, then pass
/// your instance to add_callback() on the [`WatchdogPlatform`] trait.
#[async_trait::async_trait]
pub trait WatchdogCallback: Send + Sync {
    /// Called when the watchdog timer expires
    async fn on_timeout(&self);
}

/// Blanket implementation of [`WatchdogCallback`] for closures.
///
/// This allows you to pass simple closures directly as callbacks without
/// needing to create a struct.
#[async_trait::async_trait]
impl<F> WatchdogCallback for F
where
    F: Fn() + Send + Sync,
{
    async fn on_timeout(&self) {
        self();
    }
}

/// Platform hooks required by the watchdog device.
#[async_trait::async_trait]
pub trait WatchdogPlatform: Send {
    /// Callback fired when the timer expires.
    async fn on_timeout(&mut self);

    // Check if the watchdog previously timed-out, clearing the bit in the
    // process.
    async fn read_and_clear_boot_status(&mut self) -> bool;

    /// Add a callback, which executes when the watchdog times out
    fn add_callback(&mut self, callback: Box<dyn WatchdogCallback>);
}

/// A simple implementation of [`WatchdogPlatform`], suitable for ephemeral VMs.
pub struct SimpleWatchdogPlatform {
    /// Whether the watchdog has timed out or not.
    watchdog_status: bool,
    /// Callbacks to execute when the watchdog times out.
    callbacks: Vec<Box<dyn WatchdogCallback>>,
}

impl SimpleWatchdogPlatform {
    pub fn new() -> Self {
        SimpleWatchdogPlatform {
            watchdog_status: false,
            callbacks: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl WatchdogPlatform for SimpleWatchdogPlatform {
    async fn on_timeout(&mut self) {
        self.watchdog_status = true;
        for callback in &self.callbacks {
            callback.on_timeout().await;
        }
    }

    async fn read_and_clear_boot_status(&mut self) -> bool {
        if self.watchdog_status {
            self.watchdog_status = false;
        }
        self.watchdog_status
    }

    fn add_callback(&mut self, callback: Box<dyn WatchdogCallback>) {
        self.callbacks.push(callback);
    }
}
