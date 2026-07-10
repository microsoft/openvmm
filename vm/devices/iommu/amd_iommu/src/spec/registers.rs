// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MMIO register definitions and PCI capability block types for the AMD IOMMU.
//!
//! Based on AMD IOMMU Specification Rev 3.11, §3.2 (PCI Capability Block) and
//! §3.4 (MMIO Register Map).

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;

// =============================================================================
// MMIO Register Offsets (§3.4)
// =============================================================================

open_enum! {
    /// MMIO register offsets for the AMD IOMMU.
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum MmioRegister: u16 {
        /// Device Table Base Address Register (64-bit).
        DEV_TAB_BASE        = 0x0000,
        /// Command Buffer Base Address Register (64-bit).
        CMD_BUF_BASE        = 0x0008,
        /// Event Log Base Address Register (64-bit).
        EVT_LOG_BASE        = 0x0010,
        /// IOMMU Control Register (64-bit).
        IOMMU_CTRL          = 0x0018,
        /// Exclusion Base Register (64-bit).
        EXCL_BASE           = 0x0020,
        /// Exclusion Range Limit Register (64-bit).
        EXCL_LIMIT          = 0x0028,
        /// Extended Feature Register (64-bit, RO).
        EXT_FEAT            = 0x0030,
        /// General XT Interrupt Control Register (64-bit). §3.4.8.
        ///
        /// IOMMU's own MSI destination in X2APIC (XT) format. Used when
        /// `IntCapXTEn=1` for the event log / IOMMU general interrupt,
        /// replacing the legacy MSI capability registers at 0x158..0x164.
        GEN_XT_INT_CTRL     = 0x0170,
        /// PPR XT Interrupt Control Register (64-bit). §3.4.8.
        ///
        /// IOMMU's own MSI destination in X2APIC (XT) format for the PPR
        /// log interrupt. Used when `IntCapXTEn=1`.
        PPR_XT_INT_CTRL     = 0x0178,
        /// Command Buffer Head Pointer (64-bit).
        CMD_BUF_HEAD        = 0x2000,
        /// Command Buffer Tail Pointer (64-bit).
        CMD_BUF_TAIL        = 0x2008,
        /// Event Log Head Pointer (64-bit).
        EVT_LOG_HEAD        = 0x2010,
        /// Event Log Tail Pointer (64-bit).
        EVT_LOG_TAIL        = 0x2018,
        /// IOMMU Status Register (64-bit).
        IOMMU_STATUS        = 0x2020,
        /// Extended Feature Register 2 (MMIO offset 0x01A0, RO).
        EXT_FEAT2           = 0x01A0,
    }
}

// =============================================================================
// MMIO Register Bitfield Definitions
// =============================================================================

/// Device Table Base Address Register (MMIO offset 0x0000, 64-bit).
///
/// §3.4.1. Holds the base address and size of the device table.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct DevTabBase {
    /// Table size: number of entries = (size + 1) * 4096 / 32.
    /// Max value 0x1FF = 2MB = 65536 entries.
    #[bits(9)]
    pub size: u16,
    #[bits(3)]
    _reserved1: u64,
    /// Device table base address, bits [51:12]. 4KB-aligned.
    #[bits(40)]
    pub base_addr: u64,
    #[bits(12)]
    _reserved2: u64,
}

/// Command Buffer Base Address Register (MMIO offset 0x0008, 64-bit).
///
/// §3.4.2. Holds the base address and size of the command buffer.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct CmdBufBase {
    #[bits(12)]
    _reserved1: u64,
    /// Command buffer base address, bits [51:12]. 4KB-aligned.
    #[bits(40)]
    pub base_addr: u64,
    #[bits(4)]
    _reserved2: u64,
    /// Log2 of buffer length in entries. Min 0b1000 (256 entries).
    #[bits(4)]
    pub length: u8,
    #[bits(4)]
    _reserved3: u64,
}

