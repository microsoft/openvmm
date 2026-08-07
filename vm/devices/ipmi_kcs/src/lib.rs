// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IPMI KCS chipset device for OpenVMM.
//!
//! Thin OpenVMM integration layer over the shared [`ipmi_kcs_core`] crate:
//! wraps the `no_std` KCS/SEL state machine in the `ChipsetDevice` /
//! `PortIoIntercept` / save-restore framework and supplies a `std`-backed
//! [`SystemClock`]. The protocol/SEL logic itself lives in `ipmi_kcs_core` so it
//! can also be consumed by the C FFI staticlib (`ipmi_kcs_ffi`).

#![forbid(unsafe_code)]

pub mod resolver;

pub use ipmi_kcs_core::KcsError;
pub use ipmi_kcs_core::protocol;
pub use ipmi_kcs_core::sink;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use inspect::InspectMut;
use ipmi_kcs_core::KcsDevice;
use ipmi_kcs_core::sink::BmcClock;
use ipmi_kcs_core::sink::NullSelSink;
use ipmi_kcs_core::sink::SelDeps;
use std::ops::RangeInclusive;
use std::sync::Arc;
use vmcore::device_state::ChangeDeviceState;

/// Wall clock backed by `std::time::SystemTime`.
pub struct SystemClock;

impl BmcClock for SystemClock {
    fn now_unix_secs(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// IPMI KCS device.
#[derive(InspectMut)]
pub struct IpmiKcsDevice {
    #[inspect(flatten)]
    inner: KcsDevice,

    // Static I/O region.
    #[inspect(skip)]
    pio_region: (&'static str, RangeInclusive<u16>),
}

impl IpmiKcsDevice {
    /// Create a new IPMI KCS device in the IDLE state with default (no-op sink,
    /// system clock) dependencies.
    pub fn new() -> Self {
        Self::with_deps(SelDeps::new(Arc::new(NullSelSink), Arc::new(SystemClock)))
    }

    /// Create a new IPMI KCS device with the given SEL egress/clock
    /// dependencies. Used when hosting inside OpenHCL to forward SEL entries.
    pub fn with_deps(deps: SelDeps) -> Self {
        Self {
            inner: KcsDevice::with_deps(deps),
            pio_region: ("ipmi_kcs", ipmi_kcs_core::KCS_PORT_RANGE),
        }
    }
}

impl ChangeDeviceState for IpmiKcsDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.inner.reset();
    }
}

impl ChipsetDevice for IpmiKcsDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }
}

impl PortIoIntercept for IpmiKcsDevice {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        match self.inner.io_read(io_port) {
            Ok(byte) => {
                data[0] = byte;
                IoResult::Ok
            }
            Err(KcsError::InvalidRegister) => IoResult::Err(IoError::InvalidRegister),
        }
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        match self.inner.io_write(io_port, data[0]) {
            Ok(()) => IoResult::Ok,
            Err(KcsError::InvalidRegister) => IoResult::Err(IoError::InvalidRegister),
        }
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        std::slice::from_ref(&self.pio_region)
    }
}

mod save_restore {
    use crate::IpmiKcsDevice;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for IpmiKcsDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let NoSavedState = state;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    /// Access size other than 1 byte is rejected at the bus boundary.
    #[test]
    fn invalid_access_size() {
        let mut dev = IpmiKcsDevice::new();
        let mut data = [0u8; 2];
        assert!(matches!(
            dev.io_read(protocol::KCS_DATA_REG, &mut data),
            IoResult::Err(IoError::InvalidAccessSize)
        ));
        assert!(matches!(
            dev.io_write(protocol::KCS_DATA_REG, &[0, 0]),
            IoResult::Err(IoError::InvalidAccessSize)
        ));
    }

    /// Unknown ports are rejected.
    #[test]
    fn invalid_register() {
        let mut dev = IpmiKcsDevice::new();
        let mut data = [0u8];
        assert!(matches!(
            dev.io_read(0xCA4, &mut data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            dev.io_write(0xCA4, &[0]),
            IoResult::Err(IoError::InvalidRegister)
        ));
    }

    /// The device delegates to the core: a Get Device ID transaction works
    /// through the `PortIoIntercept` wrapper.
    #[test]
    fn get_device_id_through_wrapper() {
        let mut dev = IpmiKcsDevice::new();
        // WRITE_START
        dev.io_write(protocol::KCS_STATUS_CMD_REG, &[protocol::KcsCommand::WRITE_START.0])
            .unwrap();
        // dummy read, write NetFn/LUN byte
        let mut b = [0u8];
        dev.io_read(protocol::KCS_DATA_REG, &mut b).unwrap();
        dev.io_write(protocol::KCS_DATA_REG, &[0x18]).unwrap();
        // WRITE_END, dummy read, last byte (cmd 0x01)
        dev.io_write(protocol::KCS_STATUS_CMD_REG, &[protocol::KcsCommand::WRITE_END.0])
            .unwrap();
        dev.io_read(protocol::KCS_DATA_REG, &mut b).unwrap();
        dev.io_write(protocol::KCS_DATA_REG, &[0x01]).unwrap();

        // Read response bytes.
        let mut resp = Vec::new();
        loop {
            let mut s = [0u8];
            dev.io_read(protocol::KCS_STATUS_CMD_REG, &mut s).unwrap();
            let mut d = [0u8];
            dev.io_read(protocol::KCS_DATA_REG, &mut d).unwrap();
            if protocol::KcsState(s[0] & protocol::STATUS_STATE_MASK)
                != protocol::KcsState::READ_STATE
            {
                break;
            }
            resp.push(d[0]);
            dev.io_write(protocol::KCS_DATA_REG, &[protocol::KcsCommand::READ.0])
                .unwrap();
        }
        assert_eq!(resp[0], 0x1C); // App response NetFn/LUN
        assert_eq!(resp[2], protocol::CompletionCode::SUCCESS.0);
        assert_eq!(resp[3], 0x20); // Device ID
    }
}
