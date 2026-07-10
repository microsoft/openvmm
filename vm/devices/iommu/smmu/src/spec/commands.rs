// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 command queue entry definitions.
//!
//! Command queue entries are 16 bytes (128 bits). The opcode is in bits `[7:0]`
//! of the first dword.

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

open_enum! {
    /// Command queue opcodes.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum CmdOpcode: u8 {
        /// Prefetch configuration.
        PREFETCH_CFG = 0x01,
        /// Invalidate cached STE.
        CFGI_STE = 0x03,
        /// Invalidate cached STE range (with Range=31 for ALL).
        CFGI_STE_RANGE = 0x04,
        /// Invalidate cached context descriptor.
        CFGI_CD = 0x05,
        /// Invalidate all cached CDs for a stream.
        CFGI_CD_ALL = 0x06,
        /// Invalidate all non-Hyp TLB entries.
        TLBI_NH_ALL = 0x10,
        /// Invalidate non-Hyp TLB entries by ASID.
        TLBI_NH_ASID = 0x11,
        /// Invalidate non-Hyp TLB entry by VA.
        TLBI_NH_VA = 0x12,
        /// Invalidate non-Hyp TLB entry by VA (all ASIDs).
        TLBI_NH_VAA = 0x13,
        /// Invalidate all stage 1+2 TLB entries for a VMID.
        TLBI_S12_VMALL = 0x28,
        /// Invalidate all non-secure non-Hyp TLB entries.
        TLBI_NSNH_ALL = 0x30,
        /// Synchronization command.
        CMD_SYNC = 0x46,
    }
}

/// Raw command queue entry (16 bytes = 2 quadwords).
///
/// Commands are parsed by reading the opcode from the first byte of `qw0`,
/// then interpreting the remaining fields based on the command type.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CmdEntry {
    /// First quadword — contains the opcode and command-specific fields.
    pub qw0: u64,
    /// Second quadword — contains address or other extended fields.
    pub qw1: u64,
}

impl CmdEntry {
    /// Returns the command opcode (bits `[7:0]` of qw0).
    pub fn opcode(&self) -> CmdOpcode {
        CmdOpcode((self.qw0 & 0xFF) as u8)
    }
}

/// CMD_CFGI_STE (opcode 0x03): Invalidate cached STE.
#[bitfield(u64)]
pub struct CmdCfgiSte {
    /// Opcode (bits `[7:0]`).
    #[bits(8)]
    pub opcode: u8,
    /// SSec (bit 8) — non-secure.
    pub ssec: bool,
    #[bits(23)]
    _reserved0: u32,
    /// StreamID (bits `[63:32]`).
    #[bits(32)]
    pub sid: u32,
}

/// CMD_CFGI_STE_RANGE (opcode 0x04): Invalidate cached STE range.
///
/// When Range=31, this is CMD_CFGI_ALL (invalidate all STEs).
#[bitfield(u64)]
pub struct CmdCfgiSteRange {
    /// Opcode (bits `[7:0]`).
    #[bits(8)]
    pub opcode: u8,
    /// SSec (bit 8) — non-secure.
    pub ssec: bool,
    #[bits(23)]
    _reserved0: u32,
    /// StreamID (bits `[63:32]`).
    #[bits(32)]
    pub sid: u32,
}

impl CmdCfgiSteRange {
    /// The range field is in bits `[68:64]` of the full 128-bit entry (low bits of qw1).
    pub fn range_from_entry(entry: &CmdEntry) -> u8 {
        (entry.qw1 & 0x1F) as u8
    }

    /// Range=31 means invalidate ALL STEs.
    pub const RANGE_ALL: u8 = 31;
}

/// CMD_CFGI_CD (opcode 0x05): Invalidate cached context descriptor.
#[bitfield(u64)]
pub struct CmdCfgiCd {
    /// Opcode (bits `[7:0]`).
    #[bits(8)]
    pub opcode: u8,
    /// SSec (bit 8) — non-secure.
    pub ssec: bool,
    #[bits(3)]
    _reserved0: u32,
    /// SubstreamID (bits `[31:12]`).
    #[bits(20)]
    pub ssid: u32,
    /// StreamID (bits `[63:32]`).
    #[bits(32)]
    pub sid: u32,
}

/// CMD_TLBI_NH_ASID (opcode 0x11): Invalidate TLB by ASID.
#[bitfield(u64)]
pub struct CmdTlbiNhAsid {
    /// Opcode (bits `[7:0]`).
    #[bits(8)]
    pub opcode: u8,
    #[bits(24)]
    _reserved0: u32,
    /// VMID (bits `[47:32]`).
    #[bits(16)]
    pub vmid: u16,
    /// ASID (bits `[63:48]`).
    #[bits(16)]
    pub asid: u16,
}

