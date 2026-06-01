// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command buffer entry types for the AMD IOMMU.
//!
//! All commands are 128 bits (16 bytes). Based on AMD IOMMU Specification
//! Rev 3.11, §2.4.

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A raw 128-bit command buffer entry (16 bytes).
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct CommandEntry {
    /// First dword: operand-dependent fields.
    pub dw0: u32,
    /// Second dword: bits 31:28 = opcode, bits 27:0 = operand-dependent.
    pub dw1: u32,
    /// Third dword: second operand low.
    pub dw2: u32,
    /// Fourth dword: second operand high.
    pub dw3: u32,
}

impl CommandEntry {
    /// Extract the 4-bit opcode from bits 31:28 of dw1.
    pub fn opcode(&self) -> CommandOpcode {
        CommandOpcode((self.dw1 >> 28) as u8)
    }
}

open_enum! {
    /// AMD IOMMU command opcodes (§2.4).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum CommandOpcode: u8 {
        /// §2.4.1 — Write completion stamp and/or signal interrupt.
        COMPLETION_WAIT             = 0x01,
        /// §2.4.2 — Invalidate a device table entry.
        INVALIDATE_DEVTAB_ENTRY     = 0x02,
        /// §2.4.3 — Invalidate IOMMU TLB pages.
        INVALIDATE_IOMMU_PAGES      = 0x03,
        /// §2.4.4 — Invalidate IOTLB pages (requires IotlbSup).
        INVALIDATE_IOTLB_PAGES      = 0x04,
        /// §2.4.5 — Invalidate interrupt table entry.
        INVALIDATE_INTERRUPT_TABLE  = 0x05,
        /// §2.4.6 — Prefetch IOMMU pages (requires PreFSup).
        PREFETCH_IOMMU_PAGES        = 0x06,
        /// §2.4.8 — Invalidate all IOMMU state.
        INVALIDATE_IOMMU_ALL        = 0x08,
    }
}

/// COMPLETION_WAIT command fields (opcode 0x01, §2.4.1).
///
/// ```text
/// +00: [31:3]=StoreAddress[31:3], [2]=f (flush), [1]=i (interrupt), [0]=s (store)
/// +04: [31:28]=01h, [19:0]=StoreAddress[51:32]
/// +08: [31:0]=StoreData[31:0]
/// +12: [31:0]=StoreData[63:32]
/// ```
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct CompletionWaitDw0Dw1 {
    /// Store flag — if set, write StoreData to StoreAddress.
    pub s: bool,
    /// Interrupt flag — if set, signal completion interrupt.
    pub i: bool,
    /// Flush flag — if set, flush internal write buffers. No-op for emulator.
    pub f: bool,
    /// Store address bits [31:3].
    #[bits(29)]
    pub store_addr_lo: u32,
    /// Store address bits [51:32].
    #[bits(20)]
    pub store_addr_hi: u32,
    #[bits(8)]
    _reserved: u64,
    /// Opcode (must be 0x01). Bits [31:28] of dw1.
    #[bits(4)]
    _opcode: u8,
}

impl CompletionWaitDw0Dw1 {
    /// Reconstruct the full 52-bit store address (bits 51:3 shifted left by 3).
    pub fn store_address(&self) -> u64 {
        let lo = self.store_addr_lo() as u64;
        let hi = self.store_addr_hi() as u64;
        (hi << 29 | lo) << 3
    }
}

/// Parse a `CommandEntry` as COMPLETION_WAIT fields.
impl From<&CommandEntry> for CompletionWaitDw0Dw1 {
    fn from(entry: &CommandEntry) -> Self {
        let raw = (entry.dw0 as u64) | ((entry.dw1 as u64) << 32);
        CompletionWaitDw0Dw1::from(raw)
    }
}

/// Extract the 64-bit StoreData from a COMPLETION_WAIT command.
pub fn completion_wait_store_data(entry: &CommandEntry) -> u64 {
    (entry.dw2 as u64) | ((entry.dw3 as u64) << 32)
}

