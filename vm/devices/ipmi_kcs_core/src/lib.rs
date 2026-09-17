// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IPMI KCS (Keyboard Controller Style) BMC core: protocol state machine and
//! System Event Log (SEL) handling.
//!
//! This crate is the **shared, dependency-light core** consumed by:
//! - the OpenVMM `ipmi_kcs` chipset device (native Rust, in OpenHCL), and
//! - the `ipmi_kcs_ffi` C ABI staticlib (linked by the Legacy HCL C++ in the OS
//!   Repo),
//!
//! so both hosts share one implementation of the KCS state machine and SEL.
//!
//! It is `no_std` + `alloc` with no host/platform dependencies: forwarding SEL
//! entries and reading the wall clock are injected via [`sink::SelSink`] and
//! [`sink::BmcClock`]. The `inspect` and `trace` features are off by default so
//! minimal-footprint consumers link neither.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

/// Rate-limited warning that compiles to nothing unless the `trace` feature is
/// enabled (so the `no_std` core needs no tracing dependency).
#[cfg(feature = "trace")]
macro_rules! warn_ratelimited {
    ($($t:tt)*) => { tracelimit::warn_ratelimited!($($t)*) };
}
#[cfg(not(feature = "trace"))]
macro_rules! warn_ratelimited {
    ($($t:tt)*) => {};
}

pub mod protocol;
pub mod sink;

mod sel;

pub use sel::SEL_RECORD_SIZE;
pub use sel::SelStore;

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use protocol::CompletionCode;
use protocol::IpmiCommand;
use protocol::IpmiNetFn;
use protocol::KcsCommand;
use protocol::KcsState;
use protocol::STATUS_CD;
use protocol::STATUS_IBF;
use protocol::STATUS_OBF;
use protocol::STATUS_STATE_MASK;
use protocol::set_state_in_status;
use sink::SelDeps;

/// Inclusive I/O port range owned by the KCS interface (0xCA2..=0xCA3).
pub const KCS_PORT_RANGE: core::ops::RangeInclusive<u16> =
    protocol::KCS_DATA_REG..=protocol::KCS_STATUS_CMD_REG;

/// Error returned by [`KcsDevice`] port accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KcsError {
    /// The accessed I/O port is not a KCS register.
    InvalidRegister,
}

/// IPMI KCS BMC core: the KCS protocol state machine plus the SEL store.
///
/// This type owns no bus/intercept machinery — consumers drive it through
/// [`KcsDevice::io_read`] / [`KcsDevice::io_write`] and map the ports/errors
/// into their own device framework (OpenVMM `ChipsetDevice`, or the C FFI).
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct KcsDevice {
    // KCS protocol state
    #[cfg_attr(feature = "inspect", inspect(hex))]
    status: u8,
    #[cfg_attr(feature = "inspect", inspect(hex))]
    data_out: u8,
    #[cfg_attr(feature = "inspect", inspect(with = "Vec::len"))]
    write_buffer: Vec<u8>,
    #[cfg_attr(feature = "inspect", inspect(with = "VecDeque::len"))]
    read_buffer: VecDeque<u8>,
    write_end_pending: bool,

    // IPMI layer
    sel: SelStore,
}

impl KcsDevice {
    /// Create a new IPMI KCS device in the IDLE state with the given SEL
    /// egress/clock dependencies.
    pub fn with_deps(deps: SelDeps) -> Self {
        Self {
            status: KcsState::IDLE_STATE.0,
            data_out: 0,
            write_buffer: Vec::new(),
            read_buffer: VecDeque::new(),
            write_end_pending: false,
            sel: SelStore::with_deps(deps),
        }
    }

    /// Read a KCS register. `KCS_DATA_REG` returns the output byte (clearing
    /// OBF); `KCS_STATUS_CMD_REG` returns the status register.
    pub fn io_read(&mut self, io_port: u16) -> Result<u8, KcsError> {
        let val = match io_port {
            protocol::KCS_DATA_REG => {
                // Reading data clears OBF.
                self.status &= !STATUS_OBF;
                self.data_out
            }
            protocol::KCS_STATUS_CMD_REG => {
                // Reading status does not change any state.
                self.status
            }
            _ => return Err(KcsError::InvalidRegister),
        };
        Ok(val)
    }

