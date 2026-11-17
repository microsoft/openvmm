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
        // Devices are stopped before reset, and stop finishes the action.
        assert!(
            self.timeout_action.is_none(),
            "reset with a timeout action in flight"
        );
        self.state = WatchdogServicesState::new();
        // The reset state is unconfigured, so a deadline the guest armed
        // before the reset must not fire after it.
        self.stop_timer();
    }

    /// Runs an in-flight timeout action to completion.
    ///
    /// Devices call this when they stop, so neither a saved state nor a reset
    /// sees a timeout whose action has not finished.
    ///
    /// Cancelling this future leaves the action available to poll or finish later.
    pub async fn finish_timeout_action(&mut self) {
        if let Some(action) = &mut self.timeout_action {
            self.platform = Some(action.await);
            self.timeout_action = None;
        }
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
            }

            let Poll::Ready(_now) = self.vmtime.poll_timeout(cx) else {
                return;
            };
            tracing::error!(name = self.debug_id, "Encountered a watchdog timeout");
            self.state.config.set_configured(false);
            self.state.config.set_enabled(false);

            // Poll the action instead of blocking on it: the platform persists
            // the timeout to its store first, and the store can be served by
            // the thread polling this device.
            // Only an in-flight action holds the platform, and that case
            // returned or handed it back at the top of the loop.
            let mut platform = self
                .platform
                .take()
                .expect("platform must be present without an in-flight timeout action");
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

#[cfg(test)]
mod tests {
    use super::Register;
    use super::WatchdogServices;
    use crate::platform::WatchdogCallback;
    use crate::platform::WatchdogPlatform;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Wake;
    use std::task::Waker;
    use test_with_tracing::test;
    use vmcore::vmtime::SavedState as VmTimeSavedState;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    /// A platform whose timeout action waits for a permit, standing in for a
    /// store served by the thread that polls the device.
    struct GatedPlatform {
        permit: mesh::Receiver<()>,
        started: Arc<AtomicUsize>,
        completed: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl WatchdogPlatform for GatedPlatform {
        async fn on_timeout(&mut self) {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.permit.recv().await.unwrap();
            self.completed.fetch_add(1, Ordering::SeqCst);
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
        started: Arc<AtomicUsize>,
        completed: Arc<AtomicUsize>,
    }

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn test_watchdog(driver: &DefaultDriver) -> TestWatchdog {
        let keeper = VmTimeKeeper::new(driver, VmTime::from_100ns(0));
        let vmtime = keeper.builder().build(driver).await.unwrap();
        let (permit, permit_recv) = mesh::channel();
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let platform = GatedPlatform {
            permit: permit_recv,
            started: started.clone(),
            completed: completed.clone(),
        };
        let watchdog =
            WatchdogServices::new("test", vmtime.access("watchdog"), Box::new(platform), false)
                .await;
        TestWatchdog {
            watchdog,
            keeper,
            permit,
            started,
            completed,
        }
    }

    /// Polls once. With a blocking timeout action this call never returns.
    fn poll_once(watchdog: &mut WatchdogServices) {
        let waker = Waker::noop();
        watchdog.poll(&mut Context::from_waker(waker));
    }

    /// Arms a one-second count, then moves the stopped clock to `secs`, so
    /// the timer is due on the next poll without waiting for real time.
    async fn arm_and_expire(watchdog: &mut WatchdogServices, keeper: &mut VmTimeKeeper, secs: u64) {
        arm(watchdog);
        advance_to(keeper, secs).await;
    }

    /// Arms a one-second count.
    fn arm(watchdog: &mut WatchdogServices) {
        watchdog.write(Register::Count, 1).unwrap();
        watchdog.write(Register::Config, 0b11).unwrap();
    }

    /// Moves the stopped clock to `secs`.
    async fn advance_to(keeper: &mut VmTimeKeeper, secs: u64) {
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
            started,
            completed,
        } = test_watchdog(&driver).await;

        arm_and_expire(&mut watchdog, &mut keeper, 2).await;

        let wakes = Arc::new(WakeCounter::default());
        let waker = Waker::from(wakes.clone());
        watchdog.poll(&mut Context::from_waker(&waker));
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert_eq!(watchdog.read(Register::Config).unwrap() & 0b11, 0);

        let before = wakes.0.load(Ordering::SeqCst);
        permit.send(());
        assert!(wakes.0.load(Ordering::SeqCst) > before);
        poll_once(&mut watchdog);
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        poll_once(&mut watchdog);
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }

    /// Stopping the device finishes an in-flight action, so no saved state is
    /// taken with a timeout still owed.
    #[async_test]
    async fn finish_runs_the_in_flight_action(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            mut keeper,
            permit,
            completed,
            ..
        } = test_watchdog(&driver).await;

        arm_and_expire(&mut watchdog, &mut keeper, 2).await;
        poll_once(&mut watchdog);
        {
            let wakes = Arc::new(WakeCounter::default());
            let waker = Waker::from(wakes.clone());
            let mut finish = pin!(watchdog.finish_timeout_action());
            assert!(
                finish
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            assert_eq!(completed.load(Ordering::SeqCst), 0);
            let before = wakes.0.load(Ordering::SeqCst);
            permit.send(());
            assert!(wakes.0.load(Ordering::SeqCst) > before);
            finish.await;
        }
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        let mut finish = pin!(watchdog.finish_timeout_action());
        assert!(
            finish
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }

    #[async_test]
    async fn cancelled_finish_can_be_retried(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            mut keeper,
            permit,
            started,
            completed,
        } = test_watchdog(&driver).await;

        arm_and_expire(&mut watchdog, &mut keeper, 2).await;
        poll_once(&mut watchdog);
        {
            let mut finish = pin!(watchdog.finish_timeout_action());
            assert!(
                finish
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        {
            let mut finish = pin!(watchdog.finish_timeout_action());
            assert!(
                finish
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            permit.send(());
            finish.await;
        }
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 1);

        arm_and_expire(&mut watchdog, &mut keeper, 4).await;
        poll_once(&mut watchdog);
        permit.send(());
        watchdog.finish_timeout_action().await;
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(completed.load(Ordering::SeqCst), 2);
    }

    #[async_test]
    async fn finish_does_not_dispatch_an_expired_timer(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            mut keeper,
            permit,
            started,
            completed,
        } = test_watchdog(&driver).await;

        arm_and_expire(&mut watchdog, &mut keeper, 2).await;
        {
            let mut finish = pin!(watchdog.finish_timeout_action());
            assert!(
                finish
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_ready()
            );
        }
        assert_eq!(started.load(Ordering::SeqCst), 0);
        assert_eq!(watchdog.read(Register::Config).unwrap() & 0b11, 0b11);

        poll_once(&mut watchdog);
        arm_and_expire(&mut watchdog, &mut keeper, 4).await;
        permit.send(());
        watchdog.finish_timeout_action().await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(watchdog.read(Register::Config).unwrap() & 0b11, 0b11);

        poll_once(&mut watchdog);
        assert_eq!(started.load(Ordering::SeqCst), 2);
        permit.send(());
        watchdog.finish_timeout_action().await;
        assert_eq!(completed.load(Ordering::SeqCst), 2);
    }

    /// A reset leaves the watchdog unconfigured, so a timer the guest armed
    /// before the reset must not fire after it.
    #[async_test]
    async fn reset_cancels_the_armed_timer(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            mut keeper,
            permit,
            started,
            completed,
        } = test_watchdog(&driver).await;

        arm(&mut watchdog);
        watchdog.reset();
        advance_to(&mut keeper, 2).await;

        permit.send(());
        poll_once(&mut watchdog);
        assert_eq!(watchdog.read(Register::Config).unwrap() & 0b11, 0);
        assert_eq!(started.load(Ordering::SeqCst), 0);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
    }

