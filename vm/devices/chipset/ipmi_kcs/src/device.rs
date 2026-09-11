// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Chipset adapters for the IPMI KCS PIO and MMIO interfaces.

use crate::IpmiKcs;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_resources::ipmi_kcs::IPMI_KCS_DATA_PORT;
use chipset_resources::ipmi_kcs::IPMI_KCS_MMIO_BASE_ADDRESS_AARCH64;
use chipset_resources::ipmi_kcs::IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64;
use chipset_resources::ipmi_kcs::IPMI_KCS_MMIO_REGION_SIZE_AARCH64;
use chipset_resources::ipmi_kcs::IPMI_KCS_MMIO_STATUS_COMMAND_ADDRESS_AARCH64;
use chipset_resources::ipmi_kcs::IPMI_KCS_STATUS_COMMAND_PORT;
use inspect::InspectMut;
use std::ops::RangeInclusive;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

const PIO_REGIONS: [(&str, RangeInclusive<u16>); 1] = [(
    "ipmi-kcs",
    IPMI_KCS_DATA_PORT..=IPMI_KCS_STATUS_COMMAND_PORT,
)];
const MMIO_REGIONS: [(&str, RangeInclusive<u64>); 1] = [(
    "ipmi-kcs",
    IPMI_KCS_MMIO_BASE_ADDRESS_AARCH64
        ..=IPMI_KCS_MMIO_BASE_ADDRESS_AARCH64 + IPMI_KCS_MMIO_REGION_SIZE_AARCH64 - 1,
)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Pio,
    Mmio,
}

/// Transport adapter for a virtual IPMI KCS interface.
pub struct IpmiKcsDevice {
    core: IpmiKcs,
    transport: Transport,
}

impl IpmiKcsDevice {
    /// Creates an AMD64 port-I/O device around the transport-independent KCS core.
    pub fn new_pio(core: IpmiKcs) -> Self {
        Self {
            core,
            transport: Transport::Pio,
        }
    }

    /// Creates an ARM64 MMIO device around the transport-independent KCS core.
    pub fn new_mmio(core: IpmiKcs) -> Self {
        Self {
            core,
            transport: Transport::Mmio,
        }
    }
}

impl ChangeDeviceState for IpmiKcsDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.core.reset();
    }
}

impl ChipsetDevice for IpmiKcsDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        match self.transport {
            Transport::Pio => Some(self),
            Transport::Mmio => None,
        }
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        match self.transport {
            Transport::Pio => None,
            Transport::Mmio => Some(self),
        }
    }
}

impl PortIoIntercept for IpmiKcsDevice {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        let [value] = data else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };

        *value = match io_port {
            IPMI_KCS_DATA_PORT => self.core.read_data(),
            IPMI_KCS_STATUS_COMMAND_PORT => self.core.read_status(),
            _ => return IoResult::Err(IoError::InvalidRegister),
        };
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        let [value] = data else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };

        match io_port {
            IPMI_KCS_DATA_PORT => self.core.write_data(*value),
            IPMI_KCS_STATUS_COMMAND_PORT => self.core.write_command(*value),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &PIO_REGIONS
    }
}

impl MmioIntercept for IpmiKcsDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        let [value] = data else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };

        *value = match address {
            IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64 => self.core.read_data(),
            IPMI_KCS_MMIO_STATUS_COMMAND_ADDRESS_AARCH64 => self.core.read_status(),
            _ => return IoResult::Err(IoError::InvalidRegister),
        };
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        let [value] = data else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };

        match address {
            IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64 => self.core.write_data(*value),
            IPMI_KCS_MMIO_STATUS_COMMAND_ADDRESS_AARCH64 => self.core.write_command(*value),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        &MMIO_REGIONS
    }
}

impl InspectMut for IpmiKcsDevice {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        let stats = self.core.stats();
        req.respond()
            .hex("status", self.core.read_status())
            .field("sel_records", self.core.sel_len())
            .field(
                "sel_time_offset_seconds",
                self.core.sel_time_offset_seconds(),
            )
            .field("committed", stats.committed)
            .field("forwarded", stats.forwarded)
            .field("rate_limited", stats.rate_limited)
            .field("sink_dropped", stats.sink_dropped);
    }
}