    /// Write a KCS register. `KCS_DATA_REG` is a data write; `KCS_STATUS_CMD_REG`
    /// is a command write.
    pub fn io_write(&mut self, io_port: u16, byte: u8) -> Result<(), KcsError> {
        match io_port {
            protocol::KCS_DATA_REG => self.handle_data_write(byte),
            protocol::KCS_STATUS_CMD_REG => self.handle_command_write(byte),
            _ => return Err(KcsError::InvalidRegister),
        }
        Ok(())
    }

    /// Reset to the IDLE state and clear the SEL.
    pub fn reset(&mut self) {
        self.status = KcsState::IDLE_STATE.0;
        self.data_out = 0;
        self.write_buffer.clear();
        self.read_buffer.clear();
        self.write_end_pending = false;
        self.sel.reset();
    }

    /// Get the current KCS state from the status register.
    fn kcs_state(&self) -> KcsState {
        KcsState(self.status & STATUS_STATE_MASK)
    }

    /// Set the KCS state in the status register.
    fn set_kcs_state(&mut self, state: KcsState) {
        self.status = set_state_in_status(self.status, state);
    }

    /// Handle a write to the command register (port 0xCA3).
    fn handle_command_write(&mut self, cmd: u8) {
        let cmd = KcsCommand(cmd);
        self.status |= STATUS_CD; // Mark last write as command.

        match cmd {
            KcsCommand::WRITE_START => {
                self.write_buffer.clear();
                self.write_end_pending = false;
                self.set_kcs_state(KcsState::WRITE_STATE);
                // Set OBF so host reads dummy byte before writing data.
                self.data_out = 0x00;
                self.status |= STATUS_OBF;
            }
            KcsCommand::WRITE_END => {
                // Next data byte will be the last one.
                self.write_end_pending = true;
                self.set_kcs_state(KcsState::WRITE_STATE);
                // Set OBF so host reads dummy byte before writing last byte.
                self.data_out = 0x00;
                self.status |= STATUS_OBF;
            }
            KcsCommand::READ => {
                // This is handled as a data write during READ state.
                // Should not appear on the command register normally.
                warn_ratelimited!("unexpected READ command on command register");
            }
            KcsCommand::GET_STATUS_ABORT => {
                self.handle_abort();
            }
            _ => {
                warn_ratelimited!(cmd = cmd.0, "unknown KCS command");
                self.handle_abort();
            }
        }

        // Clear IBF — we've consumed the command.
        self.status &= !STATUS_IBF;
    }

    /// Handle a write to the data register (port 0xCA2).
    fn handle_data_write(&mut self, byte: u8) {
        self.status &= !STATUS_CD; // Mark last write as data.

        match self.kcs_state() {
            KcsState::WRITE_STATE => {
                self.write_buffer.push(byte);

                if self.write_end_pending {
                    // This was the last byte. Process the complete message.
                    self.write_end_pending = false;
                    self.process_ipmi_message();
                } else {
                    // More bytes expected. Set OBF for dummy read.
                    self.data_out = 0x00;
                    self.status |= STATUS_OBF;
                }
            }
            KcsState::READ_STATE => {
                // Host is acknowledging a byte read (should be READ=0x68).
                // Advance to next byte.
                if let Some(next_byte) = self.read_buffer.pop_front() {
                    self.data_out = next_byte;
                    self.status |= STATUS_OBF;
                    // Stay in READ state.
                } else {
                    // No more bytes — transition to IDLE.
                    self.data_out = 0x00; // Dummy status byte.
                    self.status |= STATUS_OBF;
                    self.set_kcs_state(KcsState::IDLE_STATE);
                }
            }
            _ => {
                warn_ratelimited!(
                    state = self.kcs_state().0,
                    "data write in unexpected state"
                );
            }
        }

        // Clear IBF — we've consumed the data.
        self.status &= !STATUS_IBF;
    }

