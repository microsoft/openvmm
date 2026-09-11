// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IPMI KCS (Keyboard Controller Style) device implementation.
//!
//! Exposes a virtual IPMI BMC via the KCS system interface at I/O ports
//! 0xCA2 (data) and 0xCA3 (status/command). Supports System Event Log (SEL)
//! operations and basic device identification.

#![forbid(unsafe_code)]

pub mod protocol;
pub mod resolver;
mod sel;
pub mod sink;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use inspect::InspectMut;
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
use sel::SelStore;
use sink::SelDeps;
use std::collections::VecDeque;
use std::ops::RangeInclusive;
use vmcore::device_state::ChangeDeviceState;

/// IPMI KCS device.
#[derive(InspectMut)]
pub struct IpmiKcsDevice {
    // KCS protocol state
    #[inspect(hex)]
    status: u8,
    #[inspect(hex)]
    data_out: u8,
    #[inspect(with = "Vec::len")]
    write_buffer: Vec<u8>,
    #[inspect(with = "VecDeque::len")]
    read_buffer: VecDeque<u8>,
    write_end_pending: bool,

    // IPMI layer
    sel: SelStore,

    // Static I/O region
    #[inspect(skip)]
    pio_region: (&'static str, RangeInclusive<u16>),
}

impl IpmiKcsDevice {
    /// Create a new IPMI KCS device in the IDLE state with default (no-op sink,
    /// system clock) dependencies.
    pub fn new() -> Self {
        Self::with_deps(SelDeps::default())
    }

