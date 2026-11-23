// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use inspect::Inspect;
use inspect::InspectMut;
use std::task::Context;
use vmcore::device_state::ChangeDeviceState;
use vmcore::vmtime::VmTimeAccess;
use watchdog_core::WatchdogServices;
use watchdog_core::platform::WatchdogPlatform;

open_enum::open_enum! {
    enum WatchdogPort: u16 {
        ADDRESS          = 0x0,
        DATA             = 0x4,
    }
}

pub struct GuestwatchdogRt {
    pio_static_wdat_port: Box<dyn ControlPortIoIntercept>,
}

#[derive(Debug, Inspect)]
pub struct GuestWatchdogState {
    watchdog_reg: u32,
}

#[derive(InspectMut)]
pub struct GuestWatchdogServices {
    // Runtime glue
    #[inspect(skip)]
    rt: GuestwatchdogRt,

    // Sub-emulators
    watchdog: WatchdogServices,

    // Volatile state
    state: GuestWatchdogState,
}

impl GuestWatchdogServices {
    pub async fn new(
        vmtime: VmTimeAccess,
        watchdog_platform: Box<dyn WatchdogPlatform>,
        pio_static_wdat_port: Box<dyn ControlPortIoIntercept>,
        is_restoring: bool,
    ) -> GuestWatchdogServices {
        GuestWatchdogServices {
            watchdog: WatchdogServices::new(
                "guest-watchdog",
                vmtime,
                watchdog_platform,
                is_restoring,
            )
            .await,
            state: GuestWatchdogState { watchdog_reg: 0 },
            rt: GuestwatchdogRt {
                pio_static_wdat_port,
            },
        }
    }

    fn read_data(&mut self, addr: u32) -> u32 {
        match WatchdogRegister(addr) {
            WatchdogRegister::WATCHDOG_RESOLUTION
            | WatchdogRegister::WATCHDOG_CONFIG
            | WatchdogRegister::WATCHDOG_COUNT => {
                let reg = bios_cmd_to_watchdog_register(WatchdogRegister(addr)).unwrap();
                match self.watchdog.read(reg) {
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
            _ => {
                tracelimit::warn_ratelimited!(?addr, "unknown bios read");
                !0
            }
        }
    }

    fn write_data(&mut self, addr: u32, data: u32) {
        match WatchdogRegister(addr) {
            WatchdogRegister::WATCHDOG_RESOLUTION
            | WatchdogRegister::WATCHDOG_CONFIG
            | WatchdogRegister::WATCHDOG_COUNT => {
                let reg = bios_cmd_to_watchdog_register(WatchdogRegister(addr)).unwrap();
                match self.watchdog.write(reg, data) {
                    Ok(()) => (),
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            "Error while writing to watchdog device"
                        );
                    }
                }
            }
            _ => tracelimit::warn_ratelimited!(addr, data, "unknown bios write"),
        }
    }
}

fn bios_cmd_to_watchdog_register(cmd: WatchdogRegister) -> Option<watchdog_core::Register> {
    let res = match cmd {
        WatchdogRegister::WATCHDOG_RESOLUTION => watchdog_core::Register::Resolution,
        WatchdogRegister::WATCHDOG_CONFIG => watchdog_core::Register::Config,
        WatchdogRegister::WATCHDOG_COUNT => watchdog_core::Register::Count,
        _ => return None,
    };
    Some(res)
}

impl PollDevice for GuestWatchdogServices {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        self.watchdog.poll(cx);
    }
}

impl ChangeDeviceState for GuestWatchdogServices {
    fn start(&mut self) {}

    async fn stop(&mut self) {
        self.watchdog.finish_timeout_action().await;
    }

    async fn reset(&mut self) {
        self.watchdog.reset();
        self.state.watchdog_reg = 0;
    }
}

impl ChipsetDevice for GuestWatchdogServices {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PortIoIntercept for GuestWatchdogServices {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        if let Some(offset) = self.rt.pio_static_wdat_port.offset_of(io_port) {
            let v = match WatchdogPort(offset) {
                WatchdogPort::ADDRESS => self.state.watchdog_reg,
                WatchdogPort::DATA => self.read_data(self.state.watchdog_reg),
                _ => return IoResult::Err(IoError::InvalidRegister),
            };

            data.copy_from_slice(&v.to_ne_bytes());
            return IoResult::Ok;
        }
        IoResult::Err(IoError::InvalidRegister)
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        if let Some(offset) = self.rt.pio_static_wdat_port.offset_of(io_port) {
            let v = u32::from_ne_bytes(data.try_into().unwrap());
            match WatchdogPort(offset) {
                WatchdogPort::ADDRESS => self.state.watchdog_reg = v,
                WatchdogPort::DATA => self.write_data(self.state.watchdog_reg, v),
                _ => return IoResult::Err(IoError::InvalidRegister),
            };
            return IoResult::Ok;
        }
        IoResult::Err(IoError::InvalidRegister)
    }
}

