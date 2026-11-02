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
        use crate::UefiCommand;
        use crate::UefiDevice;
        use crate::UefiRuntimeDeps;
        use chipset_device::mmio::MmioIntercept;
        use chipset_device::pio::PortIoIntercept;
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
        use watchdog_core::platform::WatchdogCallback;
        use watchdog_core::platform::WatchdogPlatform;

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

        async fn new_device(
            driver: &DefaultDriver,
            platform: Box<dyn WatchdogPlatform>,
            use_mmio: bool,
            is_restoring: bool,
        ) -> (UefiDevice, VmTimeKeeper) {
            let keeper = VmTimeKeeper::new(driver, VmTime::from_100ns(0));
            let vmtime = keeper.builder().build(driver).await.unwrap();
            let gm = GuestMemory::allocate(64 * 1024);
            let (_genid_send, generation_id_recv) = mesh::channel();
            let (_watchdog_send, watchdog_recv) = mesh::channel();
            let device = UefiDevice::new(
                UefiRuntimeDeps {
                    gm: gm.clone(),
                    nvram_storage: Box::new(InMemoryNvram::new()),
                    logger: Box::new(TestLogger),
                    vmtime: &vmtime,
                    watchdog_platform: platform,
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
                    use_mmio,
                    command_set: if use_mmio {
                        UefiCommandSet::Aarch64
                    } else {
                        UefiCommandSet::X64
                    },
                    diagnostics_log_level: LogLevel::make_default(),
                    diagnostics_rate_limit: None,
                },
                is_restoring,
            )
            .await
            .unwrap();
            (device, keeper)
        }

        fn write_register(device: &mut UefiDevice, use_mmio: bool, reg: UefiCommand, value: u32) {
            if use_mmio {
                let base = *MmioIntercept::get_static_regions(device)[0].1.start();
                device.mmio_write(base, &reg.0.to_ne_bytes()).unwrap();
                device.mmio_write(base + 4, &value.to_ne_bytes()).unwrap();
            } else {
                let base = *PortIoIntercept::get_static_regions(device)[0].1.start();
                device.io_write(base, &reg.0.to_ne_bytes()).unwrap();
                device.io_write(base + 4, &value.to_ne_bytes()).unwrap();
            }
        }

        fn read_register(device: &mut UefiDevice, use_mmio: bool, reg: UefiCommand) -> u32 {
            let mut data = [0; 4];
            if use_mmio {
                let base = *MmioIntercept::get_static_regions(device)[0].1.start();
                device.mmio_write(base, &reg.0.to_ne_bytes()).unwrap();
                device.mmio_read(base + 4, &mut data).unwrap();
            } else {
                let base = *PortIoIntercept::get_static_regions(device)[0].1.start();
                device.io_write(base, &reg.0.to_ne_bytes()).unwrap();
                device.io_read(base + 4, &mut data).unwrap();
            }
            u32::from_ne_bytes(data)
        }

        async fn advance_to(keeper: &mut VmTimeKeeper, secs: u64) {
            keeper
                .restore(VmTimeSavedState::from_vmtime(VmTime::from_100ns(
                    secs * 10_000_000,
                )))
                .await;
        }

        async fn check_stop_restore_and_rearm(driver: DefaultDriver, use_mmio: bool) {
            let (permit, permit_recv) = mesh::channel();
            let completed = Arc::new(AtomicUsize::new(0));
            let (mut device, mut keeper) = new_device(
                &driver,
                Box::new(GatedPlatform {
                    permit: permit_recv,
                    completed: completed.clone(),
                }),
                use_mmio,
                false,
            )
            .await;
            device.start();
            write_register(&mut device, use_mmio, UefiCommand::WATCHDOG_COUNT, 3);
            write_register(&mut device, use_mmio, UefiCommand::WATCHDOG_CONFIG, 0b11);
            advance_to(&mut keeper, 4).await;
            device.poll_device(&mut Context::from_waker(Waker::noop()));
            assert_eq!(
                read_register(&mut device, use_mmio, UefiCommand::WATCHDOG_CONFIG) & 0b11,
                0
            );
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
            assert_eq!(
                read_register(&mut device, use_mmio, UefiCommand::WATCHDOG_CONFIG) & 0b11,
                0
            );
            drop(device);
            drop(keeper);

            let (permit, permit_recv) = mesh::channel();
            let (mut restored, mut keeper) = new_device(
                &driver,
                Box::new(GatedPlatform {
                    permit: permit_recv,
                    completed: completed.clone(),
                }),
                use_mmio,
                true,
            )
            .await;
            restored.restore(saved).unwrap();
            restored.start();
            advance_to(&mut keeper, 20).await;
            restored.poll_device(&mut Context::from_waker(Waker::noop()));
            assert_eq!(completed.load(Ordering::SeqCst), 1);
            assert_eq!(
                read_register(&mut restored, use_mmio, UefiCommand::WATCHDOG_CONFIG) & 0b11,
                0
            );
            assert_eq!(
                read_register(&mut restored, use_mmio, UefiCommand::WATCHDOG_COUNT),
                3
            );

            permit.send(());
            write_register(&mut restored, use_mmio, UefiCommand::WATCHDOG_CONFIG, 0b11);
            assert_eq!(
                read_register(&mut restored, use_mmio, UefiCommand::WATCHDOG_COUNT),
                3
            );
            advance_to(&mut keeper, 22).await;
            restored.poll_device(&mut Context::from_waker(Waker::noop()));
            assert_eq!(completed.load(Ordering::SeqCst), 1);
            assert_eq!(
                read_register(&mut restored, use_mmio, UefiCommand::WATCHDOG_CONFIG) & 0b11,
                0b11
            );
            advance_to(&mut keeper, 24).await;
            restored.poll_device(&mut Context::from_waker(Waker::noop()));
            assert_eq!(completed.load(Ordering::SeqCst), 2);
            assert_eq!(
                read_register(&mut restored, use_mmio, UefiCommand::WATCHDOG_CONFIG) & 0b11,
                0
            );
            advance_to(&mut keeper, 40).await;
            restored.poll_device(&mut Context::from_waker(Waker::noop()));
            assert_eq!(completed.load(Ordering::SeqCst), 2);
            restored.stop().await;
        }

        #[async_test]
        async fn port_io_stop_restore_and_rearm(driver: DefaultDriver) {
            check_stop_restore_and_rearm(driver, false).await;
        }

        #[async_test]
        async fn mmio_stop_restore_and_rearm(driver: DefaultDriver) {
            check_stop_restore_and_rearm(driver, true).await;
        }
    }
}
