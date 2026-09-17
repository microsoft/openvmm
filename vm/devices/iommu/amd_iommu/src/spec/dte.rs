// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device Table Entry (DTE) for the AMD IOMMU.
//!
//! Each DTE is 256 bits (32 bytes), stored as `[u64; 4]`. Based on AMD IOMMU
//! Specification Rev 3.11, §2.2.2, Figure 7, Table 7.

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A raw 256-bit (32-byte) Device Table Entry.
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Dte {
    /// DW0 (bits 63:0) — Address translation fields.
    pub dw0: DteDw0,
    /// DW1 (bits 127:64) — DomainID, GCR3, IoCtl.
    pub dw1: DteDw1,
    /// DW2 (bits 191:128) — Interrupt remapping fields.
    pub dw2: DteDw2,
    /// DW3 (bits 255:192) — Reserved for guest/vIOMMU features.
    pub dw3: u64,
}

/// DTE size in bytes.
pub const DTE_SIZE: usize = 32;

/// DTE DW0 (bits 63:0) — Address Translation.
///
/// Key fields: V (valid), TV (translation valid), Mode (paging levels),
/// HostPTRootPtr (page table root), IR/IW (read/write permission).
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct DteDw0 {
    /// Valid — 1 = this DTE is valid.
    pub v: bool,
    /// Translation Valid — 1 = address translation active.
    pub tv: bool,
    #[bits(2)]
    _reserved1: u64,
    /// CXL memory attribute (bits 6:4). Controls CXL IO bit in ATS
    /// completions. Not used in emulator.
    #[bits(3)]
    pub cxl_mem_attr: u8,
    /// Host Access/Dirty (bits 8:7). Controls whether the IOMMU updates
    /// the Access and Dirty bits in the host page table.
    #[bits(2)]
    pub had: u8,
    /// Paging mode / number of translation levels.
    /// 0 = disabled/pass-through, 1–6 = N-level page table, 7 = reserved.
    #[bits(3)]
    pub mode: u8,
    /// Host page table root pointer, bits [51:12]. 4KB-aligned.
    #[bits(40)]
    pub host_pt_root_ptr: u64,
    /// PPR enable (not used in our emulator).
    pub ppr: bool,
    /// Guest RPT (not used).
    pub gprp: bool,
    /// Guest I/O valid (not used).
    pub giov: bool,
    /// Guest valid (not used).
    pub gv: bool,
    /// Guest CR3 root table level (GLX).
    #[bits(2)]
    pub glx: u8,
    /// GCR3 table root pointer bits [14:12].
    #[bits(3)]
    pub gcr3_trp_14_12: u8,
    /// IOMMU Read permission — 1 = reads allowed.
    pub ir: bool,
    /// IOMMU Write permission — 1 = writes allowed.
    pub iw: bool,
    #[bits(1)]
    _reserved3: u64,
}

impl DteDw0 {
    /// Get the paging mode as a typed enum.
    pub fn paging_mode(&self) -> PagingMode {
        PagingMode(self.mode())
    }

    /// Get the full host page table root pointer address (bits 51:12 shifted left by 12).
    pub fn page_table_root_address(&self) -> u64 {
        self.host_pt_root_ptr() << 12
    }
}

/// DTE DW1 (bits 127:64) — DomainID, GCR3, IoCtl.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct DteDw1 {
    /// Domain ID for TLB invalidation matching (16-bit).
    #[bits(16)]
    pub domain_id: u16,
    /// GCR3 table root pointer bits [30:15].
    #[bits(16)]
    pub gcr3_trp_30_15: u16,
    /// IOTLB enable (bit 96). Controls IOMMU response to ATS requests.
    /// Not used when IotlbSup=0.
    pub i: bool,
    /// Suppress all events.
    pub se: bool,
    /// Suppress all page faults.
    pub sa: bool,
    /// Port I/O control.
    #[bits(2)]
    pub io_ctl: u8,
    /// IOTLB cache hint (bit 101). Not used when IotlbSup=0.
    pub cache: bool,
    /// Snoop disable.
    pub sd: bool,
    /// Exclusion range.
    pub ex: bool,
    /// SysMgt.
    #[bits(2)]
    pub sys_mgt: u8,
    /// SATS.
    pub sats: bool,
    /// GCR3 table root pointer bits [51:31].
    #[bits(21)]
    pub gcr3_trp_51_31: u32,
}