open_enum::open_enum! {
    pub enum WatchdogRegister: u32 {
        WATCHDOG_CONFIG              = 0x27,
        WATCHDOG_RESOLUTION          = 0x28,
        WATCHDOG_COUNT               = 0x29,
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
        #[mesh(package = "chipset.watchdog.guest")]
        pub struct SavedState {
            #[mesh(1)]
            pub watchdog_reg: u32,
            #[mesh(2)]
            pub inner: <watchdog_core::WatchdogServices as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for GuestWatchdogServices {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let GuestWatchdogState { watchdog_reg } = self.state;
            let saved_state = state::SavedState {
                watchdog_reg,
                inner: self.watchdog.save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                watchdog_reg,
                inner,
            } = state;
            self.state = GuestWatchdogState { watchdog_reg };
            self.watchdog.restore(inner)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GuestWatchdogServices;
    use super::WatchdogRegister;
    use chipset_device::pio::ControlPortIoIntercept;
    use chipset_device::pio::PortIoIntercept;
    use chipset_device::poll_device::PollDevice;
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
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::SaveRestore;
    use vmcore::vmtime::SavedState as VmTimeSavedState;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;
    use watchdog_core::platform::WatchdogCallback;
    use watchdog_core::platform::WatchdogPlatform;

    /// A platform whose timeout action waits for a permit.
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

    struct TestPorts(Option<u16>);

    impl ControlPortIoIntercept for TestPorts {
        fn region_name(&self) -> &str {
            "watchdog-test"
        }

        fn map(&mut self, addr: u16) {
            self.0 = Some(addr);
        }

        fn unmap(&mut self) {
            self.0 = None;
        }

        fn addr(&self) -> Option<u16> {
            self.0
        }

        fn len(&self) -> u16 {
            8
        }

        fn offset_of(&self, addr: u16) -> Option<u16> {
            let offset = addr.checked_sub(self.0?)?;
            (offset < self.len()).then_some(offset)
        }
    }

    async fn new_device(
        driver: &DefaultDriver,
        platform: Box<dyn WatchdogPlatform>,
        is_restoring: bool,
    ) -> (GuestWatchdogServices, VmTimeKeeper) {
        let keeper = VmTimeKeeper::new(driver, VmTime::from_100ns(0));
        let vmtime = keeper.builder().build(driver).await.unwrap();
        let device = GuestWatchdogServices::new(
            vmtime.access("guest-watchdog"),
            platform,
            Box::new(TestPorts(Some(0x1000))),
            is_restoring,
        )
        .await;
        (device, keeper)
    }

    fn write_register(device: &mut GuestWatchdogServices, reg: WatchdogRegister, value: u32) {
        device.io_write(0x1000, &reg.0.to_ne_bytes()).unwrap();
        device.io_write(0x1004, &value.to_ne_bytes()).unwrap();
    }

    fn read_register(device: &mut GuestWatchdogServices, reg: WatchdogRegister) -> u32 {
        device.io_write(0x1000, &reg.0.to_ne_bytes()).unwrap();
        let mut data = [0; 4];
        device.io_read(0x1004, &mut data).unwrap();
        u32::from_ne_bytes(data)
    }

    async fn advance_to(keeper: &mut VmTimeKeeper, secs: u64) {
        keeper
            .restore(VmTimeSavedState::from_vmtime(VmTime::from_100ns(
                secs * 10_000_000,
            )))
            .await;
    }

    #[async_test]
    async fn stopped_timeout_restores_without_replay_and_can_be_rearmed(driver: DefaultDriver) {
        let (permit, permit_recv) = mesh::channel();
        let completed = Arc::new(AtomicUsize::new(0));
        let (mut device, mut keeper) = new_device(
            &driver,
            Box::new(GatedPlatform {
                permit: permit_recv,
                completed: completed.clone(),
            }),
            false,
        )
        .await;
        device.start();
        write_register(&mut device, WatchdogRegister::WATCHDOG_COUNT, 3);
        write_register(&mut device, WatchdogRegister::WATCHDOG_CONFIG, 0b11);
        advance_to(&mut keeper, 4).await;
        device.poll_device(&mut Context::from_waker(Waker::noop()));
        assert_eq!(
            read_register(&mut device, WatchdogRegister::WATCHDOG_CONFIG) & 0b11,
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
            read_register(&mut device, WatchdogRegister::WATCHDOG_CONFIG) & 0b11,
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
            true,
        )
        .await;
        restored.restore(saved).unwrap();
        restored.start();
        advance_to(&mut keeper, 20).await;
        restored.poll_device(&mut Context::from_waker(Waker::noop()));
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(
            read_register(&mut restored, WatchdogRegister::WATCHDOG_CONFIG) & 0b11,
            0
        );
        assert_eq!(
            read_register(&mut restored, WatchdogRegister::WATCHDOG_COUNT),
            3
        );

        permit.send(());
        write_register(&mut restored, WatchdogRegister::WATCHDOG_CONFIG, 0b11);
        assert_eq!(
            read_register(&mut restored, WatchdogRegister::WATCHDOG_COUNT),
            3
        );
        advance_to(&mut keeper, 22).await;
        restored.poll_device(&mut Context::from_waker(Waker::noop()));
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(
            read_register(&mut restored, WatchdogRegister::WATCHDOG_CONFIG) & 0b11,
            0b11
        );
        advance_to(&mut keeper, 24).await;
        restored.poll_device(&mut Context::from_waker(Waker::noop()));
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        assert_eq!(
            read_register(&mut restored, WatchdogRegister::WATCHDOG_CONFIG) & 0b11,
            0
        );
        advance_to(&mut keeper, 40).await;
        restored.poll_device(&mut Context::from_waker(Waker::noop()));
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        restored.stop().await;
    }
}
