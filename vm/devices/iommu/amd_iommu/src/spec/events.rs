// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Event log entry types for the AMD IOMMU.
//!
//! All events are 128 bits (16 bytes). Based on AMD IOMMU Specification
//! Rev 3.11, §2.5.

use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A raw 128-bit event log entry (16 bytes).
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct EventEntry {
    /// First dword: bits 31:16 = DeviceID (for most events), bits 15:0 = event-specific.
    pub dw0: u32,
    /// Second dword: bits 31:28 = EventCode, bits 27:0 = event-specific.
    pub dw1: u32,
    /// Third dword: address low or operand-dependent.
    pub dw2: u32,
    /// Fourth dword: address high or operand-dependent.
    pub dw3: u32,
}

impl EventEntry {
    /// Extract the 4-bit event code from bits 31:28 of dw1.
    pub fn event_code(&self) -> EventCode {
        EventCode((self.dw1 >> 28) as u8)
    }

    /// Extract the DeviceID from bits 31:16 of dw0.
    pub fn device_id(&self) -> u16 {
        (self.dw0 >> 16) as u16
    }
}

open_enum! {
    /// AMD IOMMU event codes (§2.5).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum EventCode: u8 {
        /// §2.5.2 — Illegal device table entry.
        ILLEGAL_DEV_TABLE_ENTRY = 0x01,
        /// §2.5.3 — I/O page fault.
        IO_PAGE_FAULT           = 0x02,
        /// §2.5.4 — Device table hardware error.
        DEV_TAB_HARDWARE_ERROR  = 0x03,
        /// §2.5.5 — Page table hardware error.
        PAGE_TAB_HARDWARE_ERROR = 0x04,
        /// §2.5.6 — Illegal command error.
        ILLEGAL_COMMAND_ERROR   = 0x05,
        /// §2.5.7 — Command hardware error.
        COMMAND_HARDWARE_ERROR  = 0x06,
        /// §2.5.8 — IOTLB invalidation timeout error.
        IOTLB_INV_TIMEOUT       = 0x07,
        /// §2.5.9 — Invalid device request.
        INVALID_DEVICE_REQUEST  = 0x08,
    }
}

// =============================================================================
// Event construction helpers
// =============================================================================

impl EventEntry {
    /// Create an ILLEGAL_DEV_TABLE_ENTRY event (code 1, §2.5.2).
    ///
    /// - `device_id`: the device that caused the fault.
    /// - `is_interrupt`: true if this was an interrupt request, false for DMA.
    /// - `is_write`: true if write, false if read.
    /// - `address`: the faulting address.
    pub fn illegal_dev_table_entry(
        device_id: u16,
        is_interrupt: bool,
        is_write: bool,
        address: u64,
    ) -> Self {
        let flags = (is_write as u32) << 2 | (is_interrupt as u32) << 3;
        Self {
            dw0: (device_id as u32) << 16 | flags,
            dw1: (EventCode::ILLEGAL_DEV_TABLE_ENTRY.0 as u32) << 28,
            dw2: address as u32,
            dw3: (address >> 32) as u32,
        }
    }

    /// Create an IO_PAGE_FAULT event (code 2, §2.5.3).
    ///
    /// - `device_id`: the device that caused the fault.
    /// - `domain_id`: domain ID from the DTE.
    /// - `is_interrupt`: true if this was an interrupt request.
    /// - `is_write`: true if write access.
    /// - `address`: the faulting IOVA.
    pub fn io_page_fault(
        device_id: u16,
        domain_id: u16,
        is_interrupt: bool,
        is_write: bool,
        address: u64,
    ) -> Self {
        let flags = is_interrupt as u32;
        Self {
            dw0: (device_id as u32) << 16 | flags,
            dw1: (EventCode::IO_PAGE_FAULT.0 as u32) << 28 | (domain_id as u32),
            dw2: address as u32,
            dw3: ((is_write as u32) << 31) | ((address >> 32) as u32 & 0x7FFF_FFFF),
        }
    }

    /// Create an ILLEGAL_COMMAND_ERROR event (code 5, §2.5.6).
    ///
    /// - `cmd_address`: physical address of the illegal command in the command buffer.
    pub fn illegal_command_error(cmd_address: u64) -> Self {
        Self {
            dw0: 0,
            dw1: (EventCode::ILLEGAL_COMMAND_ERROR.0 as u32) << 28,
            dw2: cmd_address as u32,
            dw3: (cmd_address >> 32) as u32,
        }
    }

    /// Create a DEV_TAB_HARDWARE_ERROR event (code 3, §2.5.4).
    ///
    /// - `device_id`: the device whose DTE access failed.
    /// - `address`: the physical address that failed to read.
    pub fn dev_tab_hardware_error(device_id: u16, address: u64) -> Self {
        Self {
            dw0: (device_id as u32) << 16,
            dw1: (EventCode::DEV_TAB_HARDWARE_ERROR.0 as u32) << 28,
            dw2: address as u32,
            dw3: (address >> 32) as u32,
        }
    }