/// Event Log Base Address Register (MMIO offset 0x0010, 64-bit).
///
/// §3.4.3. Holds the base address and size of the event log.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct EvtLogBase {
    #[bits(12)]
    _reserved1: u64,
    /// Event log base address, bits [51:12]. 4KB-aligned.
    #[bits(40)]
    pub base_addr: u64,
    #[bits(4)]
    _reserved2: u64,
    /// Log2 of event log length in entries. Min 0b1000 (256 entries).
    #[bits(4)]
    pub length: u8,
    #[bits(4)]
    _reserved3: u64,
}

/// IOMMU Control Register (MMIO offset 0x0018, 64-bit).
///
/// §3.4.4. Master control register for the IOMMU.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IommuCtrl {
    /// Master enable for all IOMMU translation.
    pub iommu_en: bool,
    /// HyperTransport tunnel enable (stored, no effect in emulator).
    pub ht_tun_en: bool,
    /// Event logging enable.
    pub evt_log_en: bool,
    /// Event log interrupt enable.
    pub evt_int_en: bool,
    /// Completion wait interrupt enable.
    pub com_wait_int_en: bool,
    /// Invalidation timeout (stored, no effect in emulator).
    #[bits(3)]
    pub inv_timeout: u8,
    /// Pass posted writes (HT attribute, stored).
    pub pass_pw: bool,
    /// Response pass posted writes (HT attribute, stored).
    pub res_pass_pw: bool,
    /// Coherent (HT attribute, stored).
    pub coherent: bool,
    /// Isochronous (HT attribute, stored).
    pub isoc: bool,
    /// Command buffer enable.
    pub cmd_buf_en: bool,
    /// PPR log enable (stored, no effect).
    pub ppr_log_en: bool,
    /// PPR interrupt enable (stored, no effect).
    pub ppr_int_en: bool,
    /// PPR enable (stored, no effect).
    pub ppr_en: bool,
    /// Guest translation enable (stored, no effect).
    pub gt_en: bool,
    /// Guest virtual APIC enable (stored, no effect).
    pub ga_en: bool,
    #[bits(4)]
    _reserved1: u64,
    /// SMI filter enable (stored, no effect).
    pub smif_en: bool,
    /// Self-writeback dirty enable (stored, no effect).
    pub slf_wb_dis: bool,
    /// SMI filter log enable (stored, no effect).
    pub smif_log_en: bool,
    #[bits(3)]
    _reserved2: u64,
    /// GA log enable (stored, no effect). Bit 28.
    pub ga_log_en: bool,
    /// GA interrupt enable (stored, no effect). Bit 29.
    pub ga_int_en: bool,
    /// Dual PPR log enable (stored, no effect). Bits 31:30.
    #[bits(2)]
    pub dual_ppr_log_en: u8,
    /// Dual event log enable (stored, no effect). Bits 33:32.
    #[bits(2)]
    pub dual_evt_log_en: u8,
    /// Device table segmentation enable (stored, no effect). Bits 36:34.
    #[bits(3)]
    pub dev_tbl_seg_en: u8,
    /// Privilege abort enable (stored, no effect). Bits 38:37.
    #[bits(2)]
    pub priv_abrt_en: u8,
    /// PPR auto-response enable (stored, no effect). Bit 39.
    pub ppr_auto_rsp_en: bool,
    /// Memory access routing and control enable (stored, no effect). Bit 40.
    pub marc_en: bool,
    /// Block stop marker enable (stored, no effect). Bit 41.
    pub blk_stop_mrk_en: bool,
    /// PPR auto response always-on (stored, no effect). Bit 42.
    pub ppr_auto_rsp_aon: bool,
    /// Interrupt remapping mode: number of interrupts per function.
    /// 00b = 512 interrupts/function. Bits 44:43.
    #[bits(2)]
    pub num_int_remap_mode: u8,
    /// Enhanced PPR handling enable (stored, no effect). Bit 45.
    pub eph_en: bool,
    /// Host access/dirty update mode (stored, no effect). Bits 47:46.
    #[bits(2)]
    pub had_update: u8,
    /// Guest dirty update disable (stored, no effect). Bit 48.
    pub gd_update_dis: bool,
    #[bits(1)]
    _reserved3: u64,
    /// x2APIC enable (stored, no effect). Bit 50.
    pub xt_en: bool,
    /// IOMMU x2APIC interrupt generation enable (stored, no effect). Bit 51.
    pub int_cap_xt_en: bool,
    /// Virtualized command buffer enable (stored, no effect). Bit 52.
    pub vcmd_en: bool,
    /// Virtualized IOMMU enable (stored, no effect). Bit 53.
    pub viommu_en: bool,
    /// GA update disable (stored, no effect). Bit 54.
    pub ga_update_dis: bool,
    /// Guest APIC physical processor interrupt enable (stored, no effect). Bit 55.
    pub gappi_en: bool,
    /// Tiered memory page migration enable (stored, no effect). Bit 56.
    pub tmpm_en: bool,
    #[bits(1)]
    _reserved4: u64,
    /// GCR3 table root pointer mode (stored, no effect). Bit 58.
    pub gcr3_trp_mode: bool,
    /// IRT cache disable (stored, no effect). Bit 59.
    pub irt_cache_dis: bool,
    /// Guest buffer TRP mode (stored, no effect). Bit 60.
    pub gst_buffer_trp_mode: bool,
    /// SNP AVIC enable (stored, no effect). Bits 63:61.
    #[bits(3)]
    pub snp_avic_en: u8,
}