/// DTE DW2 (bits 191:128) — Interrupt Remapping.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct DteDw2 {
    /// Interrupt map valid — 1 = interrupt remapping is active.
    pub iv: bool,
    /// Log2 of interrupt remapping table size.
    /// 0 = 1 entry, 1 = 2, ..., 10 = 1024 entries.
    #[bits(4)]
    pub int_tab_len: u8,
    /// Ignore unmapped interrupt faults.
    pub ig: bool,
    /// Interrupt Remapping Table root pointer, bits [51:6].
    /// Address = value << 6 (128-byte aligned).
    #[bits(46)]
    pub int_tab_root_ptr: u64,
    #[bits(2)]
    _reserved1: u64,
    /// Guest paging mode (not used).
    #[bits(2)]
    pub guest_paging_mode: u8,
    /// Pass INIT interrupts.
    pub init_pass: bool,
    /// Pass ExtInt interrupts.
    pub eint_pass: bool,
    /// Pass NMI interrupts.
    pub nmi_pass: bool,
    /// Host page table mode (not used).
    pub hpt_mode: bool,
    /// Interrupt control mode.
    /// 00 = abort, 01 = pass-through unmapped, 10 = remap, 11 = reserved.
    #[bits(2)]
    pub int_ctl: u8,
    /// Pass LINT0.
    pub lint0_pass: bool,
    /// Pass LINT1.
    pub lint1_pass: bool,
}

impl DteDw2 {
    /// Get the interrupt control mode as a typed enum.
    pub fn int_ctl_mode(&self) -> IntCtl {
        IntCtl(self.int_ctl())
    }

    /// Get the full interrupt remapping table base address.
    /// Address = IntTabRootPtr << 6.
    pub fn int_tab_address(&self) -> u64 {
        self.int_tab_root_ptr() << 6
    }

    /// Get the maximum number of IRT entries: 2^IntTabLen.
    pub fn int_tab_entries(&self) -> u32 {
        1u32 << self.int_tab_len()
    }
}

open_enum! {
    /// DTE paging mode (bits 11:9 of DW0).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum PagingMode: u8 {
        /// Translation disabled / pass-through.
        DISABLED    = 0,
        /// 1-level page table.
        ONE_LEVEL   = 1,
        /// 2-level page table.
        TWO_LEVEL   = 2,
        /// 3-level page table.
        THREE_LEVEL = 3,
        /// 4-level page table.
        FOUR_LEVEL  = 4,
        /// 5-level page table.
        FIVE_LEVEL  = 5,
        /// 6-level page table.
        SIX_LEVEL   = 6,
    }
}

/// Reserved paging mode value (7).
pub const PAGING_MODE_RESERVED: u8 = 7;

open_enum! {
    /// DTE interrupt control mode (IntCtl field in DW2, bits 189:188).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum IntCtl: u8 {
        /// Abort — target abort on interrupt.
        ABORT           = 0b00,
        /// Pass-through — forward unmapped interrupts unchanged.
        PASS_THROUGH    = 0b01,
        /// Remap — remap interrupts via the IRT.
        REMAP           = 0b10,
    }
}