impl SaveRestore for IpmiKcsDevice {
    type SavedState = <IpmiKcs as SaveRestore>::SavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        self.core.save()
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        self.core.restore(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KCS_COMMAND_WRITE_END;
    use crate::KCS_COMMAND_WRITE_START;
    use crate::KCS_DATA_READ_NEXT;
    use crate::KCS_STATE_IDLE;
    use crate::KCS_STATE_READ;
    use crate::STATUS_STATE_MASK;
    use crate::TrustedClock;
    use ipmi_protocol::COMMAND_GET_DEVICE_ID;
    use ipmi_protocol::NETFN_APPLICATION;
    use std::sync::Arc;
    use std::sync::atomic::AtomicI64;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    #[derive(Clone)]
    struct FakeClock(Arc<AtomicI64>);

    impl FakeClock {
        fn new(seconds: i64) -> Self {
            Self(Arc::new(AtomicI64::new(seconds)))
        }
    }

    impl TrustedClock for FakeClock {
        fn unix_seconds(&mut self) -> i64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn device() -> IpmiKcsDevice {
        IpmiKcsDevice::new_pio(IpmiKcs::new(Box::new(FakeClock::new(100))))
    }

    fn mmio_device() -> IpmiKcsDevice {
        IpmiKcsDevice::new_mmio(IpmiKcs::new(Box::new(FakeClock::new(100))))
    }

    fn read(device: &mut IpmiKcsDevice, port: u16) -> u8 {
        let mut data = [0];
        device.io_read(port, &mut data).unwrap();
        data[0]
    }

    fn write(device: &mut IpmiKcsDevice, port: u16, value: u8) {
        device.io_write(port, &[value]).unwrap();
    }

    fn transact(device: &mut IpmiKcsDevice, request: &[u8]) -> Vec<u8> {
        write(
            device,
            IPMI_KCS_STATUS_COMMAND_PORT,
            KCS_COMMAND_WRITE_START,
        );
        assert_eq!(read(device, IPMI_KCS_DATA_PORT), 0);

        for byte in &request[..request.len() - 1] {
            write(device, IPMI_KCS_DATA_PORT, *byte);
            assert_eq!(read(device, IPMI_KCS_DATA_PORT), 0);
        }

        write(device, IPMI_KCS_STATUS_COMMAND_PORT, KCS_COMMAND_WRITE_END);
        assert_eq!(read(device, IPMI_KCS_DATA_PORT), 0);
        write(device, IPMI_KCS_DATA_PORT, request[request.len() - 1]);

        let mut response = Vec::new();
        while read(device, IPMI_KCS_STATUS_COMMAND_PORT) & STATUS_STATE_MASK == KCS_STATE_READ {
            response.push(read(device, IPMI_KCS_DATA_PORT));
            write(device, IPMI_KCS_DATA_PORT, KCS_DATA_READ_NEXT);
        }
        assert_eq!(
            read(device, IPMI_KCS_STATUS_COMMAND_PORT) & STATUS_STATE_MASK,
            KCS_STATE_IDLE
        );
        assert_eq!(read(device, IPMI_KCS_DATA_PORT), 0);
        response
    }

    #[test]
    fn advertises_exact_amd64_pio_region() {
        let mut device = device();
        assert!(device.supports_pio().is_some());
        assert!(device.supports_mmio().is_none());
        assert_eq!(
            PortIoIntercept::get_static_regions(&mut device),
            &[(
                "ipmi-kcs",
                IPMI_KCS_DATA_PORT..=IPMI_KCS_STATUS_COMMAND_PORT
            )]
        );
    }

    #[test]
    fn rejects_invalid_accesses() {
        let mut device = device();

        assert!(matches!(
            device.io_read(IPMI_KCS_DATA_PORT, &mut [0; 2]),
            IoResult::Err(IoError::InvalidAccessSize)
        ));
        assert!(matches!(
            device.io_write(IPMI_KCS_DATA_PORT, &[0; 2]),
            IoResult::Err(IoError::InvalidAccessSize)
        ));
        assert!(matches!(
            device.io_read(IPMI_KCS_DATA_PORT - 1, &mut [0]),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            device.io_write(IPMI_KCS_STATUS_COMMAND_PORT + 1, &[0]),
            IoResult::Err(IoError::InvalidRegister)
        ));
    }

    #[test]
    fn get_device_id_works_over_pio() {
        let mut device = device();
        let response = transact(
            &mut device,
            &[NETFN_APPLICATION << 2, COMMAND_GET_DEVICE_ID],
        );

        assert_eq!(
            response,
            [
                (NETFN_APPLICATION << 2) | 0x04,
                COMMAND_GET_DEVICE_ID,
                0x00,
                0x20,
                0x01,
                0x02,
                0x00,
                0x02,
                0x04,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
            ]
        );
    }

    #[test]
    fn advertises_exact_arm64_mmio_region() {
        let mut device = mmio_device();
        assert!(device.supports_pio().is_none());
        assert!(device.supports_mmio().is_some());
        assert_eq!(
            MmioIntercept::get_static_regions(&mut device),
            &[(
                "ipmi-kcs",
                IPMI_KCS_MMIO_BASE_ADDRESS_AARCH64
                    ..=IPMI_KCS_MMIO_BASE_ADDRESS_AARCH64 + IPMI_KCS_MMIO_REGION_SIZE_AARCH64 - 1
            )]
        );
    }

    #[test]
    fn arm64_mmio_rejects_invalid_accesses() {
        let mut device = mmio_device();

        assert!(matches!(
            device.mmio_read(IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64, &mut [0; 4]),
            IoResult::Err(IoError::InvalidAccessSize)
        ));
        assert!(matches!(
            device.mmio_write(IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64, &[0; 4]),
            IoResult::Err(IoError::InvalidAccessSize)
        ));
        assert!(matches!(
            device.mmio_read(IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64 + 1, &mut [0]),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            device.mmio_write(IPMI_KCS_MMIO_STATUS_COMMAND_ADDRESS_AARCH64 + 1, &[0]),
            IoResult::Err(IoError::InvalidRegister)
        ));
    }

    #[test]
    fn get_device_id_works_over_arm64_mmio() {
        let mut device = mmio_device();

        device
            .mmio_write(
                IPMI_KCS_MMIO_STATUS_COMMAND_ADDRESS_AARCH64,
                &[KCS_COMMAND_WRITE_START],
            )
            .unwrap();
        let mut value = [0];
        device
            .mmio_read(IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64, &mut value)
            .unwrap();
        assert_eq!(value[0], 0);

        device
            .mmio_write(
                IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64,
                &[NETFN_APPLICATION << 2],
            )
            .unwrap();
        device
            .mmio_read(IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64, &mut value)
            .unwrap();
        assert_eq!(value[0], 0);

        device
            .mmio_write(
                IPMI_KCS_MMIO_STATUS_COMMAND_ADDRESS_AARCH64,
                &[KCS_COMMAND_WRITE_END],
            )
            .unwrap();
        device
            .mmio_read(IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64, &mut value)
            .unwrap();
        assert_eq!(value[0], 0);
        device
            .mmio_write(IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64, &[COMMAND_GET_DEVICE_ID])
            .unwrap();

        let mut response = Vec::new();
        loop {
            device
                .mmio_read(IPMI_KCS_MMIO_STATUS_COMMAND_ADDRESS_AARCH64, &mut value)
                .unwrap();
            if value[0] & STATUS_STATE_MASK != KCS_STATE_READ {
                break;
            }
            device
                .mmio_read(IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64, &mut value)
                .unwrap();
            response.push(value[0]);
            device
                .mmio_write(IPMI_KCS_MMIO_DATA_ADDRESS_AARCH64, &[KCS_DATA_READ_NEXT])
                .unwrap();
        }

        assert_eq!(response[0], (NETFN_APPLICATION << 2) | 0x04);
        assert_eq!(response[1], COMMAND_GET_DEVICE_ID);
        assert_eq!(response[2], 0x00);
    }
}