/// Exclusion Base Register (MMIO offset 0x0020, 64-bit).
///
/// §3.4.5. Exclusion range base address.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct ExclBase {
    /// Exclusion range enable.
    pub ex_en: bool,
    /// Allow all devices in exclusion range.
    pub allow: bool,
    #[bits(10)]
    _reserved1: u64,
    /// Exclusion base address, bits [51:12].
    #[bits(40)]
    pub base_addr: u64,
    #[bits(12)]
    _reserved2: u64,
}

/// Exclusion Range Limit Register (MMIO offset 0x0028, 64-bit).
///
/// §3.4.6. Exclusion range upper limit.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct ExclLimit {
    #[bits(12)]
    _reserved1: u64,
    /// Exclusion limit address, bits [51:12].
    #[bits(40)]
    pub limit_addr: u64,
    #[bits(12)]
    _reserved2: u64,
}

/// Extended Feature Register (MMIO offset 0x0030, 64-bit, RO).
///
/// §3.4.7. Reports IOMMU hardware capabilities.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct ExtFeat {
    /// Prefetch support.
    pub pref_sup: bool,
    /// PPR support.
    pub ppr_sup: bool,
    /// x2APIC support.
    pub xt_sup: bool,
    /// NX (no-execute) bit support.
    pub nx_sup: bool,
    /// Guest translation support.
    pub gt_sup: bool,
    /// Guest APIC physical interrupt support.
    pub gappi_sup: bool,
    /// INVALIDATE_IOMMU_ALL support.
    pub ia_sup: bool,
    /// Guest virtual APIC support.
    pub ga_sup: bool,
    /// Hardware error register support.
    pub he_sup: bool,
    /// Performance counter support.
    pub pc_sup: bool,
    /// Host Address Translation Size (00 = 4-level, 01 = 5-level, 10 = 6-level).
    #[bits(2)]
    pub hats: u8,
    /// Guest Address Translation Size.
    #[bits(2)]
    pub gats: u8,
    /// Guest CR3 root pointer table level support.
    #[bits(2)]
    pub glx_sup: u8,
    /// SMI filter register support.
    #[bits(2)]
    pub smif_sup: u8,
    /// SMI filter register count.
    #[bits(3)]
    pub smif_rc: u8,
    /// Guest APIC mode support.
    #[bits(3)]
    pub gam_sup: u8,
    /// Dual PPR log support.
    #[bits(2)]
    pub dual_ppr_log_sup: u8,
    #[bits(2)]
    _reserved1: u64,
    /// Dual event log support.
    #[bits(2)]
    pub dual_evt_log_sup: u8,
    #[bits(1)]
    _reserved2: u64,
    /// SATS (secure ATS) support.
    pub sats_sup: bool,
    /// PAS max.
    #[bits(5)]
    pub pas_max: u8,
    /// User/supervisor support.
    pub us_sup: bool,
    /// Device table segmentation support.
    #[bits(2)]
    pub dev_tbl_seg_sup: u8,
    /// PPR overflow early warning.
    pub ppr_ovrflw_early: bool,
    /// PPR auto-response support.
    pub ppr_auto_rsp_sup: bool,
    /// MARC (memory access routing and control) support.
    #[bits(2)]
    pub marc_sup: u8,
    /// Block stop marker support.
    pub blk_stop_mrk_sup: bool,
    /// Performance optimization support.
    pub perf_opt_sup: bool,
    /// MSI capability MMIO access support.
    pub msi_cap_mmio_sup: bool,
    /// Snoop attribute support.
    pub snoop_attr_sup: bool,
    /// Guest I/O protection support.
    pub gio_sup: bool,
    /// Host access/dirty support.
    pub ha_sup: bool,
    /// Enhanced PPR handling support.
    pub eph_sup: bool,
    /// Attribute forward support.
    pub attr_fw_sup: bool,
    /// HD (host dirty) support.
    pub hd_sup: bool,
    /// v2 HD (host dirty) disable support.
    pub v2_hd_dis_sup: bool,
    /// Invalidate IOTLB type support.
    pub inv_iotlb_type_sup: bool,
    /// vIOMMU support.
    pub viommu_sup: bool,
    /// VM guard I/O support.
    pub vm_guard_io_sup: bool,
    #[bits(4)]
    _reserved3: u64,
    /// v2 Host Access/Dirty disable support.
    pub v2_had_dis_sup: bool,
    /// Force physical destination mode support.
    pub force_phy_dest: bool,
    /// SNP support.
    pub snp_sup: bool,
}