    /// Handle GET_STATUS/ABORT — recover from error state.
    fn handle_abort(&mut self) {
        self.write_buffer.clear();
        self.read_buffer.clear();
        self.write_end_pending = false;
        // Enter READ state with error status byte.
        self.read_buffer.push_back(0xFF); // Error status.
        self.data_out = 0x00;
        self.status |= STATUS_OBF;
        self.set_kcs_state(KcsState::READ_STATE);
    }

    /// Process a completed IPMI message from the write buffer.
    fn process_ipmi_message(&mut self) {
        if self.write_buffer.len() < 2 {
            warn_ratelimited!(len = self.write_buffer.len(), "IPMI message too short");
            self.enter_error_state();
            return;
        }

        let netfn_lun = self.write_buffer[0];
        let cmd = IpmiCommand(self.write_buffer[1]);
        let data = &self.write_buffer[2..];
        let netfn = protocol::extract_netfn(netfn_lun);

        let response_data = match IpmiNetFn(netfn) {
            IpmiNetFn::APP_REQUEST => self.handle_app_command(cmd, data),
            IpmiNetFn::STORAGE_REQUEST => self.sel.handle_command(cmd, data),
            _ => {
                warn_ratelimited!(netfn = netfn, "unsupported IPMI NetFn");
                vec![CompletionCode::INVALID_COMMAND.0]
            }
        };

        // Build response: [ResponseNetFn/LUN, Cmd, ...response_data]
        let resp_netfn_lun = protocol::response_netfn_lun(netfn_lun);
        self.read_buffer.clear();
        self.read_buffer.push_back(resp_netfn_lun);
        self.read_buffer.push_back(cmd.0);
        for b in response_data {
            self.read_buffer.push_back(b);
        }

        // Enter READ state with first byte ready.
        if let Some(first_byte) = self.read_buffer.pop_front() {
            self.data_out = first_byte;
        }
        self.status |= STATUS_OBF;
        self.set_kcs_state(KcsState::READ_STATE);
    }

    /// Handle App NetFn commands.
    fn handle_app_command(&self, cmd: IpmiCommand, _data: &[u8]) -> Vec<u8> {
        match cmd {
            IpmiCommand::GET_DEVICE_ID => self.cmd_get_device_id(),
            _ => {
                warn_ratelimited!(cmd = cmd.0, "unsupported App command");
                vec![CompletionCode::INVALID_COMMAND.0]
            }
        }
    }

    /// Get Device ID (NetFn=App, Cmd=0x01).
    /// Response format per IPMI v2.0 Section 20.1.
    fn cmd_get_device_id(&self) -> Vec<u8> {
        vec![
            CompletionCode::SUCCESS.0, // Completion code
            0x20,                      // Device ID
            0x01,                      // Device revision
            0x01,                      // Firmware revision 1 (major, bit 7=0 = device available)
            0x00,                      // Firmware revision 2 (minor, BCD)
            0x02,                      // IPMI version 2.0 (BCD: low nibble=major, high=minor)
            0x2D, // Additional device support: SEL + SDR Repo + Sensor + FRU + IPMB Event Receiver
            0x37,
            0x01,
            0x00, // Manufacturer ID (IANA 311 = Microsoft, LS byte first)
            0x01,
            0x00, // Product ID (LS byte first) — 0x0001 = virtual BMC
        ]
    }