    /// Reset relies on stop having finished the action.
    #[async_test]
    #[should_panic(expected = "reset with a timeout action in flight")]
    async fn reset_with_an_in_flight_action_panics(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            mut keeper,
            permit: _permit,
            completed: _completed,
            ..
        } = test_watchdog(&driver).await;

        arm_and_expire(&mut watchdog, &mut keeper, 2).await;
        poll_once(&mut watchdog);
        watchdog.reset();
    }

    #[async_test]
    async fn rearm_and_disable_while_action_is_pending(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            mut keeper,
            permit,
            started,
            completed,
        } = test_watchdog(&driver).await;

        arm_and_expire(&mut watchdog, &mut keeper, 2).await;
        poll_once(&mut watchdog);
        assert_eq!(started.load(Ordering::SeqCst), 1);

        arm_and_expire(&mut watchdog, &mut keeper, 4).await;
        poll_once(&mut watchdog);
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert_eq!(watchdog.read(Register::Config).unwrap() & 0b11, 0b11);

        permit.send(());
        poll_once(&mut watchdog);
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(watchdog.read(Register::Config).unwrap() & 0b11, 0);

        watchdog.write(Register::Config, 0).unwrap();
        poll_once(&mut watchdog);
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(completed.load(Ordering::SeqCst), 1);

        permit.send(());
        poll_once(&mut watchdog);
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        poll_once(&mut watchdog);
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(completed.load(Ordering::SeqCst), 2);
    }

    #[async_test]
    async fn ready_timeout_action_completes_in_one_poll(driver: DefaultDriver) {
        let TestWatchdog {
            mut watchdog,
            mut keeper,
            permit,
            completed,
            ..
        } = test_watchdog(&driver).await;

        permit.send(());
        arm_and_expire(&mut watchdog, &mut keeper, 2).await;
        poll_once(&mut watchdog);
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(watchdog.read(Register::Config).unwrap() & 0b11, 0);
    }

    #[async_test]
    async fn base_platform_persists_before_callbacks(driver: DefaultDriver) {
        use crate::platform::BaseWatchdogPlatform;
        use vmcore::non_volatile_store::NonVolatileStore;
        use vmcore::non_volatile_store::NonVolatileStoreError;

        struct GatedStore {
            permit: mesh::Receiver<()>,
            fail: bool,
        }

        #[async_trait::async_trait]
        impl NonVolatileStore for GatedStore {
            async fn persist(&mut self, data: Vec<u8>) -> Result<(), NonVolatileStoreError> {
                assert_eq!(data, [1]);
                self.permit.recv().await.unwrap();
                if self.fail {
                    return Err(NonVolatileStoreError::new(std::io::Error::other(
                        "test store failure",
                    )));
                }
                Ok(())
            }

            async fn restore(&mut self) -> Result<Option<Vec<u8>>, NonVolatileStoreError> {
                Ok(None)
            }
        }

        for fail in [false, true] {
            let mut keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
            let vmtime = keeper.builder().build(&driver).await.unwrap();
            let (permit, permit_recv) = mesh::channel();
            let mut platform = BaseWatchdogPlatform::new(Box::new(GatedStore {
                permit: permit_recv,
                fail,
            }))
            .await
            .unwrap();
            let completed = Arc::new(AtomicUsize::new(0));
            let callback_count = completed.clone();
            platform.add_callback(Box::new(move || {
                callback_count.fetch_add(1, Ordering::SeqCst);
            }));
            let mut watchdog =
                WatchdogServices::new("test", vmtime.access("watchdog"), Box::new(platform), false)
                    .await;

            arm_and_expire(&mut watchdog, &mut keeper, 2).await;
            poll_once(&mut watchdog);
            assert_eq!(completed.load(Ordering::SeqCst), 0);
            assert_eq!(watchdog.read(Register::Config).unwrap() & 0b11, 0);
            permit.send(());
            poll_once(&mut watchdog);
            assert_eq!(completed.load(Ordering::SeqCst), 1);
            poll_once(&mut watchdog);
            assert_eq!(completed.load(Ordering::SeqCst), 1);
        }
    }
}