/// XT Interrupt Control Register (MMIO offsets 0x0170, 0x0178, 64-bit).
///
/// §3.4.13. Specifies the IOMMU's own MSI destination in x2APIC format,
/// used when `CONTROL[IntCapXTEn]=1`. Implemented when `EFR[XTSup]=1`. There
/// are two instances with the same layout: `GEN_XT_INT_CTRL` (0x0170) for
/// the event log / general interrupt and `PPR_XT_INT_CTRL` (0x0178) for the
/// PPR log interrupt.
///
/// The emulator stores writes to these registers verbatim but does not source
/// MSIs from them, because IOMMU-internal interrupts are not delivered to the
/// guest (events are surfaced through `IOMMU_STATUS` only).
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct XtIntCtrl {
    #[bits(2)]
    _reserved1: u64,
    /// Interrupt destination mode (0 = physical, 1 = logical).
    pub xt_int_dest_mode: bool,
    #[bits(5)]
    _reserved2: u64,
    /// Destination APIC ID, low 24 bits.
    #[bits(24)]
    pub xt_int_dest_low: u32,
    /// Interrupt vector.
    #[bits(8)]
    pub xt_int_vector: u8,
    /// Delivery mode (0 = fixed, etc.; AMD reuses APIC encoding).
    pub xt_int_dm: bool,
    #[bits(15)]
    _reserved3: u64,
    /// Destination APIC ID, high 8 bits.
    #[bits(8)]
    pub xt_int_dest_high: u8,
}

/// Command Buffer Head Pointer Register (MMIO offset 0x2000, 64-bit).
///
/// §3.4.9. Read by software to determine which commands have been consumed.
/// Updated by the IOMMU as commands are processed.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct CmdBufHead {
    #[bits(4)]
    _reserved1: u64,
    /// Head pointer offset, 16-byte aligned (bits [18:4]).
    #[bits(15)]
    pub head_ptr: u32,
    #[bits(45)]
    _reserved2: u64,
}

/// Command Buffer Tail Pointer Register (MMIO offset 0x2008, 64-bit).
///
/// §3.4.10. Written by software to indicate new commands have been added.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct CmdBufTail {
    #[bits(4)]
    _reserved1: u64,
    /// Tail pointer offset, 16-byte aligned (bits [18:4]).
    #[bits(15)]
    pub tail_ptr: u32,
    #[bits(45)]
    _reserved2: u64,
}

