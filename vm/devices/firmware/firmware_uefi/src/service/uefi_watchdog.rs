// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::UefiDevice;
use inspect::Inspect;
use vmcore::vmtime::VmTimeAccess;
use watchdog_core::WatchdogServices;
use watchdog_core::platform::WatchdogPlatform;

#[derive(Inspect)]
pub struct UefiWatchdogServices {
    pub watchdog: WatchdogServices,
}

impl UefiWatchdogServices {
    pub async fn new(
        vmtime: VmTimeAccess,
        platform: Box<dyn WatchdogPlatform>,
        is_restoring: bool,
    ) -> UefiWatchdogServices {
        UefiWatchdogServices {
            watchdog: WatchdogServices::new("uefi-watchdog", vmtime, platform, is_restoring).await,
        }
    }
}

impl UefiDevice {
    pub(crate) fn handle_watchdog_read(&mut self, reg: watchdog_core::Register) -> u32 {
        match self.service.uefi_watchdog.watchdog.read(reg) {
            Ok(val) => val,
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "Error while reading from watchdog device"
                );
                !0
            }
        }
    }

    pub(crate) fn handle_watchdog_write(&mut self, reg: watchdog_core::Register, val: u32) {
        match self.service.uefi_watchdog.watchdog.write(reg, val) {
            Ok(()) => (),
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "Error while writing to watchdog device"
                );
            }
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
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "firmware.uefi.watchdog")]
        pub struct SavedState {
            #[mesh(1)]
            pub inner: <watchdog_core::WatchdogServices as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for UefiWatchdogServices {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let saved_state = state::SavedState {
                inner: self.watchdog.save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { inner } = state;
            self.watchdog.restore(inner)?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::UefiRuntimeDeps;
        use chipset_device::poll_device::PollDevice;
        use firmware_uefi_resources::LogLevel;
        use firmware_uefi_resources::UefiCommandSet;
        use firmware_uefi_resources::UefiConfig;
        use firmware_uefi_resources::platform::UefiEvent;
        use firmware_uefi_resources::platform::UefiLogger;
        use guestmem::GuestMemory;
        use pal_async::DefaultDriver;
        use pal_async::async_test;
        use std::future::Future;
        use std::pin::pin;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::task::Context;
        use std::task::Waker;
        use test_with_tracing::test;
        use uefi_nvram_storage::in_memory::InMemoryNvram;
        use vmcore::device_state::ChangeDeviceState;
        use vmcore::save_restore::SaveRestore;
        use vmcore::vmtime::SavedState as VmTimeSavedState;
        use vmcore::vmtime::VmTime;
        use vmcore::vmtime::VmTimeKeeper;
        use watchdog_core::Register;
        use watchdog_core::platform::WatchdogCallback;

        struct GatedPlatform {
            permit: mesh::Receiver<()>,
            completed: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl WatchdogPlatform for GatedPlatform {
            async fn on_timeout(&mut self) {
                self.permit.recv().await.unwrap();
                self.completed.fetch_add(1, Ordering::SeqCst);
            }

            async fn read_and_clear_boot_status(&mut self) -> bool {
                false
            }

            fn add_callback(&mut self, _callback: Box<dyn WatchdogCallback>) {}
        }

        struct TestLogger;

        impl UefiLogger for TestLogger {
            fn log_event(&self, _event: UefiEvent) {}
        }

        #[async_test]
        async fn stop_finishes_the_timeout_action(driver: DefaultDriver) {
            let mut keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
            let vmtime = keeper.builder().build(&driver).await.unwrap();
            let (permit, permit_recv) = mesh::channel();
            let completed = Arc::new(AtomicUsize::new(0));
            let gm = GuestMemory::allocate(64 * 1024);
            let (_genid_send, generation_id_recv) = mesh::channel();
            let (_watchdog_send, watchdog_recv) = mesh::channel();
            let mut device = UefiDevice::new(
                UefiRuntimeDeps {
                    gm: gm.clone(),
                    nvram_storage: Box::new(InMemoryNvram::new()),
                    logger: Box::new(TestLogger),
                    vmtime: &vmtime,
                    watchdog_platform: Box::new(GatedPlatform {
                        permit: permit_recv,
                        completed: completed.clone(),
                    }),
                    watchdog_recv,
                    generation_id_deps: generation_id::GenerationIdRuntimeDeps {
                        gm,
                        generation_id_recv,
                        notify_interrupt: vmcore::line_interrupt::LineInterrupt::detached(),
                    },
                    vsm_config: None,
                    time_source: Box::new(local_clock::MockLocalClock::new()),
                },
                UefiConfig {
                    base_template: None,
                    custom_uefi_json: None,
                    secure_boot: false,
                    initial_generation_id: [0; 16],
                    use_mmio: false,
                    command_set: UefiCommandSet::X64,
                    diagnostics_log_level: LogLevel::make_default(),
                    diagnostics_rate_limit: None,
                },
                false,
            )
            .await
            .unwrap();
            device.start();
            device.handle_watchdog_write(Register::Count, 1);
            device.handle_watchdog_write(Register::Config, 0b11);
            keeper
                .restore(VmTimeSavedState::from_vmtime(VmTime::from_100ns(
                    2 * 10_000_000,
                )))
                .await;
            device.poll_device(&mut Context::from_waker(Waker::noop()));
            assert_eq!(completed.load(Ordering::SeqCst), 0);
            {
                let mut stop = pin!(device.stop());
                assert!(
                    stop.as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                assert_eq!(completed.load(Ordering::SeqCst), 0);
                permit.send(());
                stop.await;
            }
            assert_eq!(completed.load(Ordering::SeqCst), 1);
            let saved = device.save().unwrap();
            device.reset().await;
            device.restore(saved).unwrap();
            device.start();
            device.poll_device(&mut Context::from_waker(Waker::noop()));
            assert_eq!(completed.load(Ordering::SeqCst), 1);
        }
    }
}
