// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A watchdog timer device.
//!
//! This is not based on any real hardware, and is a bespoke to Hyper-V.
//!
//! This implementation is used by both the Hyper-V UEFI helper device, and the
//! Guest Watchdog device.

#![expect(missing_docs)]

pub mod platform;
use inspect::Inspect;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use thiserror::Error;
use vmcore::vmtime::VmTimeAccess;

#[derive(Debug, Error)]
pub enum WatchdogServiceError {
    #[error("attempted to set config with invalid bits: {0:08x?}")]
    InvalidConfigBits(u32),
    #[error("attempted to start watchdog with count set to zero")]
    ZeroCount,
    #[error("attempted to write to read-only Resolution register")]
    WriteResolution,
}

// Watchdog timer default period in seconds.
const BIOS_WATCHDOG_TIMER_PERIOD_S: u32 = 1;

// Watchdog timer default count (2 minutes).
const BIOS_WATCHDOG_DEFAULT_COUNT: u32 = (2 * 60) / BIOS_WATCHDOG_TIMER_PERIOD_S;

/// Values for the BIOS Watchdog Config register.
#[derive(Inspect)]
#[inspect(debug)]
#[bitfield_struct::bitfield(u32)]
struct ConfigBits {
    pub configured: bool,
    pub enabled: bool,
    #[bits(2)]
    _reserved: u32,
    /// Deprecated: Watchdog isn't configurable anymore
    pub one_shot: bool,
    #[bits(3)]
    _reserved2: u32,
    /// Enabled if previous reset was due to the watchdog
    pub boot_status: bool,
    #[bits(23)]
    _reserved3: u32,
}

impl ConfigBits {
    pub fn contains_unsupported_bits(&self) -> bool {
        u32::from(*self)
            & !u32::from(
                Self::new()
                    .with_configured(true)
                    .with_enabled(true)
                    .with_one_shot(true)
                    .with_boot_status(true),
            )
            != 0
    }
}

/// [`WatchdogServices`] device registers.
#[derive(Debug)]
pub enum Register {
    /// (RW) Used to configure the watchdog, set the mode, and temporarily
    /// suspend or resume the timer.
    Config,
    /// (RO) Contains the resolution of the hardware timer in seconds.
    Resolution,
    /// (RW) Used to specify expiration of the watchdog timer.
    ///
    /// A recommended default value can be read after the device is reset and
    /// after the watchdog is disabled via the Config register.
    Count,
}

#[derive(Clone, Copy, Debug, Inspect)]
pub struct WatchdogServicesState {
    // register state
    config: ConfigBits,
    resolution: u32,
    count: u32,
    // internal state
    configured_count: u32,
}

impl WatchdogServicesState {
    fn new() -> Self {
        Self {
            config: ConfigBits::new(),
            resolution: BIOS_WATCHDOG_TIMER_PERIOD_S,
            count: BIOS_WATCHDOG_DEFAULT_COUNT,
            configured_count: BIOS_WATCHDOG_DEFAULT_COUNT,
        }
    }
}

#[derive(Inspect)]
pub struct WatchdogServices {
    debug_id: String,
    // Runtime glue
    #[inspect(skip)]
    vmtime: VmTimeAccess,
    #[inspect(skip)]
    platform: Box<dyn platform::WatchdogPlatform>,

    // Volatile state
    #[inspect(flatten)]
    state: WatchdogServicesState,
}

impl WatchdogServices {
    pub async fn new(
        debug_id: impl Into<String>,
        vmtime: VmTimeAccess,
        platform: Box<dyn platform::WatchdogPlatform>,
        is_restoring: bool,
    ) -> WatchdogServices {
        let mut watchdog = WatchdogServices {
            debug_id: debug_id.into(),
            vmtime,
            platform,
            state: WatchdogServicesState::new(),
        };

        if !is_restoring {
            watchdog
                .state
                .config
                .set_boot_status(watchdog.platform.read_and_clear_boot_status().await);
        }

        watchdog
    }

    pub fn reset(&mut self) {
        self.state = WatchdogServicesState::new();
    }

    pub fn read(&mut self, reg: Register) -> Result<u32, WatchdogServiceError> {
        tracing::debug!(?reg, "read");

        let val = match reg {
            Register::Config => self.state.config.into(),
            Register::Resolution => self.state.resolution,
            Register::Count => self.state.count,
        };

        Ok(val)
    }

    pub fn write(&mut self, reg: Register, val: u32) -> Result<(), WatchdogServiceError> {
        tracing::debug!(?reg, "write {:x}", val);

        match reg {
            Register::Config => {
                self.state.config = {
                    let mut new_config = ConfigBits::from(val);
                    if new_config.contains_unsupported_bits() {
                        return Err(WatchdogServiceError::InvalidConfigBits(val));
                    }

                    // Setting the boot status is the protocol to clear it.
                    if new_config.boot_status() {
                        new_config.set_boot_status(false);
                    } else {
                        // Otherwise, make sure to preserve the old value
                        new_config.set_boot_status(self.state.config.boot_status());
                    }

                    // reset count to default if the timer is not longer configured
                    if !new_config.configured() {
                        self.state.count = 0;
                    }

                    new_config
                };

                if self.state.config.configured() && self.state.config.enabled() {
                    self.start_timer()?
                } else {
                    self.stop_timer()
                }
            }
            Register::Resolution => return Err(WatchdogServiceError::WriteResolution),
            Register::Count => {
                self.state.count = val;
                self.state.configured_count = val;
            }
        }

        Ok(())
    }

    fn start_timer(&mut self) -> Result<(), WatchdogServiceError> {
        let seconds = self.state.count * self.state.resolution;

        let next_tick = self
            .vmtime
            .now()
            .wrapping_add(Duration::from_secs(seconds as u64));
        self.state.count = self.state.configured_count;

        self.vmtime.set_timeout(next_tick);
        Ok(())
    }

    fn stop_timer(&mut self) {
        self.vmtime.cancel_timeout();
    }

    pub fn poll(&mut self, cx: &mut Context<'_>) {
        while let Poll::Ready(_now) = self.vmtime.poll_timeout(cx) {
            tracing::error!(name = self.debug_id, "Encountered a watchdog timeout");
            self.state.config.set_configured(false);
            self.state.config.set_enabled(false);
            pal_async::local::block_with_io(async |_driver| self.platform.on_timeout().await);
        }
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;

        #[derive(Protobuf)]
        #[mesh(package = "chipset.watchdog.core")]
        pub struct SavedState {
            #[mesh(1)]
            pub config: u32,
            #[mesh(2)]
            pub resolution: u32,
            #[mesh(3)]
            pub count: u32,
            #[mesh(4)]
            pub configured_count: u32,
        }
    }

    impl SaveRestore for WatchdogServices {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let WatchdogServicesState {
                config,
                resolution,
                count,
                configured_count,
            } = self.state;

            let saved_state = state::SavedState {
                config: config.into(),
                resolution,
                count,
                configured_count,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                config,
                resolution,
                count,
                configured_count,
            } = state;

            self.state = WatchdogServicesState {
                config: ConfigBits::from(config),
                resolution,
                count,
                configured_count,
            };

            Ok(())
        }
    }
}