/// Reserved IntCtl value.
pub const INT_CTL_RESERVED: u8 = 0b11;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dte_size() {
        assert_eq!(size_of::<Dte>(), DTE_SIZE);
        assert_eq!(DTE_SIZE, 32);
    }

    #[test]
    fn test_dte_dw0_valid_and_mode() {
        let dw0 = DteDw0::new().with_v(true).with_tv(true).with_mode(4);
        assert!(dw0.v());
        assert!(dw0.tv());
        assert_eq!(dw0.paging_mode(), PagingMode::FOUR_LEVEL);
    }

    #[test]
    fn test_dte_dw0_page_table_root() {
        // Set page table root to physical address 0x1_0000_0000 (4GB).
        // bits [51:12] = 0x1_0000_0000 >> 12 = 0x10_0000
        let dw0 = DteDw0::new().with_host_pt_root_ptr(0x10_0000);
        assert_eq!(dw0.page_table_root_address(), 0x1_0000_0000);
    }

    #[test]
    fn test_dte_dw0_permissions() {
        let dw0 = DteDw0::new().with_ir(true).with_iw(true);
        assert!(dw0.ir());
        assert!(dw0.iw());

        let dw0_ro = DteDw0::new().with_ir(true).with_iw(false);
        assert!(dw0_ro.ir());
        assert!(!dw0_ro.iw());
    }

    #[test]
    fn test_dte_dw1_domain_id() {
        let dw1 = DteDw1::new().with_domain_id(0x1234);
        assert_eq!(dw1.domain_id(), 0x1234);
    }

    #[test]
    fn test_dte_dw2_interrupt_remapping() {
        let dw2 = DteDw2::new()
            .with_iv(true)
            .with_int_tab_len(8) // 2^8 = 256 entries
            .with_int_ctl(IntCtl::REMAP.0)
            .with_int_tab_root_ptr(0x1000); // address = 0x1000 << 6 = 0x40000
        assert!(dw2.iv());
        assert_eq!(dw2.int_ctl_mode(), IntCtl::REMAP);
        assert_eq!(dw2.int_tab_entries(), 256);
        assert_eq!(dw2.int_tab_address(), 0x40000);
    }

    #[test]
    fn test_dte_dw2_int_ctl_modes() {
        assert_eq!(IntCtl::ABORT.0, 0b00);
        assert_eq!(IntCtl::PASS_THROUGH.0, 0b01);
        assert_eq!(IntCtl::REMAP.0, 0b10);
    }

    #[test]
    fn test_dte_dw2_passthrough_interrupts() {
        let dw2 = DteDw2::new()
            .with_init_pass(true)
            .with_eint_pass(true)
            .with_nmi_pass(true)
            .with_lint0_pass(true)
            .with_lint1_pass(true);
        assert!(dw2.init_pass());
        assert!(dw2.eint_pass());
        assert!(dw2.nmi_pass());
        assert!(dw2.lint0_pass());
        assert!(dw2.lint1_pass());
    }

    #[test]
    fn test_paging_modes() {
        assert_eq!(PagingMode::DISABLED.0, 0);
        assert_eq!(PagingMode::ONE_LEVEL.0, 1);
        assert_eq!(PagingMode::TWO_LEVEL.0, 2);
        assert_eq!(PagingMode::THREE_LEVEL.0, 3);
        assert_eq!(PagingMode::FOUR_LEVEL.0, 4);
        assert_eq!(PagingMode::FIVE_LEVEL.0, 5);
        assert_eq!(PagingMode::SIX_LEVEL.0, 6);
    }

    #[test]
    fn test_dte_passthrough_config() {
        // DTE configured for pass-through: V=1, TV=0
        let dte = Dte {
            dw0: DteDw0::new()
                .with_v(true)
                .with_tv(false)
                .with_ir(true)
                .with_iw(true),
            dw1: DteDw1::new(),
            dw2: DteDw2::new(),
            dw3: 0,
        };
        assert!(dte.dw0.v());
        assert!(!dte.dw0.tv());
        assert_eq!(dte.dw0.paging_mode(), PagingMode::DISABLED);
    }

    #[test]
    fn test_dte_full_translation_config() {
        // DTE configured for 4-level translation with interrupt remapping.
        let dte = Dte {
            dw0: DteDw0::new()
                .with_v(true)
                .with_tv(true)
                .with_mode(PagingMode::FOUR_LEVEL.0)
                .with_host_pt_root_ptr(0x100) // root at 0x100000
                .with_ir(true)
                .with_iw(true),
            dw1: DteDw1::new().with_domain_id(42),
            dw2: DteDw2::new()
                .with_iv(true)
                .with_int_tab_len(8)
                .with_int_ctl(IntCtl::REMAP.0)
                .with_int_tab_root_ptr(0x200),
            dw3: 0,
        };
        assert!(dte.dw0.v());
        assert!(dte.dw0.tv());
        assert_eq!(dte.dw0.paging_mode(), PagingMode::FOUR_LEVEL);
        assert_eq!(dte.dw0.page_table_root_address(), 0x100000);
        assert!(dte.dw0.ir());
        assert!(dte.dw0.iw());
        assert_eq!(dte.dw1.domain_id(), 42);
        assert!(dte.dw2.iv());
        assert_eq!(dte.dw2.int_ctl_mode(), IntCtl::REMAP);
        assert_eq!(dte.dw2.int_tab_entries(), 256);
        assert_eq!(dte.dw2.int_tab_address(), 0x200 << 6);
    }
}