/// Event Log Head Pointer Register (MMIO offset 0x2010, 64-bit).
///
/// §3.4.11. Written by software after consuming event log entries.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct EvtLogHead {
    #[bits(4)]
    _reserved1: u64,
    /// Head pointer offset, 16-byte aligned (bits [18:4]).
    #[bits(15)]
    pub head_ptr: u32,
    #[bits(45)]
    _reserved2: u64,
}

/// Event Log Tail Pointer Register (MMIO offset 0x2018, 64-bit).
///
/// §3.4.12. Updated by the IOMMU after writing event log entries.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct EvtLogTail {
    #[bits(4)]
    _reserved1: u64,
    /// Tail pointer offset, 16-byte aligned (bits [18:4]).
    #[bits(15)]
    pub tail_ptr: u32,
    #[bits(45)]
    _reserved2: u64,
}

/// IOMMU Status Register (MMIO offset 0x2020, 64-bit).
///
/// §3.4.13. Bits 0–2 are RW1C (write-1-to-clear), bits 3–4 are RO.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IommuStatus {
    /// Event log overflow (RW1C). Set when the event log is full.
    pub evt_overflow: bool,
    /// Event log interrupt (RW1C). Set when an event is written to the log.
    pub evt_log_int: bool,
    /// Completion wait interrupt (RW1C). Set by COMPLETION_WAIT with i=1.
    pub com_wait_int: bool,
    /// Event log running (RO). 1 = the IOMMU is logging events.
    pub evt_log_run: bool,
    /// Command buffer running (RO). 1 = the IOMMU is fetching commands.
    pub cmd_buf_run: bool,
    #[bits(5)]
    _reserved1: u64,
    /// PPR overflow (RW1C, not implemented).
    pub ppr_overflow: bool,
    /// PPR interrupt (RW1C, not implemented).
    pub ppr_int: bool,
    /// PPR log running (RO, not implemented).
    pub ppr_log_run: bool,
    /// Guest log running (RO, not implemented).
    pub ga_log_run: bool,
    /// Guest log overflow (RW1C, not implemented).
    pub ga_log_overflow: bool,
    /// Guest log interrupt (RW1C, not implemented).
    pub ga_log_int: bool,
    #[bits(48)]
    _reserved2: u64,
}

// =============================================================================
// PCI Capability Block Types (§3.2)
// =============================================================================

open_enum! {
    /// PCI Capability offset within the AMD IOMMU capability block.
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum CapabilityOffset: u8 {
        /// Capability header (32-bit).
        HEADER      = 0x00,
        /// Base address low (32-bit).
        BASE_LOW    = 0x04,
        /// Base address high (32-bit).
        BASE_HIGH   = 0x08,
        /// Range register (32-bit).
        RANGE       = 0x0C,
        /// Miscellaneous information 0 (32-bit).
        MISC_INFO_0 = 0x10,
    }
}

/// AMD IOMMU Capability ID in PCI config space.
pub const CAP_ID: u8 = 0x0F;

/// AMD IOMMU PCI Capability Header (offset 00h, 32-bit).
///
/// §3.2.1.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct CapHeader {
    /// Capability ID (0x0F for IOMMU).
    #[bits(8)]
    pub cap_id: u8,
    /// Next capability pointer.
    #[bits(8)]
    pub cap_ptr: u8,
    /// Capability type (011b for IOMMU).
    #[bits(3)]
    pub cap_type: u8,
    /// Capability revision (00001b).
    #[bits(5)]
    pub cap_rev: u8,
    /// IOTLB support.
    pub iotlb_sup: bool,
    /// HyperTransport tunnel.
    pub ht_tunnel: bool,
    /// NpCache (not-present cache).
    pub np_cache: bool,
    /// Extended Feature Register support.
    pub efr_sup: bool,
    /// Capability extension (MiscInfo1 present).
    pub cap_ext: bool,
    #[bits(3)]
    _reserved: u32,
}

