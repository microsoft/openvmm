// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KCS (Keyboard Controller Style) state machine and IPMI message protocol.

use open_enum::open_enum;

open_enum! {
    /// KCS interface states (encoded in status register S1:S0, bits 7:6).
    #[allow(missing_docs)]
    pub enum KcsState: u8 {
        IDLE_STATE  = 0x00,
        READ_STATE  = 0x40,
        WRITE_STATE = 0x80,
        ERROR_STATE = 0xC0,
    }
}

open_enum! {
    /// KCS commands written to the command register.
    #[allow(missing_docs)]
    pub enum KcsCommand: u8 {
        GET_STATUS_ABORT = 0x60,
        WRITE_START      = 0x61,
        WRITE_END        = 0x62,
        READ             = 0x68,
    }
}

open_enum! {
    /// IPMI Network Function codes (upper 6 bits of NetFn/LUN byte).
    #[allow(missing_docs)]
    pub enum IpmiNetFn: u8 {
        APP_REQUEST      = 0x06,
        APP_RESPONSE     = 0x07,
        STORAGE_REQUEST  = 0x0A,
        STORAGE_RESPONSE = 0x0B,
    }
}

open_enum! {
    /// IPMI command codes.
    #[allow(missing_docs)]
    pub enum IpmiCommand: u8 {
        GET_DEVICE_ID   = 0x01,
        GET_SEL_INFO    = 0x40,
        GET_SEL_ENTRY   = 0x43,
        ADD_SEL_ENTRY   = 0x44,
        CLEAR_SEL       = 0x47,
        GET_SEL_TIME    = 0x48,
        SET_SEL_TIME    = 0x49,
    }
}

open_enum! {
    /// IPMI completion codes.
    #[allow(missing_docs)]
    pub enum CompletionCode: u8 {
        SUCCESS                      = 0x00,
        INVALID_COMMAND              = 0xC1,
        REQUEST_DATA_LENGTH_INVALID  = 0xC7,
        INSUFFICIENT_PRIVILEGE       = 0xD4,
    }
}

/// KCS data register I/O port address (Base+0).
pub const KCS_DATA_REG: u16 = 0xCA2;
/// KCS status/command register I/O port address (Base+1).
pub const KCS_STATUS_CMD_REG: u16 = 0xCA3;

/// Status register bit: Output Buffer Full — data available for host to read.
pub const STATUS_OBF: u8 = 0x01;
/// Status register bit: Input Buffer Full — host must wait until 0 before writing.
pub const STATUS_IBF: u8 = 0x02;
/// Status register bit: BMC has a message for the host.
pub const STATUS_SMS_ATN: u8 = 0x04;
/// Status register bit: Command/Data flag — 1 = last write was command.
pub const STATUS_CD: u8 = 0x08;
/// Status register mask for state bits (S1:S0).
pub const STATUS_STATE_MASK: u8 = 0xC0;

/// Encode the state into the status register, preserving other bits.
pub fn set_state_in_status(status: u8, state: KcsState) -> u8 {
    (status & !STATUS_STATE_MASK) | state.0
}

/// Extract the state from the status register.
pub fn get_state_from_status(status: u8) -> KcsState {
    KcsState(status & STATUS_STATE_MASK)
}

/// Build a NetFn/LUN byte from a network function and LUN.
pub fn netfn_lun(netfn: u8, lun: u8) -> u8 {
    (netfn << 2) | (lun & 0x03)
}

/// Extract the NetFn from a NetFn/LUN byte.
pub fn extract_netfn(netfn_lun: u8) -> u8 {
    netfn_lun >> 2
}

/// Extract the LUN from a NetFn/LUN byte.
pub fn extract_lun(netfn_lun: u8) -> u8 {
    netfn_lun & 0x03
}

/// Convert a request NetFn to a response NetFn (set bit 0 of NetFn, which
/// is bit 2 of the NetFn/LUN byte).
pub fn response_netfn_lun(request_netfn_lun: u8) -> u8 {
    request_netfn_lun | 0x04
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn state_encoding() {
        let status = set_state_in_status(0x00, KcsState::IDLE_STATE);
        assert_eq!(get_state_from_status(status), KcsState::IDLE_STATE);

        let status = set_state_in_status(STATUS_OBF | STATUS_IBF, KcsState::READ_STATE);
        assert_eq!(status, 0x40 | STATUS_OBF | STATUS_IBF);
        assert_eq!(get_state_from_status(status), KcsState::READ_STATE);

        let status = set_state_in_status(0x0F, KcsState::WRITE_STATE);
        assert_eq!(get_state_from_status(status), KcsState::WRITE_STATE);
        assert_eq!(status & !STATUS_STATE_MASK, 0x0F);

        let status = set_state_in_status(0x00, KcsState::ERROR_STATE);
        assert_eq!(get_state_from_status(status), KcsState::ERROR_STATE);
    }

    #[test]
    fn netfn_lun_encoding() {
        assert_eq!(netfn_lun(0x06, 0x00), 0x18);
        assert_eq!(netfn_lun(0x0A, 0x00), 0x28);
        assert_eq!(extract_netfn(0x18), 0x06);
        assert_eq!(extract_lun(0x18), 0x00);
        assert_eq!(response_netfn_lun(0x18), 0x1C);
        assert_eq!(response_netfn_lun(0x28), 0x2C);
    }
}