    /// Create a PAGE_TAB_HARDWARE_ERROR event (code 4, §2.5.5).
    ///
    /// - `device_id`: the device whose page table access failed.
    /// - `address`: the physical address that failed to read.
    pub fn page_tab_hardware_error(device_id: u16, address: u64) -> Self {
        Self {
            dw0: (device_id as u32) << 16,
            dw1: (EventCode::PAGE_TAB_HARDWARE_ERROR.0 as u32) << 28,
            dw2: address as u32,
            dw3: (address >> 32) as u32,
        }
    }

    /// Create a COMMAND_HARDWARE_ERROR event (code 6, §2.5.7).
    ///
    /// - `cmd_address`: physical address of the command that failed to read.
    pub fn command_hardware_error(cmd_address: u64) -> Self {
        Self {
            dw0: 0,
            dw1: (EventCode::COMMAND_HARDWARE_ERROR.0 as u32) << 28,
            dw2: cmd_address as u32,
            dw3: (cmd_address >> 32) as u32,
        }
    }

    /// Create an INVALID_DEVICE_REQUEST event (code 8, §2.5.9).
    ///
    /// - `device_id`: the device that caused the request.
    /// - `request_type`: sub-type of the invalid request.
    /// - `address`: the faulting address.
    pub fn invalid_device_request(device_id: u16, request_type: u8, address: u64) -> Self {
        Self {
            dw0: (device_id as u32) << 16 | (request_type as u32 & 0xF),
            dw1: (EventCode::INVALID_DEVICE_REQUEST.0 as u32) << 28,
            dw2: address as u32,
            dw3: (address >> 32) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_entry_size() {
        assert_eq!(size_of::<EventEntry>(), 16);
    }

    #[test]
    fn test_event_codes() {
        assert_eq!(EventCode::ILLEGAL_DEV_TABLE_ENTRY.0, 0x01);
        assert_eq!(EventCode::IO_PAGE_FAULT.0, 0x02);
        assert_eq!(EventCode::DEV_TAB_HARDWARE_ERROR.0, 0x03);
        assert_eq!(EventCode::PAGE_TAB_HARDWARE_ERROR.0, 0x04);
        assert_eq!(EventCode::ILLEGAL_COMMAND_ERROR.0, 0x05);
        assert_eq!(EventCode::COMMAND_HARDWARE_ERROR.0, 0x06);
        assert_eq!(EventCode::IOTLB_INV_TIMEOUT.0, 0x07);
        assert_eq!(EventCode::INVALID_DEVICE_REQUEST.0, 0x08);
    }

    #[test]
    fn test_illegal_dev_table_entry_event() {
        let event = EventEntry::illegal_dev_table_entry(0x1234, false, true, 0xDEAD_BEEF_0000);
        assert_eq!(event.event_code(), EventCode::ILLEGAL_DEV_TABLE_ENTRY);
        assert_eq!(event.device_id(), 0x1234);
        // RW bit is bit 2 of dw0
        assert_eq!(event.dw0 & 0x04, 0x04);
        // I bit is bit 3 of dw0
        assert_eq!(event.dw0 & 0x08, 0x00);
        // Address
        assert_eq!(event.dw2, 0xBEEF_0000);
        assert_eq!(event.dw3, 0x0000_DEAD);
    }

    #[test]
    fn test_io_page_fault_event() {
        let event = EventEntry::io_page_fault(0xABCD, 0x0042, true, true, 0x1_0000_0000);
        assert_eq!(event.event_code(), EventCode::IO_PAGE_FAULT);
        assert_eq!(event.device_id(), 0xABCD);
        // I bit (interrupt) is bit 0 of dw0
        assert_eq!(event.dw0 & 0x01, 0x01);
        // DomainID in dw1[15:0]
        assert_eq!(event.dw1 & 0xFFFF, 0x0042);
        // RW bit in dw3[31]
        assert_eq!(event.dw3 & 0x8000_0000, 0x8000_0000);
        // Address
        assert_eq!(event.dw2, 0x0000_0000);
        assert_eq!(event.dw3 & 0x7FFF_FFFF, 0x0000_0001);
    }

    #[test]
    fn test_illegal_command_error_event() {
        let event = EventEntry::illegal_command_error(0xFEDC_BA98_7654_3210);
        assert_eq!(event.event_code(), EventCode::ILLEGAL_COMMAND_ERROR);
        assert_eq!(event.dw0, 0); // No DeviceID
        assert_eq!(event.dw2, 0x7654_3210);
        assert_eq!(event.dw3, 0xFEDC_BA98);
    }

    #[test]
    fn test_invalid_device_request_event() {
        let event = EventEntry::invalid_device_request(0x00FF, 0x03, 0xAAAA_BBBB_CCCC);
        assert_eq!(event.event_code(), EventCode::INVALID_DEVICE_REQUEST);
        assert_eq!(event.device_id(), 0x00FF);
        assert_eq!(event.dw0 & 0x0F, 0x03); // request type
    }
}
