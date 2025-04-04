// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PIIX4 - CMOS RTC
//!
//! Extends basic x86 CMOS RTC with a few additional ports + more RAM.

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use inspect::Inspect;
use inspect::InspectMut;
use local_clock::InspectableLocalClock;
use std::ops::RangeInclusive;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::vmtime::VmTimeSource;

open_enum::open_enum! {
    enum Piix4CmosRtcIoPort: u16 {
        ADDRESS          = 0x70,
        DATA             = 0x71,
        EXTENDED_ADDRESS = 0x72,
        EXTENDED_DATA    = 0x73,
        ADDRESS_SHADOW_2 = 0x74,
        DATA_SHADOW_2    = 0x75,
        ADDRESS_SHADOW_3 = 0x76,
        DATA_SHADOW_3    = 0x77,
    }
}

#[derive(Debug, Inspect)]
struct Piix4CmosRtcState {
    ext_addr: u8,
}

#[derive(InspectMut)]
pub struct Piix4CmosRtc {
    // Sub-emulators
    #[inspect(mut)]
    inner: chipset::cmos_rtc::Rtc,

    // Volatile state
    state: Piix4CmosRtcState,
}

impl Piix4CmosRtc {
    pub fn new(
        real_time_source: Box<dyn InspectableLocalClock>,
        interrupt: LineInterrupt,
        vmtime_source: &VmTimeSource,
        initial_cmos: Option<[u8; 256]>,
        enlightened_interrupts: bool,
    ) -> Piix4CmosRtc {
        Piix4CmosRtc {
            state: Piix4CmosRtcState { ext_addr: 0 },
            inner: chipset::cmos_rtc::Rtc::new(
                real_time_source,
                interrupt,
                vmtime_source,
                0x32,
                initial_cmos,
                enlightened_interrupts,
            ),
        }
    }
}

impl ChangeDeviceState for Piix4CmosRtc {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.inner.reset().await;
    }
}