/// INVALIDATE_DEVTAB_ENTRY command (opcode 0x02, §2.4.2).
///
/// ```text
/// +00: [15:0]=DeviceID
/// ```
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct InvalidateDevTabEntry {
    /// Device ID (BDF) to invalidate.
    pub device_id: u16,
    _reserved: u16,
}

impl From<&CommandEntry> for InvalidateDevTabEntry {
    fn from(entry: &CommandEntry) -> Self {
        InvalidateDevTabEntry::from(entry.dw0)
    }
}

/// INVALIDATE_IOMMU_PAGES command (opcode 0x03, §2.4.3).
///
/// ```text
/// +00: [19:0]=PASID
/// +04: [31:28]=03h, [15:0]=DomainID
/// +08: [31:12]=Address[31:12], [2]=GN, [1]=PDE, [0]=S
/// +12: [31:0]=Address[63:32]
/// ```
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct InvalidateIommuPagesDw0 {
    /// PASID (process address space ID, not used in our emulator).
    #[bits(20)]
    pub pasid: u32,
    #[bits(12)]
    _reserved: u32,
}

impl From<&CommandEntry> for InvalidateIommuPagesDw0 {
    fn from(entry: &CommandEntry) -> Self {
        InvalidateIommuPagesDw0::from(entry.dw0)
    }
}

/// DomainID from dw1 of INVALIDATE_IOMMU_PAGES.
pub fn invalidate_iommu_pages_domain_id(entry: &CommandEntry) -> u16 {
    entry.dw1 as u16
}

/// Address and control bits from dw2/dw3 of INVALIDATE_IOMMU_PAGES.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct InvalidateIommuPagesDw2Dw3 {
    /// Size bit — 1 = invalidate all pages in domain.
    pub s: bool,
    /// PDE bit — 1 = also invalidate page directory entries.
    pub pde: bool,
    /// Guest/Nested bit.
    pub gn: bool,
    #[bits(9)]
    _reserved: u64,
    /// Address bits [31:12].
    #[bits(20)]
    pub addr_lo: u32,
    /// Address bits [63:32].
    pub addr_hi: u32,
}

impl From<&CommandEntry> for InvalidateIommuPagesDw2Dw3 {
    fn from(entry: &CommandEntry) -> Self {
        let raw = (entry.dw2 as u64) | ((entry.dw3 as u64) << 32);
        InvalidateIommuPagesDw2Dw3::from(raw)
    }
}

/// INVALIDATE_INTERRUPT_TABLE command (opcode 0x05, §2.4.5).
///
/// ```text
/// +00: [15:0]=DeviceID
/// ```
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct InvalidateInterruptTable {
    /// Device ID (BDF) whose interrupt table to invalidate.
    pub device_id: u16,
    _reserved: u16,
}