/// CMD_TLBI_NH_VA (opcode 0x12): Invalidate TLB by virtual address.
#[bitfield(u64)]
pub struct CmdTlbiNhVa {
    /// Opcode (bits `[7:0]`).
    #[bits(8)]
    pub opcode: u8,
    #[bits(24)]
    _reserved0: u32,
    /// VMID (bits `[47:32]`).
    #[bits(16)]
    pub vmid: u16,
    /// ASID (bits `[63:48]`).
    #[bits(16)]
    pub asid: u16,
}

impl CmdTlbiNhVa {
    /// The address field is in bits `[127:68]` of the full 128-bit entry.
    /// This extracts the VA from the raw entry (address bits `[63:12]`).
    pub fn addr_from_entry(entry: &CmdEntry) -> u64 {
        let shifted = entry.qw1 >> 4; // bits [127:68] → bits [59:0]
        (shifted & ((1u64 << 52) - 1)) << 12
    }

    /// Leaf bit is at bit 64 of the 128-bit entry (bit 0 of qw1).
    pub fn leaf_from_entry(entry: &CmdEntry) -> bool {
        entry.qw1 & 1 != 0
    }
}

/// CMD_SYNC (opcode 0x46): Synchronization command.
#[bitfield(u64)]
pub struct CmdSync {
    /// Opcode (bits `[7:0]`).
    #[bits(8)]
    pub opcode: u8,
    #[bits(4)]
    _reserved0: u32,
    /// Completion signal type (bits `[13:12]`).
    #[bits(2)]
    pub cs: u8,
    #[bits(8)]
    _reserved1: u32,
    /// MSI shareability (bits `[23:22]`).
    #[bits(2)]
    pub msh: u8,
    /// MSI attributes (bits `[27:24]`).
    #[bits(4)]
    pub msi_attr: u8,
    #[bits(4)]
    _reserved2: u32,
    /// MSI data (bits `[63:32]`).
    #[bits(32)]
    pub msi_data: u32,
}

impl CmdSync {
    /// Extract the MSI address from the full 128-bit command entry.
    /// MSI address is in bits `[119:66]` → address `[55:2]`.
    pub fn msi_addr_from_entry(entry: &CmdEntry) -> u64 {
        let shifted = entry.qw1 >> 2; // bits [119:66] → bits [53:0]
        shifted & ((1u64 << 54) - 1)
    }

    /// Returns the full MSI address (with bits `[1:0]` = 0).
    pub fn msi_write_addr_from_entry(entry: &CmdEntry) -> u64 {
        Self::msi_addr_from_entry(entry) << 2
    }
}

open_enum! {
    /// CMD_SYNC completion signal types.
    pub enum SyncCs: u8 {
        /// No signal.
        SIG_NONE = 0b00,
        /// Send MSI/IRQ.
        SIG_IRQ = 0b01,
        /// Send SEV wakeup event.
        SIG_SEV = 0b10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cmd_opcodes() {
        assert_eq!(CmdOpcode::PREFETCH_CFG.0, 0x01);
        assert_eq!(CmdOpcode::CFGI_STE.0, 0x03);
        assert_eq!(CmdOpcode::CFGI_STE_RANGE.0, 0x04);
        assert_eq!(CmdOpcode::CFGI_CD.0, 0x05);
        assert_eq!(CmdOpcode::CFGI_CD_ALL.0, 0x06);
        assert_eq!(CmdOpcode::TLBI_NH_ALL.0, 0x10);
        assert_eq!(CmdOpcode::TLBI_NH_ASID.0, 0x11);
        assert_eq!(CmdOpcode::TLBI_NH_VA.0, 0x12);
        assert_eq!(CmdOpcode::TLBI_NH_VAA.0, 0x13);
        assert_eq!(CmdOpcode::TLBI_NSNH_ALL.0, 0x30);
        assert_eq!(CmdOpcode::CMD_SYNC.0, 0x46);
    }

    #[test]
    fn test_cmd_entry_opcode() {
        let entry = CmdEntry { qw0: 0x46, qw1: 0 };
        assert_eq!(entry.opcode(), CmdOpcode::CMD_SYNC);
    }

    #[test]
    fn test_cmd_sync_msi_addr() {
        // MSI address = 0x1234_5678
        // Stored in qw1 bits [55:2] as (addr >> 2) << 2
        let addr: u64 = 0x1234_5678;
        let addr_shifted = addr >> 2;
        let entry = CmdEntry {
            qw0: CmdOpcode::CMD_SYNC.0 as u64,
            qw1: addr_shifted << 2,
        };
        assert_eq!(CmdSync::msi_write_addr_from_entry(&entry), addr & !0x3);
    }

    #[test]
    fn test_cfgi_ste_range_all() {
        let entry = CmdEntry {
            qw0: CmdOpcode::CFGI_STE_RANGE.0 as u64,
            qw1: 31,
        };
        assert_eq!(
            CmdCfgiSteRange::range_from_entry(&entry),
            CmdCfgiSteRange::RANGE_ALL
        );
    }

    #[test]
    fn test_cmd_entry_size() {
        assert_eq!(size_of::<CmdEntry>(), 16);
    }
}