/// AMD IOMMU Base Address Low (capability offset 04h, 32-bit).
///
/// §3.2.2.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct BaseAddrLow {
    /// Enable bit (RW1S — locks all cap regs when set to 1).
    pub enable: bool,
    #[bits(13)]
    _reserved: u32,
    /// Base address bits [31:14]. MMIO base is 16KB-aligned.
    #[bits(18)]
    pub base_addr: u32,
}

/// AMD IOMMU Base Address High (capability offset 08h, 32-bit).
///
/// §3.2.3.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct BaseAddrHigh {
    /// Base address bits [63:32].
    #[bits(32)]
    pub base_addr: u32,
}

/// AMD IOMMU Range Register (capability offset 0Ch, 32-bit).
///
/// §3.2.4.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct Range {
    /// Unit ID (deprecated, set to 0).
    #[bits(5)]
    pub unit_id: u8,
    #[bits(2)]
    _reserved: u32,
    /// Range valid — 1 = FirstDevice/LastDevice/BusNumber fields are valid.
    pub rng_valid: bool,
    /// Bus number for the IOMMU's upstream bus.
    #[bits(8)]
    pub bus_number: u8,
    /// First device (BDF bits [7:0]) managed by this IOMMU.
    #[bits(8)]
    pub first_device: u8,
    /// Last device (BDF bits [7:0]) managed by this IOMMU.
    #[bits(8)]
    pub last_device: u8,
}

/// AMD IOMMU Miscellaneous Information Register 0 (capability offset 10h, 32-bit).
///
/// §3.2.5.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct MiscInfo0 {
    /// MSI message number (interrupt vector index for event log + completion wait).
    #[bits(5)]
    pub msi_num: u8,
    /// Guest VA size (0 = not supported).
    #[bits(3)]
    pub gva_size: u8,
    /// Physical address size in bits.
    #[bits(7)]
    pub pa_size: u8,
    /// Virtual address size in bits.
    #[bits(7)]
    pub va_size: u8,
    /// HT ATS reserved bit.
    pub ht_ats_resv: bool,
    #[bits(4)]
    _reserved: u32,
    /// MSI message number for PPR log.
    #[bits(5)]
    pub msi_num_ppr: u8,
}

/// AMD IOMMU PCI Vendor ID.
pub const PCI_VENDOR_ID: u16 = 0x1022;

/// AMD IOMMU PCI Device ID (family 17h / Zen IOMMU).
pub const PCI_DEVICE_ID: u16 = 0x1451;

/// PCI class code base: System Peripheral.
pub const PCI_CLASS_BASE: u8 = 0x08;

/// PCI class code sub: IOMMU.
pub const PCI_CLASS_SUB: u8 = 0x06;