    /// Create a new IPMI KCS device with the given SEL egress/clock
    /// dependencies. Used when hosting inside OpenHCL to forward SEL entries.
    pub fn with_deps(deps: SelDeps) -> Self {
        Self {
            status: KcsState::IDLE_STATE.0,
            data_out: 0,
            write_buffer: Vec::new(),
            read_buffer: VecDeque::new(),
            write_end_pending: false,
            sel: SelStore::with_deps(deps),
            pio_region: (
                "ipmi_kcs",
                protocol::KCS_DATA_REG..=protocol::KCS_STATUS_CMD_REG,
            ),
        }
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
                tracelimit::warn_ratelimited!("unexpected READ command on command register");
            }
            KcsCommand::GET_STATUS_ABORT => {
                self.handle_abort();
            }
            _ => {
                tracelimit::warn_ratelimited!(cmd = cmd.0, "unknown KCS command");
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
                tracelimit::warn_ratelimited!(
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
            tracelimit::warn_ratelimited!(len = self.write_buffer.len(), "IPMI message too short");
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
                tracelimit::warn_ratelimited!(netfn = netfn, "unsupported IPMI NetFn");
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
                tracelimit::warn_ratelimited!(cmd = cmd.0, "unsupported App command");
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
            0x00, // Product ID (LS byte first) — 0x0001 = OpenVMM virtual BMC
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

impl ChangeDeviceState for IpmiKcsDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.status = KcsState::IDLE_STATE.0;
        self.data_out = 0;
        self.write_buffer.clear();
        self.read_buffer.clear();
        self.write_end_pending = false;
        self.sel.reset();
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

        data[0] = match io_port {
            protocol::KCS_DATA_REG => {
                // Reading data clears OBF.
                self.status &= !STATUS_OBF;
                self.data_out
            }
            protocol::KCS_STATUS_CMD_REG => {
                // Reading status does not change any state.
                self.status
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        };

        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        match io_port {
            protocol::KCS_DATA_REG => {
                self.handle_data_write(data[0]);
            }
            protocol::KCS_STATUS_CMD_REG => {
                self.handle_command_write(data[0]);
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        IoResult::Ok
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

    /// Helper: simulate a full KCS write-read transaction.
    /// Sends a request and returns the response bytes.
    fn kcs_transfer(dev: &mut IpmiKcsDevice, request: &[u8]) -> Vec<u8> {
        assert!(!request.is_empty(), "request must not be empty");

        // 1. Write WRITE_START to command register.
        dev.io_write(protocol::KCS_STATUS_CMD_REG, &[KcsCommand::WRITE_START.0])
            .unwrap();

        // 2. Write all bytes except the last.
        for &byte in &request[..request.len() - 1] {
            // Wait for OBF (should be set), read dummy.
            assert_obf_set(dev);
            read_data(dev); // dummy read to clear OBF
            dev.io_write(protocol::KCS_DATA_REG, &[byte]).unwrap();
        }

        // 3. Write WRITE_END command.
        dev.io_write(protocol::KCS_STATUS_CMD_REG, &[KcsCommand::WRITE_END.0])
            .unwrap();

        // 4. Read dummy, write last byte.
        assert_obf_set(dev);
        read_data(dev); // dummy read
        dev.io_write(protocol::KCS_DATA_REG, &[*request.last().unwrap()])
            .unwrap();

        // 5. READ phase — collect response bytes.
        let mut response = Vec::new();
        loop {
            assert_obf_set(dev);
            let status = read_status(dev);
            let byte = read_data(dev);

            if KcsState(status & STATUS_STATE_MASK) != KcsState::READ_STATE {
                // IDLE — done.
                break;
            }

            response.push(byte);
            // Acknowledge with READ.
            dev.io_write(protocol::KCS_DATA_REG, &[KcsCommand::READ.0])
                .unwrap();
        }

        response
    }

    fn read_status(dev: &mut IpmiKcsDevice) -> u8 {
        let mut data = [0u8];
        dev.io_read(protocol::KCS_STATUS_CMD_REG, &mut data)
            .unwrap();
        data[0]
    }

    fn read_data(dev: &mut IpmiKcsDevice) -> u8 {
        let mut data = [0u8];
        dev.io_read(protocol::KCS_DATA_REG, &mut data).unwrap();
        data[0]
    }

    fn assert_obf_set(dev: &mut IpmiKcsDevice) {
        let status = read_status(dev);
        assert!(
            status & STATUS_OBF != 0,
            "OBF not set, status: {:#04x}",
            status
        );
    }

    #[test]
    fn kcs_get_device_id() {
        let mut dev = IpmiKcsDevice::new();
        // Get Device ID: NetFn=App(0x06), LUN=0 -> NetFn/LUN = 0x18
        let resp = kcs_transfer(&mut dev, &[0x18, 0x01]);
        // Response: [NetFn/LUN, Cmd, CC, DeviceID, ...]
        assert!(resp.len() >= 3, "response too short: {:?}", resp);
        assert_eq!(resp[0], 0x1C); // App response NetFn/LUN
        assert_eq!(resp[1], 0x01); // Command
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
        assert_eq!(resp[3], 0x20); // Device ID
    }

    #[test]
    fn kcs_sel_add_and_get_roundtrip() {
        let mut dev = IpmiKcsDevice::new();

        // Add SEL Entry: NetFn=Storage(0x0A), LUN=0 -> NetFn/LUN = 0x28
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

        // Get SEL Entry.
        let get_req = vec![
            0x28, 0x43, 0x00, 0x00, // Reservation ID
            resp[3], resp[4], // Record ID
            0x00,    // Offset
            0xFF,    // Read all
        ];

        let resp = kcs_transfer(&mut dev, &get_req);
        assert_eq!(resp[0], 0x2C); // Storage response
        assert_eq!(resp[1], 0x43); // Command
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
        // Next record ID = 0xFFFF.
        assert_eq!(u16::from_le_bytes([resp[3], resp[4]]), 0xFFFF);
        // Verify some fields in the record.
        let record_data = &resp[5..5 + 16];
        assert_eq!(record_data[2], 0x02); // Record type
        assert_eq!(record_data[11], 0x42); // Sensor number (offset 11)
        assert_eq!(record_data[12], 0x6F); // Event type
    }

    #[test]
    fn kcs_unknown_command() {
        let mut dev = IpmiKcsDevice::new();
        // Unknown command under App NetFn.
        let resp = kcs_transfer(&mut dev, &[0x18, 0xFF]);
        assert_eq!(resp[2], CompletionCode::INVALID_COMMAND.0);
    }

    #[test]
    fn kcs_unknown_netfn() {
        let mut dev = IpmiKcsDevice::new();
        // NetFn=0x30 (unknown) -> NetFn/LUN = 0xC0
        let resp = kcs_transfer(&mut dev, &[0xC0, 0x01]);
        assert_eq!(resp[2], CompletionCode::INVALID_COMMAND.0);
    }

    #[test]
    fn kcs_error_recovery() {
        let mut dev = IpmiKcsDevice::new();

        // Start a write but abort mid-stream.
        dev.io_write(protocol::KCS_STATUS_CMD_REG, &[KcsCommand::WRITE_START.0])
            .unwrap();

        // Send abort.
        dev.io_write(
            protocol::KCS_STATUS_CMD_REG,
            &[KcsCommand::GET_STATUS_ABORT.0],
        )
        .unwrap();

        // Should be in READ state with error status.
        let status = read_status(&mut dev);
        assert_eq!(KcsState(status & STATUS_STATE_MASK), KcsState::READ_STATE);

        // Read through the error response.
        assert!(status & STATUS_OBF != 0);
        // Read and acknowledge until IDLE.
        loop {
            let byte_status = read_status(&mut dev);
            let _byte = read_data(&mut dev);
            if KcsState(byte_status & STATUS_STATE_MASK) != KcsState::READ_STATE {
                break;
            }
            dev.io_write(protocol::KCS_DATA_REG, &[KcsCommand::READ.0])
                .unwrap();
        }

        // Now should be in IDLE state.
        let status = read_status(&mut dev);
        assert_eq!(KcsState(status & STATUS_STATE_MASK), KcsState::IDLE_STATE);

        // Verify the device still works after recovery.
        let resp = kcs_transfer(&mut dev, &[0x18, 0x01]);
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
    }

    #[test]
    fn kcs_sel_info_after_operations() {
        let mut dev = IpmiKcsDevice::new();

        // Get SEL Info — should be empty.
        let resp = kcs_transfer(&mut dev, &[0x28, 0x40]);
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
        let count = u16::from_le_bytes([resp[4], resp[5]]);
        assert_eq!(count, 0);

        // Add an entry.
        let sel_record = [0u8; 16];
        let mut add_req = vec![0x28, 0x44];
        add_req.extend_from_slice(&sel_record);
        kcs_transfer(&mut dev, &add_req);

        // Get SEL Info — should show 1.
        let resp = kcs_transfer(&mut dev, &[0x28, 0x40]);
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);
        let count = u16::from_le_bytes([resp[4], resp[5]]);
        assert_eq!(count, 1);
    }

    #[test]
    fn kcs_invalid_access_size() {
        let mut dev = IpmiKcsDevice::new();
        let mut data = [0u8; 2];
        let result = dev.io_read(protocol::KCS_DATA_REG, &mut data);
        assert!(matches!(result, IoResult::Err(IoError::InvalidAccessSize)));

        let result = dev.io_write(protocol::KCS_DATA_REG, &[0, 0]);
        assert!(matches!(result, IoResult::Err(IoError::InvalidAccessSize)));
    }

    #[test]
    fn kcs_invalid_register() {
        let mut dev = IpmiKcsDevice::new();
        let mut data = [0u8];
        let result = dev.io_read(0xCA4, &mut data);
        assert!(matches!(result, IoResult::Err(IoError::InvalidRegister)));

        let result = dev.io_write(0xCA4, &[0]);
        assert!(matches!(result, IoResult::Err(IoError::InvalidRegister)));
    }

    #[test]
    fn kcs_initial_state_is_idle() {
        let dev = IpmiKcsDevice::new();
        assert_eq!(dev.kcs_state(), KcsState::IDLE_STATE);
        assert_eq!(dev.status & STATUS_OBF, 0);
        assert_eq!(dev.status & STATUS_IBF, 0);
    }

    #[test]
    fn kcs_clear_sel_via_kcs() {
        let mut dev = IpmiKcsDevice::new();

        // Add two entries.
        let sel_record = [0u8; 16];
        let mut add_req = vec![0x28, 0x44];
        add_req.extend_from_slice(&sel_record);
        kcs_transfer(&mut dev, &add_req);
        let mut add_req = vec![0x28, 0x44];
        add_req.extend_from_slice(&sel_record);
        kcs_transfer(&mut dev, &add_req);

        // Clear SEL.
        let clear_req = vec![0x28, 0x47, 0x00, 0x00, 0x43, 0x4C, 0x52, 0xAA];
        let resp = kcs_transfer(&mut dev, &clear_req);
        assert_eq!(resp[2], CompletionCode::SUCCESS.0);

        // Verify SEL is empty.
        let resp = kcs_transfer(&mut dev, &[0x28, 0x40]);
        let count = u16::from_le_bytes([resp[4], resp[5]]);
        assert_eq!(count, 0);
    }
}
