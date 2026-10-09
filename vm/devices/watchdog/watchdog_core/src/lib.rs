// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A watchdog timer device.
//!
//! This is not based on any real hardware, and is a bespoke to Hyper-V.
//!
//! This implementation is used by both the Hyper-V UEFI helper device, and the
//! Guest Watchdog device.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod platform;
pub mod resources;
use inspect::Inspect;
use std::future::Future;
use std::pin::Pin;
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
    /// The timer fired and the platform's timeout action has not finished.
    timeout_pending: bool,
}

impl WatchdogServicesState {
    fn new() -> Self {
        Self {
            config: ConfigBits::new(),
            resolution: BIOS_WATCHDOG_TIMER_PERIOD_S,
            count: BIOS_WATCHDOG_DEFAULT_COUNT,
            configured_count: BIOS_WATCHDOG_DEFAULT_COUNT,
            timeout_pending: false,
        }
    }
}

/// The platform's `on_timeout`, holding the platform until it finishes.
type TimeoutAction = Pin<Box<dyn Future<Output = Box<dyn platform::WatchdogPlatform>> + Send>>;

#[derive(Inspect)]
pub struct WatchdogServices {
    debug_id: String,
    // Runtime glue
    #[inspect(skip)]
    vmtime: VmTimeAccess,
    /// `None` while `timeout_action` owns it.
    #[inspect(skip)]
    platform: Option<Box<dyn platform::WatchdogPlatform>>,
    #[inspect(skip)]
    timeout_action: Option<TimeoutAction>,

    // Volatile state
    #[inspect(flatten)]
    state: WatchdogServicesState,
}

impl WatchdogServices {
    pub async fn new(
        debug_id: impl Into<String>,
        vmtime: VmTimeAccess,
        mut platform: Box<dyn platform::WatchdogPlatform>,
        is_restoring: bool,
    ) -> WatchdogServices {
        let mut state = WatchdogServicesState::new();
        if !is_restoring {
            state
                .config
                .set_boot_status(platform.read_and_clear_boot_status().await);
        }

        WatchdogServices {
            debug_id: debug_id.into(),
            vmtime,
            platform: Some(platform),
            timeout_action: None,
            state,
        }
    }