/// PCI class code programming interface.
pub const PCI_CLASS_PROG_IF: u8 = 0x00;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_offsets() {
        assert_eq!(MmioRegister::DEV_TAB_BASE.0, 0x0000);
        assert_eq!(MmioRegister::CMD_BUF_BASE.0, 0x0008);
        assert_eq!(MmioRegister::EVT_LOG_BASE.0, 0x0010);
        assert_eq!(MmioRegister::IOMMU_CTRL.0, 0x0018);
        assert_eq!(MmioRegister::EXCL_BASE.0, 0x0020);
        assert_eq!(MmioRegister::EXCL_LIMIT.0, 0x0028);
        assert_eq!(MmioRegister::EXT_FEAT.0, 0x0030);
        assert_eq!(MmioRegister::GEN_XT_INT_CTRL.0, 0x0170);
        assert_eq!(MmioRegister::PPR_XT_INT_CTRL.0, 0x0178);
        assert_eq!(MmioRegister::CMD_BUF_HEAD.0, 0x2000);
        assert_eq!(MmioRegister::CMD_BUF_TAIL.0, 0x2008);
        assert_eq!(MmioRegister::EVT_LOG_HEAD.0, 0x2010);
        assert_eq!(MmioRegister::EVT_LOG_TAIL.0, 0x2018);
        assert_eq!(MmioRegister::IOMMU_STATUS.0, 0x2020);
    }

    #[test]
    fn test_dev_tab_base_roundtrip() {
        let reg = DevTabBase::new()
            .with_size(0x1FF)
            .with_base_addr(0x0001_2345_6789);
        assert_eq!(reg.size(), 0x1FF);
        assert_eq!(reg.base_addr(), 0x0001_2345_6789);
    }

    #[test]
    fn test_cmd_buf_base_roundtrip() {
        let reg = CmdBufBase::new()
            .with_length(0x09)
            .with_base_addr(0x000A_BCD0_0000);
        assert_eq!(reg.length(), 0x09);
        assert_eq!(reg.base_addr(), 0x000A_BCD0_0000);
    }

    #[test]
    fn test_evt_log_base_roundtrip() {
        let reg = EvtLogBase::new()
            .with_length(0x08)
            .with_base_addr(0x000F_EDC0_0000);
        assert_eq!(reg.length(), 0x08);
        assert_eq!(reg.base_addr(), 0x000F_EDC0_0000);
    }

    #[test]
    fn test_iommu_ctrl_roundtrip() {
        let ctrl = IommuCtrl::new()
            .with_iommu_en(true)
            .with_evt_log_en(true)
            .with_cmd_buf_en(true)
            .with_com_wait_int_en(true)
            .with_evt_int_en(true)
            .with_coherent(true);
        assert!(ctrl.iommu_en());
        assert!(ctrl.evt_log_en());
        assert!(ctrl.cmd_buf_en());
        assert!(ctrl.com_wait_int_en());
        assert!(ctrl.evt_int_en());
        assert!(ctrl.coherent());
        assert!(!ctrl.ht_tun_en());
    }

    #[test]
    fn test_iommu_ctrl_int_remap_mode() {
        let ctrl = IommuCtrl::new().with_num_int_remap_mode(0b10);
        assert_eq!(ctrl.num_int_remap_mode(), 0b10);
    }

    #[test]
    fn test_cmd_buf_head_roundtrip() {
        let reg = CmdBufHead::new().with_head_ptr(0x100);
        assert_eq!(reg.head_ptr(), 0x100);
    }

    #[test]
    fn test_iommu_status_roundtrip() {
        let status = IommuStatus::new()
            .with_evt_overflow(true)
            .with_com_wait_int(true)
            .with_cmd_buf_run(true);
        assert!(status.evt_overflow());
        assert!(status.com_wait_int());
        assert!(status.cmd_buf_run());
        assert!(!status.evt_log_int());
        assert!(!status.evt_log_run());
    }

    #[test]
    fn test_cap_header_roundtrip() {
        let hdr = CapHeader::new()
            .with_cap_id(CAP_ID)
            .with_cap_type(0b011)
            .with_cap_rev(0b00001)
            .with_efr_sup(true);
        assert_eq!(hdr.cap_id(), CAP_ID);
        assert_eq!(hdr.cap_type(), 0b011);
        assert_eq!(hdr.cap_rev(), 0b00001);
        assert!(hdr.efr_sup());
        assert!(!hdr.iotlb_sup());
    }

    #[test]
    fn test_base_addr_low_roundtrip() {
        let reg = BaseAddrLow::new().with_enable(true).with_base_addr(0x3F400); // 0xFD000000 >> 14
        assert!(reg.enable());
        assert_eq!(reg.base_addr(), 0x3F400);
    }

    #[test]
    fn test_range_roundtrip() {
        let reg = Range::new()
            .with_rng_valid(true)
            .with_bus_number(0)
            .with_first_device(0x00)
            .with_last_device(0xFF);
        assert!(reg.rng_valid());
        assert_eq!(reg.bus_number(), 0);
        assert_eq!(reg.first_device(), 0x00);
        assert_eq!(reg.last_device(), 0xFF);
    }

    #[test]
    fn test_misc_info0_roundtrip() {
        let reg = MiscInfo0::new()
            .with_pa_size(48)
            .with_va_size(48)
            .with_msi_num(0);
        assert_eq!(reg.pa_size(), 48);
        assert_eq!(reg.va_size(), 48);
        assert_eq!(reg.msi_num(), 0);
        assert_eq!(reg.gva_size(), 0);
    }
}