impl ChipsetDevice for Piix4CmosRtc {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PortIoIntercept for Piix4CmosRtc {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        // We assume all accesses are one byte in size. Attempts to
        // access larger sizes will return a single byte of information
        // (zero-extended to the size of the access).
        data[0] = match Piix4CmosRtcIoPort(io_port) {
            Piix4CmosRtcIoPort::ADDRESS | Piix4CmosRtcIoPort::DATA => {
                return self.inner.io_read(io_port, data);
            }
            Piix4CmosRtcIoPort::ADDRESS_SHADOW_2 | Piix4CmosRtcIoPort::DATA_SHADOW_2 => {
                return self.inner.io_read(io_port - 4, data);
            }
            Piix4CmosRtcIoPort::EXTENDED_ADDRESS | Piix4CmosRtcIoPort::ADDRESS_SHADOW_3 => {
                self.state.ext_addr
            }
            Piix4CmosRtcIoPort::EXTENDED_DATA | Piix4CmosRtcIoPort::DATA_SHADOW_3 => {
                self.inner.get_cmos_byte(self.state.ext_addr + 128)
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        };

        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        // We assume all accesses are one byte in size. Attempts to
        // access larger sizes will return a single byte of information
        // (zero-extended to the size of the access).
        match Piix4CmosRtcIoPort(io_port) {
            Piix4CmosRtcIoPort::ADDRESS | Piix4CmosRtcIoPort::DATA => {
                return self.inner.io_write(io_port, data);
            }
            Piix4CmosRtcIoPort::ADDRESS_SHADOW_2 | Piix4CmosRtcIoPort::DATA_SHADOW_2 => {
                return self.inner.io_write(io_port - 4, data);
            }
            Piix4CmosRtcIoPort::EXTENDED_ADDRESS | Piix4CmosRtcIoPort::ADDRESS_SHADOW_3 => {
                self.state.ext_addr = data[0] & 0x7F;
            }
            Piix4CmosRtcIoPort::EXTENDED_DATA | Piix4CmosRtcIoPort::DATA_SHADOW_3 => {
                self.inner.set_cmos_byte(self.state.ext_addr + 128, data[0]);
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &[
            (
                "io",
                (Piix4CmosRtcIoPort::ADDRESS.0)..=(Piix4CmosRtcIoPort::DATA.0),
            ),
            (
                "io-ext",
                (Piix4CmosRtcIoPort::EXTENDED_ADDRESS.0)..=(Piix4CmosRtcIoPort::DATA_SHADOW_3.0),
            ),
        ]
    }
}

impl PollDevice for Piix4CmosRtc {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.inner.poll_device(cx)
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
        #[mesh(package = "chipset.piix4.cmos_rtc")]
        pub struct SavedState {
            #[mesh(1)]
            pub ext_addr: u8,
            #[mesh(2)]
            pub inner: <chipset::cmos_rtc::Rtc as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for Piix4CmosRtc {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Piix4CmosRtcState { ext_addr } = self.state;

            let saved_state = state::SavedState {
                ext_addr,
                inner: self.inner.save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { ext_addr, inner } = state;
            self.state = Piix4CmosRtcState { ext_addr };
            self.inner.restore(inner)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_clock::MockLocalClock;

    fn new_test_rtc() -> (
        pal_async::DefaultPool,
        vmcore::vmtime::VmTimeKeeper,
        Piix4CmosRtc,
    ) {
        let mut pool = pal_async::DefaultPool::new();
        let driver = pool.driver();
        let vm_time_keeper =
            vmcore::vmtime::VmTimeKeeper::new(&driver, vmcore::vmtime::VmTime::from_100ns(0));
        let vm_time_source = pool
            .run_until(vm_time_keeper.builder().build(&driver))
            .unwrap();

        let rtc = Piix4CmosRtc::new(
            Box::new(MockLocalClock::new()),
            LineInterrupt::detached(),
            &vm_time_source,
            None,
            false,
        );

        (pool, vm_time_keeper, rtc)
    }

    fn get_cmos_data(rtc: &mut Piix4CmosRtc, addr: u8) -> u8 {
        let mut temp = [addr];
        rtc.io_write(Piix4CmosRtcIoPort::ADDRESS.0, &temp).unwrap();
        rtc.io_read(Piix4CmosRtcIoPort::DATA.0, &mut temp).unwrap();
        temp[0]
    }

    fn set_cmos_data(rtc: &mut Piix4CmosRtc, addr: u8, data: u8) {
        let mut temp = [addr];
        rtc.io_write(Piix4CmosRtcIoPort::ADDRESS.0, &temp).unwrap();
        temp[0] = data;
        rtc.io_write(Piix4CmosRtcIoPort::DATA.0, &temp).unwrap();
    }

    fn get_ext_cmos_data(rtc: &mut Piix4CmosRtc, addr: u8) -> u8 {
        let mut temp = [addr];
        rtc.io_write(Piix4CmosRtcIoPort::EXTENDED_ADDRESS.0, &temp)
            .unwrap();
        rtc.io_read(Piix4CmosRtcIoPort::EXTENDED_DATA.0, &mut temp)
            .unwrap();
        temp[0]
    }

    fn set_ext_cmos_data(rtc: &mut Piix4CmosRtc, addr: u8, data: u8) {
        let mut temp = [addr];
        rtc.io_write(Piix4CmosRtcIoPort::EXTENDED_ADDRESS.0, &temp)
            .unwrap();
        temp[0] = data;
        rtc.io_write(Piix4CmosRtcIoPort::EXTENDED_DATA.0, &temp)
            .unwrap();
    }

    fn get_cmos_data_shadow(rtc: &mut Piix4CmosRtc, addr: u8) -> u8 {
        let mut temp = [addr];
        rtc.io_write(Piix4CmosRtcIoPort::ADDRESS_SHADOW_2.0, &temp)
            .unwrap();
        rtc.io_read(Piix4CmosRtcIoPort::DATA_SHADOW_2.0, &mut temp)
            .unwrap();
        temp[0]
    }

    fn set_cmos_data_shadow(rtc: &mut Piix4CmosRtc, addr: u8, data: u8) {
        let mut temp = [addr];
        rtc.io_write(Piix4CmosRtcIoPort::ADDRESS_SHADOW_2.0, &temp)
            .unwrap();
        temp[0] = data;
        rtc.io_write(Piix4CmosRtcIoPort::DATA_SHADOW_2.0, &temp)
            .unwrap();
    }

    fn get_ext_cmos_data_shadow(rtc: &mut Piix4CmosRtc, addr: u8) -> u8 {
        let mut temp = [addr];
        rtc.io_write(Piix4CmosRtcIoPort::ADDRESS_SHADOW_3.0, &temp)
            .unwrap();
        rtc.io_read(Piix4CmosRtcIoPort::DATA_SHADOW_3.0, &mut temp)
            .unwrap();
        temp[0]
    }

    fn set_ext_cmos_data_shadow(rtc: &mut Piix4CmosRtc, addr: u8, data: u8) {
        let mut temp = [addr];
        rtc.io_write(Piix4CmosRtcIoPort::ADDRESS_SHADOW_3.0, &temp)
            .unwrap();
        temp[0] = data;
        rtc.io_write(Piix4CmosRtcIoPort::DATA_SHADOW_3.0, &temp)
            .unwrap();
    }

    #[test]
    fn test_writeable() {
        let (_, _, mut rtc) = new_test_rtc();

        //Rigisters 0x0f..0x7f should be writable, skip 0x32 which is century field of RTC
        for i in 0x0F..=0x7F {
            if i == 0x32 {
                continue;
            }

            set_cmos_data(&mut rtc, i, 0xFF);
            assert_eq!(get_cmos_data(&mut rtc, i), 0xFF);
            assert_eq!(get_cmos_data_shadow(&mut rtc, i), 0xFF);
            set_cmos_data(&mut rtc, i, 0);
            assert_eq!(get_cmos_data(&mut rtc, i), 0);
            assert_eq!(get_cmos_data_shadow(&mut rtc, i), 0);
            set_cmos_data_shadow(&mut rtc, i, 0xFF);
            assert_eq!(get_cmos_data(&mut rtc, i), 0xFF);
            assert_eq!(get_cmos_data_shadow(&mut rtc, i), 0xFF);
            set_cmos_data_shadow(&mut rtc, i, 0);
            assert_eq!(get_cmos_data(&mut rtc, i), 0);
            assert_eq!(get_cmos_data_shadow(&mut rtc, i), 0);
        }
    }

    #[test]
    fn test_writeable_ext() {
        let (_, _, mut rtc) = new_test_rtc();

        //Rigisters 0x80..0xff should be writable through extended gate 0x72/0x73
        for i in 0..=0x7F {
            set_ext_cmos_data(&mut rtc, i, 0xFF);
            assert_eq!(get_ext_cmos_data(&mut rtc, i), 0xFF);
            assert_eq!(get_ext_cmos_data_shadow(&mut rtc, i), 0xFF);
            set_ext_cmos_data(&mut rtc, i, 0);
            assert_eq!(get_ext_cmos_data(&mut rtc, i), 0);
            assert_eq!(get_ext_cmos_data_shadow(&mut rtc, i), 0);
            set_ext_cmos_data_shadow(&mut rtc, i, 0xFF);
            assert_eq!(get_ext_cmos_data(&mut rtc, i), 0xFF);
            assert_eq!(get_ext_cmos_data_shadow(&mut rtc, i), 0xFF);
            set_ext_cmos_data_shadow(&mut rtc, i, 0);
            assert_eq!(get_ext_cmos_data(&mut rtc, i), 0);
            assert_eq!(get_ext_cmos_data_shadow(&mut rtc, i), 0);
        }
    }
}