    pub fn reset(&mut self) {
        // An in-flight timeout action keeps running: it owns the platform, so
        // dropping it would lose the platform, and the timeout did happen.
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
        loop {
            if let Some(action) = &mut self.timeout_action {
                let Poll::Ready(platform) = action.as_mut().poll(cx) else {
                    return;
                };
                self.timeout_action = None;
                self.platform = Some(platform);
                self.state.timeout_pending = false;
            }

            // A restored state can owe the action without the timer firing again.
            if !self.state.timeout_pending {
                let Poll::Ready(_now) = self.vmtime.poll_timeout(cx) else {
                    return;
                };
                tracing::error!(name = self.debug_id, "Encountered a watchdog timeout");
                self.state.config.set_configured(false);
                self.state.config.set_enabled(false);
                self.state.timeout_pending = true;
            }

            // Poll the action instead of blocking on it: the platform persists
            // the timeout to its store first, and the store can be served by
            // the thread polling this device.
            // Only an in-flight action holds the platform, and that case
            // returned or handed it back at the top of the loop.
            let Some(mut platform) = self.platform.take() else {
                return;
            };
            self.timeout_action = Some(Box::pin(async move {
                platform.on_timeout().await;
                platform
            }));
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
            #[mesh(5)]
            pub timeout_pending: bool,
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
                timeout_pending,
            } = self.state;

            let saved_state = state::SavedState {
                config: config.into(),
                resolution,
                count,
                configured_count,
                timeout_pending,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                config,
                resolution,
                count,
                configured_count,
                timeout_pending,
            } = state;

            self.state = WatchdogServicesState {
                config: ConfigBits::from(config),
                resolution,
                count,
                configured_count,
                timeout_pending,
            };

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::WatchdogCallback;
    use crate::platform::WatchdogPlatform;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use vmcore::save_restore::SaveRestore;
    use vmcore::vmtime::SavedState as VmTimeSavedState;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    /// A platform whose timeout action waits for a permit, standing in for a
    /// store served by the thread that polls the device.
    struct GatedPlatform {
        permit: mesh::Receiver<()>,
        acted: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl WatchdogPlatform for GatedPlatform {
        async fn on_timeout(&mut self) {
            self.permit.recv().await.unwrap();
            self.acted.store(true, Ordering::SeqCst);
        }

        async fn read_and_clear_boot_status(&mut self) -> bool {
            false
        }

        fn add_callback(&mut self, _callback: Box<dyn WatchdogCallback>) {}
    }

    struct TestWatchdog {
        watchdog: WatchdogServices,
        keeper: VmTimeKeeper,
        permit: mesh::Sender<()>,
        acted: Arc<AtomicBool>,
    }

    async fn test_watchdog(driver: &DefaultDriver) -> TestWatchdog {
        let keeper = VmTimeKeeper::new(driver, VmTime::from_100ns(0));
        let vmtime = keeper.builder().build(driver).await.unwrap();
        let (permit, permit_recv) = mesh::channel();
        let acted = Arc::new(AtomicBool::new(false));
        let platform = GatedPlatform {
            permit: permit_recv,
            acted: acted.clone(),
        };
        let watchdog =
            WatchdogServices::new("test", vmtime.access("watchdog"), Box::new(platform), false)
                .await;
        TestWatchdog {
            watchdog,
            keeper,
            permit,
            acted,
        }
    }

    /// Polls once. With a blocking timeout action this call never returns.
    fn poll_once(watchdog: &mut WatchdogServices) {
        let waker = std::task::Waker::noop();
        watchdog.poll(&mut Context::from_waker(waker));
    }

    /// Drives the device until the platform has carried out the timeout.
    async fn drive_until_acted(watchdog: &mut WatchdogServices, acted: &AtomicBool) {
        std::future::poll_fn(|cx| {
            watchdog.poll(cx);
            if acted.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    /// Arms a one-second count, then moves the stopped clock to `secs`, so
    /// the timer is due on the next poll without waiting for real time.
    async fn arm_and_expire(watchdog: &mut WatchdogServices, keeper: &mut VmTimeKeeper, secs: u64) {
        watchdog.write(Register::Count, 1).unwrap();
        watchdog
            .write(
                Register::Config,
                ConfigBits::new()
                    .with_configured(true)
                    .with_enabled(true)
                    .into(),
            )
            .unwrap();
        keeper
            .restore(VmTimeSavedState::from_vmtime(VmTime::from_100ns(
                secs * 10_000_000,
            )))
            .await;
    }

    /// A timeout must not block the polling thread on the platform's action:
    /// that thread can be the one the action is waiting for.
    #[async_test]
    async fn timeout_action_is_polled_not_blocked_on(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            mut keeper,
            permit,
            acted,
        } = test_watchdog(&driver).await;

        arm_and_expire(&mut watchdog, &mut keeper, 2).await;

        poll_once(&mut watchdog);
        assert!(watchdog.state.timeout_pending);
        assert!(!acted.load(Ordering::SeqCst));
        assert!(!watchdog.state.config.enabled());

        permit.send(());
        drive_until_acted(&mut watchdog, &acted).await;
        assert!(!watchdog.state.timeout_pending);
        assert!(!watchdog.save().unwrap().timeout_pending);
    }

    /// A state saved while the action was in flight must still carry it out
    /// after a restore, rather than drop it.
    #[async_test]
    async fn restored_pending_timeout_runs_the_action(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            keeper: _keeper,
            permit,
            acted,
        } = test_watchdog(&driver).await;

        let mut saved = watchdog.save().unwrap();
        saved.timeout_pending = true;
        watchdog.restore(saved).unwrap();

        poll_once(&mut watchdog);
        assert!(!acted.load(Ordering::SeqCst));

        permit.send(());
        drive_until_acted(&mut watchdog, &acted).await;
        assert!(!watchdog.save().unwrap().timeout_pending);
    }

    /// A reset while the action is in flight must not lose the platform: the
    /// action still finishes, and the next timeout runs it again.
    #[async_test]
    async fn reset_during_action_keeps_the_platform(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            mut keeper,
            permit,
            acted,
        } = test_watchdog(&driver).await;

        arm_and_expire(&mut watchdog, &mut keeper, 2).await;
        poll_once(&mut watchdog);
        assert!(watchdog.state.timeout_pending);

        watchdog.reset();
        assert!(!watchdog.state.timeout_pending);

        permit.send(());
        drive_until_acted(&mut watchdog, &acted).await;
        assert!(watchdog.platform.is_some());

        acted.store(false, Ordering::SeqCst);
        arm_and_expire(&mut watchdog, &mut keeper, 4).await;
        poll_once(&mut watchdog);
        assert!(watchdog.state.timeout_pending);

        permit.send(());
        drive_until_acted(&mut watchdog, &acted).await;
        assert!(!watchdog.state.timeout_pending);
    }
}