impl From<&CommandEntry> for InvalidateInterruptTable {
    fn from(entry: &CommandEntry) -> Self {
        InvalidateInterruptTable::from(entry.dw0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_entry_opcode() {
        let entry = CommandEntry {
            dw0: 0,
            dw1: 0x1000_0000, // opcode 1 = COMPLETION_WAIT
            dw2: 0,
            dw3: 0,
        };
        assert_eq!(entry.opcode(), CommandOpcode::COMPLETION_WAIT);
    }

    #[test]
    fn test_command_opcodes() {
        assert_eq!(CommandOpcode::COMPLETION_WAIT.0, 0x01);
        assert_eq!(CommandOpcode::INVALIDATE_DEVTAB_ENTRY.0, 0x02);
        assert_eq!(CommandOpcode::INVALIDATE_IOMMU_PAGES.0, 0x03);
        assert_eq!(CommandOpcode::INVALIDATE_IOTLB_PAGES.0, 0x04);
        assert_eq!(CommandOpcode::INVALIDATE_INTERRUPT_TABLE.0, 0x05);
        assert_eq!(CommandOpcode::PREFETCH_IOMMU_PAGES.0, 0x06);
        assert_eq!(CommandOpcode::INVALIDATE_IOMMU_ALL.0, 0x08);
    }

    #[test]
    fn test_completion_wait_fields() {
        // Build a COMPLETION_WAIT command: s=1, i=1, f=0,
        // StoreAddress = 0x1000 (bits [31:3] = 0x200, bits [51:32] = 0)
        let entry = CommandEntry {
            dw0: 0x0000_1003, // addr[31:3]=0x200, s=1, i=1
            dw1: 0x1000_0000, // opcode=1, addr[51:32]=0
            dw2: 0xDEAD_BEEF, // StoreData low
            dw3: 0xCAFE_BABE, // StoreData high
        };
        assert_eq!(entry.opcode(), CommandOpcode::COMPLETION_WAIT);

        let cw = CompletionWaitDw0Dw1::from(&entry);
        assert!(cw.s());
        assert!(cw.i());
        assert!(!cw.f());
        assert_eq!(cw.store_address(), 0x1000);

        let data = completion_wait_store_data(&entry);
        assert_eq!(data, 0xCAFE_BABE_DEAD_BEEF);
    }

    #[test]
    fn test_completion_wait_store_address_high() {
        // Test with a store address that uses the high bits.
        // StoreAddress = 0x1_0000_0000 (above 4GB)
        // bits [31:3] = 0, bits [51:32] = 0x1_0000_0000 >> 32 = 1
        // But StoreAddr is bits [51:3], so 0x1_0000_0000 >> 3 = 0x2000_0000
        // store_addr_lo (bits [31:3]) = 0, store_addr_hi (bits [51:32]) = 0x1
        let entry = CommandEntry {
            dw0: 0x0000_0001, // s=1, addr_lo=0
            dw1: 0x1000_0001, // opcode=1, addr_hi[19:0] = 1
            dw2: 0,
            dw3: 0,
        };
        let cw = CompletionWaitDw0Dw1::from(&entry);
        assert!(cw.s());
        assert_eq!(cw.store_address(), 1u64 << 32);
    }

    #[test]
    fn test_invalidate_devtab_entry() {
        let entry = CommandEntry {
            dw0: 0x0000_00FF, // DeviceID = 0xFF
            dw1: 0x2000_0000, // opcode=2
            dw2: 0,
            dw3: 0,
        };
        assert_eq!(entry.opcode(), CommandOpcode::INVALIDATE_DEVTAB_ENTRY);
        let inv = InvalidateDevTabEntry::from(&entry);
        assert_eq!(inv.device_id(), 0xFF);
    }

    #[test]
    fn test_invalidate_iommu_pages() {
        let entry = CommandEntry {
            dw0: 0x0000_0000,
            dw1: 0x3000_1234, // opcode=3, DomainID=0x1234
            dw2: 0x1234_5001, // addr[31:12]=0x12345, S=1
            dw3: 0x0000_0000,
        };
        assert_eq!(entry.opcode(), CommandOpcode::INVALIDATE_IOMMU_PAGES);
        assert_eq!(invalidate_iommu_pages_domain_id(&entry), 0x1234);
        let dw2dw3 = InvalidateIommuPagesDw2Dw3::from(&entry);
        assert!(dw2dw3.s());
        assert!(!dw2dw3.pde());
    }

    #[test]
    fn test_invalidate_interrupt_table() {
        let entry = CommandEntry {
            dw0: 0x0000_ABCD,
            dw1: 0x5000_0000, // opcode=5
            dw2: 0,
            dw3: 0,
        };
        assert_eq!(entry.opcode(), CommandOpcode::INVALIDATE_INTERRUPT_TABLE);
        let inv = InvalidateInterruptTable::from(&entry);
        assert_eq!(inv.device_id(), 0xABCD);
    }

    #[test]
    fn test_invalidate_iommu_all() {
        let entry = CommandEntry {
            dw0: 0,
            dw1: 0x8000_0000, // opcode=8
            dw2: 0,
            dw3: 0,
        };
        assert_eq!(entry.opcode(), CommandOpcode::INVALIDATE_IOMMU_ALL);
    }

    #[test]
    fn test_command_entry_size() {
        assert_eq!(size_of::<CommandEntry>(), 16);
    }
}
