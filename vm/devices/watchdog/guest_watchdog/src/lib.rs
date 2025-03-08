// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device::ChipsetDevice;
use inspect::Inspect;
use inspect::InspectMut;
use std::task::Context;
use vmcore::device_state::ChangeDeviceState;
use vmcore::vmtime::VmTimeAccess;
use watchdog_core::platform::WatchdogPlatform;
use watchdog_core::WatchdogServices;

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
        register_pio: &mut dyn RegisterPortIoIntercept,
        pio_wdat_port: u16,
        is_restoring: bool,
    ) -> GuestWatchdogServices {
        let mut pio_static_wdat_port = register_pio.new_io_region("wdat_port", 8);

        pio_static_wdat_port.map(pio_wdat_port);

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

    async fn stop(&mut self) {}

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