    /// Enter error state.
    fn enter_error_state(&mut self) {
        self.write_buffer.clear();
        self.read_buffer.clear();
        self.write_end_pending = false;
        self.set_kcs_state(KcsState::ERROR_STATE);
        self.data_out = 0xFF;
        self.status |= STATUS_OBF;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::BmcClock;
    use crate::sink::NullSelSink;
    use crate::sink::SelDeps;
    use alloc::sync::Arc;
    use test_with_tracing::test;

    struct ZeroClock;
    impl BmcClock for ZeroClock {
        fn now_unix_secs(&self) -> i64 {
            0
        }
    }

    fn new_device() -> KcsDevice {
        KcsDevice::with_deps(SelDeps::new(Arc::new(NullSelSink), Arc::new(ZeroClock)))
    }

    /// Helper: simulate a full KCS write-read transaction.
    fn kcs_transfer(dev: &mut KcsDevice, request: &[u8]) -> Vec<u8> {
        assert!(!request.is_empty(), "request must not be empty");

        dev.io_write(protocol::KCS_STATUS_CMD_REG, KcsCommand::WRITE_START.0)
            .unwrap();

        for &byte in &request[..request.len() - 1] {
            assert_obf_set(dev);
            read_data(dev); // dummy read to clear OBF
            dev.io_write(protocol::KCS_DATA_REG, byte).unwrap();
        }

        dev.io_write(protocol::KCS_STATUS_CMD_REG, KcsCommand::WRITE_END.0)
            .unwrap();

        assert_obf_set(dev);
        read_data(dev); // dummy read
        dev.io_write(protocol::KCS_DATA_REG, *request.last().unwrap())
            .unwrap();

        let mut response = Vec::new();
        loop {
            assert_obf_set(dev);
            let status = read_status(dev);
            let byte = read_data(dev);

            if KcsState(status & STATUS_STATE_MASK) != KcsState::READ_STATE {
                break;
            }

            response.push(byte);
            dev.io_write(protocol::KCS_DATA_REG, KcsCommand::READ.0)
                .unwrap();
        }

        response
    }

    fn read_status(dev: &mut KcsDevice) -> u8 {
        dev.io_read(protocol::KCS_STATUS_CMD_REG).unwrap()
    }

    fn read_data(dev: &mut KcsDevice) -> u8 {
        dev.io_read(protocol::KCS_DATA_REG).unwrap()
    }

    fn assert_obf_set(dev: &mut KcsDevice) {
        let status = read_status(dev);
        assert!(status & STATUS_OBF != 0, "OBF not set, status: {:#04x}", status);
    }

    #[test]
    fn kcs_get_device_id() {
        let mut dev = new_device();
        let resp = kcs_transfer(&mut dev, &[0x18, 0x01]);
        assert!(resp.len() >= 3, "response too short: {:?}", resp);
        assert_eq!(resp[0], 0x1C); // App response NetFn/LUN
        assert_eq!(resp[1], 0x01); // Command
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
        assert_eq!(resp[3], 0x20); // Device ID
    }

    #[test]
    fn kcs_sel_add_and_get_roundtrip() {
        let mut dev = new_device();

        let sel_record: [u8; 16] = [
            0x00, 0x00, // Record ID (ignored)
            0x02, // Record Type
            0x00, 0x00, 0x00, 0x00, // Timestamp
            0x20, 0x00, // Generator ID
            0x04, // EvM Rev
            0x01, // Sensor Type
            0x42, // Sensor Number
            0x6F, // Event Dir/Type
            0x01, 0x02, 0x03, // Event Data
        ];
        let mut add_req = vec![0x28, 0x44];
        add_req.extend_from_slice(&sel_record);

        let resp = kcs_transfer(&mut dev, &add_req);
        assert_eq!(resp[0], 0x2C); // Storage response NetFn/LUN
        assert_eq!(resp[1], 0x44); // Command
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
        let record_id = u16::from_le_bytes([resp[3], resp[4]]);
        assert_eq!(record_id, 1);

        let get_req = vec![
            0x28, 0x43, 0x00, 0x00, // Reservation ID
            resp[3], resp[4], // Record ID
            0x00,    // Offset
            0xFF,    // Read all
        ];

        let resp = kcs_transfer(&mut dev, &get_req);
        assert_eq!(resp[0], 0x2C);
        assert_eq!(resp[1], 0x43);
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
        assert_eq!(u16::from_le_bytes([resp[3], resp[4]]), 0xFFFF);
        let record_data = &resp[5..5 + 16];
        assert_eq!(record_data[2], 0x02); // Record type
        assert_eq!(record_data[11], 0x42); // Sensor number
        assert_eq!(record_data[12], 0x6F); // Event type
    }

    #[test]
    fn kcs_unknown_command() {
        let mut dev = new_device();
        let resp = kcs_transfer(&mut dev, &[0x18, 0xFF]);
        assert_eq!(resp[2], CompletionCode::INVALID_COMMAND.0);
    }

    #[test]
    fn kcs_unknown_netfn() {
        let mut dev = new_device();
        let resp = kcs_transfer(&mut dev, &[0xC0, 0x01]);
        assert_eq!(resp[2], CompletionCode::INVALID_COMMAND.0);
    }

    #[test]
    fn kcs_error_recovery() {
        let mut dev = new_device();

        dev.io_write(protocol::KCS_STATUS_CMD_REG, KcsCommand::WRITE_START.0)
            .unwrap();
        dev.io_write(protocol::KCS_STATUS_CMD_REG, KcsCommand::GET_STATUS_ABORT.0)
            .unwrap();

        let status = read_status(&mut dev);
        assert_eq!(KcsState(status & STATUS_STATE_MASK), KcsState::READ_STATE);
        assert!(status & STATUS_OBF != 0);

        loop {
            let byte_status = read_status(&mut dev);
            let _byte = read_data(&mut dev);
            if KcsState(byte_status & STATUS_STATE_MASK) != KcsState::READ_STATE {
                break;
            }
            dev.io_write(protocol::KCS_DATA_REG, KcsCommand::READ.0)
                .unwrap();
        }

        let status = read_status(&mut dev);
        assert_eq!(KcsState(status & STATUS_STATE_MASK), KcsState::IDLE_STATE);

        let resp = kcs_transfer(&mut dev, &[0x18, 0x01]);
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
    }

    #[test]
    fn kcs_sel_info_after_operations() {
        let mut dev = new_device();

        let resp = kcs_transfer(&mut dev, &[0x28, 0x40]);
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
        let count = u16::from_le_bytes([resp[4], resp[5]]);
        assert_eq!(count, 0);

        let sel_record = [0u8; 16];
        let mut add_req = vec![0x28, 0x44];
        add_req.extend_from_slice(&sel_record);
        kcs_transfer(&mut dev, &add_req);

        let resp = kcs_transfer(&mut dev, &[0x28, 0x40]);
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
        let count = u16::from_le_bytes([resp[4], resp[5]]);
        assert_eq!(count, 1);
    }

    #[test]
    fn kcs_invalid_register() {
        let mut dev = new_device();
        assert_eq!(dev.io_read(0xCA4), Err(KcsError::InvalidRegister));
        assert_eq!(dev.io_write(0xCA4, 0), Err(KcsError::InvalidRegister));
    }

    #[test]
    fn kcs_initial_state_is_idle() {
        let dev = new_device();
        assert_eq!(dev.kcs_state(), KcsState::IDLE_STATE);
        assert_eq!(dev.status & STATUS_OBF, 0);
        assert_eq!(dev.status & STATUS_IBF, 0);
    }

    #[test]
    fn kcs_clear_sel_via_kcs() {
        let mut dev = new_device();

        let sel_record = [0u8; 16];
        let mut add_req = vec![0x28, 0x44];
        add_req.extend_from_slice(&sel_record);
        kcs_transfer(&mut dev, &add_req);
        let mut add_req = vec![0x28, 0x44];
        add_req.extend_from_slice(&sel_record);
        kcs_transfer(&mut dev, &add_req);

        let clear_req = vec![0x28, 0x47, 0x00, 0x00, 0x43, 0x4C, 0x52, 0xAA];
        let resp = kcs_transfer(&mut dev, &clear_req);
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);

        let resp = kcs_transfer(&mut dev, &[0x28, 0x40]);
        let count = u16::from_le_bytes([resp[4], resp[5]]);
        assert_eq!(count, 0);
    }
}
