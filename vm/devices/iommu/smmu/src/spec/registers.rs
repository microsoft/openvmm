// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 MMIO register definitions.
//!
//! Register offsets and bitfield types from the Arm SMMUv3 architecture
//! specification (IHI 0070), Chapter 6.

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;

// =============================================================================
// MMIO Register Offsets — Page 0 (base + 0x00000)
// =============================================================================

/// SMMU_IDR0: Feature identification register.
pub const IDR0: u16 = 0x0000;
/// SMMU_IDR1: Queue and stream size identification.
pub const IDR1: u16 = 0x0004;
/// SMMU_IDR2: Extended feature identification.
pub const IDR2: u16 = 0x0008;
/// SMMU_IDR3: Extended feature identification.
pub const IDR3: u16 = 0x000C;
/// SMMU_IDR4: Implementation-defined identification.
pub const IDR4: u16 = 0x0010;
/// SMMU_IDR5: Granule and output address size.
pub const IDR5: u16 = 0x0014;
/// SMMU_IIDR: Implementer identification.
pub const IIDR: u16 = 0x0018;
/// SMMU_AIDR: Architecture version identification.
pub const AIDR: u16 = 0x001C;

/// SMMU_CR0: Control register.
pub const CR0: u16 = 0x0020;
/// SMMU_CR0ACK: CR0 acknowledgment (read-only).
pub const CR0ACK: u16 = 0x0024;
/// SMMU_CR1: Queue/table access attributes.
pub const CR1: u16 = 0x0028;
/// SMMU_CR2: Extended controls.
pub const CR2: u16 = 0x002C;

/// SMMU_STATUSR: Status register.
pub const STATUSR: u16 = 0x0040;
/// SMMU_GBPA: Global bypass attributes.
pub const GBPA: u16 = 0x0044;
/// SMMU_AGBPA: Alternate global bypass attributes.
pub const AGBPA: u16 = 0x0048;

/// SMMU_IRQ_CTRL: Interrupt enable register.
pub const IRQ_CTRL: u16 = 0x0050;
/// SMMU_IRQ_CTRLACK: IRQ_CTRL acknowledgment (read-only).
pub const IRQ_CTRLACK: u16 = 0x0054;

/// SMMU_GERROR: Global error status (read-only, toggle protocol).
pub const GERROR: u16 = 0x0060;
/// SMMU_GERRORN: Global error acknowledgment.
pub const GERRORN: u16 = 0x0064;

/// SMMU_GERROR_IRQ_CFG0: GERROR MSI address (64-bit).
pub const GERROR_IRQ_CFG0: u16 = 0x0068;
/// SMMU_GERROR_IRQ_CFG1: GERROR MSI data payload.
pub const GERROR_IRQ_CFG1: u16 = 0x0070;
/// SMMU_GERROR_IRQ_CFG2: GERROR MSI attributes.
pub const GERROR_IRQ_CFG2: u16 = 0x0074;

/// SMMU_STRTAB_BASE: Stream table base address (64-bit).
pub const STRTAB_BASE: u16 = 0x0080;
/// SMMU_STRTAB_BASE_CFG: Stream table configuration.
pub const STRTAB_BASE_CFG: u16 = 0x0088;

/// SMMU_CMDQ_BASE: Command queue base address (64-bit).
pub const CMDQ_BASE: u16 = 0x0090;
/// SMMU_CMDQ_PROD: Command queue producer index.
pub const CMDQ_PROD: u16 = 0x0098;
/// SMMU_CMDQ_CONS: Command queue consumer index.
pub const CMDQ_CONS: u16 = 0x009C;

/// SMMU_EVENTQ_BASE: Event queue base address (64-bit).
pub const EVENTQ_BASE: u16 = 0x00A0;

/// SMMU_EVENTQ_IRQ_CFG0: Event queue MSI address (64-bit).
pub const EVENTQ_IRQ_CFG0: u16 = 0x00B0;
/// SMMU_EVENTQ_IRQ_CFG1: Event queue MSI data.
pub const EVENTQ_IRQ_CFG1: u16 = 0x00B8;
/// SMMU_EVENTQ_IRQ_CFG2: Event queue MSI attributes.
pub const EVENTQ_IRQ_CFG2: u16 = 0x00BC;

// =============================================================================
// MMIO Register Offsets — Page 1 (base + 0x10000)
// =============================================================================

/// SMMU_EVENTQ_PROD: Event queue producer index (page 1).
pub const EVENTQ_PROD_PAGE1: u32 = 0x100A8;
/// SMMU_EVENTQ_CONS: Event queue consumer index (page 1).
pub const EVENTQ_CONS_PAGE1: u32 = 0x100AC;

/// SMMU_CMDQ_IRQ_CFG0: Command queue MSI address (page 1, 64-bit).
pub const CMDQ_IRQ_CFG0_PAGE1: u32 = 0x10008;
/// SMMU_CMDQ_IRQ_CFG1: Command queue MSI data (page 1).
pub const CMDQ_IRQ_CFG1_PAGE1: u32 = 0x10010;
/// SMMU_CMDQ_IRQ_CFG2: Command queue MSI attributes (page 1).
pub const CMDQ_IRQ_CFG2_PAGE1: u32 = 0x10014;

/// Total MMIO region size: page 0 (64KB) + page 1 (64KB) = 128KB.
pub const MMIO_REGION_SIZE: u64 = 0x20000;

// =============================================================================
// Bitfield Types — Identification Registers
// =============================================================================

/// SMMU_IDR0: Feature identification.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct Idr0 {
    /// Stage 2 translation supported.
    pub s2p: bool,
    /// Stage 1 translation supported.
    pub s1p: bool,
    /// Translation table format.
    #[bits(2)]
    pub ttf: u8,
    /// Coherent access supported.
    pub cohacc: bool,
    /// Broadcast TLB maintenance.
    pub btm: bool,
    /// Hardware translation table update.
    #[bits(2)]
    pub httu: u8,
    /// Dormant hint.
    pub dormhint: bool,
    /// Hypervisor stage.
    pub hyp: bool,
    /// ATS supported.
    pub ats: bool,
    /// NS1ATS.
    pub ns1ats: bool,
    /// 16-bit ASID supported.
    pub asid16: bool,
    /// MSI supported.
    pub msi: bool,
    /// SEV supported.
    pub sev: bool,
    /// ATOS supported.
    pub atos: bool,
    /// PRI supported.
    pub pri: bool,
    /// VMID wildcard.
    pub vmw: bool,
    /// 16-bit VMID supported.
    pub vmid16: bool,
    /// 2-level CD table supported.
    pub cd2l: bool,
    /// Virtual ATOS.
    pub vatos: bool,
    /// Translation table endianness.
    #[bits(2)]
    pub ttendian: u8,
    /// ATS recording error.
    pub atsrecerr: bool,
    /// Stall model.
    #[bits(2)]
    pub stall_model: u8,
    /// Terminate model.
    pub term_model: bool,
    /// Stream table level.
    #[bits(2)]
    pub st_level: u8,
    #[bits(1)]
    _reserved: u32,
    /// RME implementation.
    pub rme_impl: bool,
    #[bits(1)]
    _reserved2: u32,
}

/// SMMU_IDR1: Queue and stream size identification.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct Idr1 {
    /// StreamID size (number of bits).
    #[bits(6)]
    pub sidsize: u8,
    /// SubstreamID size (number of bits).
    #[bits(5)]
    pub ssidsize: u8,
    /// Reserved.
    #[bits(5)]
    _reserved0: u32,
    /// Max event queue size as log2(entries).
    #[bits(5)]
    pub eventqs: u8,
    /// Max command queue size as log2(entries).
    #[bits(5)]
    pub cmdqs: u8,
    /// Attribute permissions override.
    pub attr_perms_ovr: bool,
    /// Attribute types override.
    pub attr_types_ovr: bool,
    /// REL (relative base pointers).
    pub rel: bool,
    /// Queues preset.
    pub queues_preset: bool,
    /// Tables preset.
    pub tables_preset: bool,
    /// Enhanced CMDQ.
    pub ecmdq: bool,
}

/// SMMU_IDR5: Granule and output address size.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct Idr5 {
    /// Output address size.
    #[bits(3)]
    pub oas: u8,
    #[bits(1)]
    _reserved0: u32,
    /// 4KB granule supported.
    pub gran4k: bool,
    /// 16KB granule supported.
    pub gran16k: bool,
    /// 64KB granule supported.
    pub gran64k: bool,
    /// Double-size support.
    pub ds: bool,
    /// 128-bit descriptors.
    pub d128: bool,
    #[bits(1)]
    _reserved1: u32,
    /// VA extension (48 or 52 bit).
    #[bits(2)]
    pub vax: u8,
    #[bits(4)]
    _reserved2: u32,
    /// Max stall entries.
    #[bits(16)]
    pub stall_max: u16,
}

// =============================================================================
// Bitfield Types — Control Registers
// =============================================================================

/// SMMU_CR0: Control register.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct Cr0 {
    /// SMMU enable.
    pub smmuen: bool,
    /// PRI queue enable.
    pub priqen: bool,
    /// Event queue enable.
    pub eventqen: bool,
    /// Command queue enable.
    pub cmdqen: bool,
    /// ATS check enable.
    pub atschk: bool,
    #[bits(1)]
    _reserved0: u32,
    /// VMW override.
    #[bits(3)]
    pub vmw: u8,
    #[bits(1)]
    _reserved1: u32,
    /// DPT walk enable.
    pub dpt_walk_en: bool,
    /// VSID enable.
    pub vsiden: bool,
    #[bits(20)]
    _reserved2: u32,
}

/// SMMU_CR1: Queue/table access attributes.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct Cr1 {
    /// Queue inner cacheability.
    #[bits(2)]
    pub queue_ic: u8,
    /// Queue outer cacheability.
    #[bits(2)]
    pub queue_oc: u8,
    /// Queue shareability.
    #[bits(2)]
    pub queue_sh: u8,
    /// Table inner cacheability.
    #[bits(2)]
    pub table_ic: u8,
    /// Table outer cacheability.
    #[bits(2)]
    pub table_oc: u8,
    /// Table shareability.
    #[bits(2)]
    pub table_sh: u8,
    #[bits(20)]
    _reserved: u32,
}

/// SMMU_CR2: Extended controls.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct Cr2 {
    /// Require private translation.
    pub recinvsid: bool,
    /// E2H enable.
    pub e2h: bool,
    /// PTM enable.
    pub ptm: bool,
    #[bits(29)]
    _reserved: u32,
}

/// SMMU_GBPA: Global bypass attributes.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct Gbpa {
    #[bits(1)]
    _reserved0: u32,
    /// Abort all incoming transactions.
    pub abort: bool,
    #[bits(3)]
    _reserved1: u32,
    /// Instruction/data type override.
    #[bits(2)]
    pub instcfg: u8,
    /// Privilege override.
    #[bits(2)]
    pub privcfg: u8,
    #[bits(3)]
    _reserved2: u32,
    /// Shareability configuration.
    #[bits(2)]
    pub shcfg: u8,
    /// Memory type config.
    #[bits(4)]
    pub alloccfg: u8,
    #[bits(13)]
    _reserved3: u32,
    /// Update in progress (cleared by SMMU on completion).
    pub update: bool,
}

/// SMMU_IRQ_CTRL: Interrupt enable control.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct IrqCtrl {
    /// Global error IRQ enable.
    pub gerror_irqen: bool,
    /// PRI queue IRQ enable.
    pub priq_irqen: bool,
    /// Event queue IRQ enable.
    pub eventq_irqen: bool,
    #[bits(29)]
    _reserved: u32,
}

/// SMMU_GERROR / SMMU_GERRORN: Global error status bits.
///
/// An error is active when `GERROR[bit] != GERRORN[bit]`. The SMMU toggles
/// GERROR to signal; software toggles GERRORN to acknowledge.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct Gerror {
    /// Command queue error.
    pub cmdq_err: bool,
    #[bits(1)]
    _reserved0: u32,
    /// Event queue access aborted.
    pub eventq_abt_err: bool,
    /// PRI queue access aborted.
    pub priq_abt_err: bool,
    /// CMD_SYNC MSI aborted.
    pub msi_cmdq_abt_err: bool,
    /// EVTQ MSI aborted.
    pub msi_eventq_abt_err: bool,
    /// PRIQ MSI aborted.
    pub msi_priq_abt_err: bool,
    /// GERROR MSI aborted.
    pub msi_gerror_abt_err: bool,
    /// Service failure mode.
    pub sfm_err: bool,
    #[bits(23)]
    _reserved1: u32,
}

// =============================================================================
// Bitfield Types — Queue Base Registers
// =============================================================================

/// SMMU_STRTAB_BASE: Stream table base address.
#[bitfield(u64)]
#[derive(PartialEq, Eq, Inspect)]
pub struct StrtabBase {
    #[bits(6)]
    _reserved0: u64,
    /// Physical address of the stream table, bits `[55:6]`.
    #[bits(50)]
    pub addr_bits: u64,
    #[bits(6)]
    _reserved1: u64,
    /// Read-allocate hint.
    pub ra: bool,
    #[bits(1)]
    _reserved2: u64,
}

impl StrtabBase {
    /// Returns the physical address of the stream table.
    pub fn addr(&self) -> u64 {
        self.addr_bits() << 6
    }
}

/// SMMU_STRTAB_BASE_CFG: Stream table configuration.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct StrtabBaseCfg {
    /// Table size as log2(entries).
    #[bits(6)]
    pub log2size: u8,
    /// Split point for 2-level tables (ignored for linear).
    #[bits(5)]
    pub split: u8,
    #[bits(5)]
    _reserved: u32,
    /// Stream table format: 0=linear, 1=2-level.
    #[bits(2)]
    pub fmt: u8,
    #[bits(14)]
    _reserved2: u32,
}

open_enum! {
    /// Stream table format values for `StrtabBaseCfg.fmt`.
    pub enum StrtabFmt: u8 {
        /// Linear stream table.
        LINEAR = 0,
        /// 2-level stream table.
        TWO_LEVEL = 1,
    }
}

/// SMMU_CMDQ_BASE / SMMU_EVENTQ_BASE: Queue base address.
#[bitfield(u64)]
#[derive(PartialEq, Eq, Inspect)]
pub struct QueueBase {
    /// Queue size as log2(entries).
    #[bits(5)]
    pub log2size: u8,
    /// Physical address of queue memory, bits `[55:5]`.
    #[bits(51)]
    pub addr_bits: u64,
    #[bits(6)]
    _reserved: u64,
    /// Read/write allocate hint.
    pub ra_wa: bool,
    #[bits(1)]
    _reserved2: u64,
}

impl QueueBase {
    /// Returns the physical address of the queue.
    pub fn addr(&self) -> u64 {
        self.addr_bits() << 5
    }
}

/// SMMU_CMDQ_CONS: Command queue consumer index.
///
/// Has an error field in the upper bits that indicates the reason for a
/// command queue error.
#[bitfield(u32)]
#[derive(PartialEq, Eq, Inspect)]
pub struct CmdqCons {
    /// Read index with wrap bit (bits `[19:0]`).
    #[bits(20)]
    pub rd: u32,
    #[bits(4)]
    _reserved: u32,
    /// Error code (valid when GERROR.CMDQ_ERR is active).
    #[bits(7)]
    pub err: u8,
    #[bits(1)]
    _reserved2: u32,
}

open_enum! {
    /// Command queue error codes for `CmdqCons.err`.
    pub enum CmdqError: u8 {
        /// No error.
        CERROR_NONE = 0,
        /// Illegal command.
        CERROR_ILL = 1,
        /// Command queue abort.
        CERROR_ABT = 2,
        /// ATS error.
        CERROR_ATS_ERR = 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strtab_base_address() {
        // Address must be 64-byte aligned (bottom 6 bits zero)
        let base = StrtabBase::new().with_addr_bits(0x1000_0000_u64 >> 6);
        assert_eq!(base.addr(), 0x1000_0000);

        let base = StrtabBase::new().with_addr_bits(0x0080_0000_0000_u64 >> 6);
        assert_eq!(base.addr(), 0x0080_0000_0000);
    }

    #[test]
    fn test_queue_base_address() {
        let base = QueueBase::new()
            .with_addr_bits(0x2000_0000_u64 >> 5)
            .with_log2size(8);
        assert_eq!(base.addr(), 0x2000_0000);
        assert_eq!(base.log2size(), 8);
    }

    #[test]
    fn test_register_offsets() {
        // Verify offsets match the spec
        assert_eq!(IDR0, 0x0000);
        assert_eq!(IDR1, 0x0004);
        assert_eq!(IDR5, 0x0014);
        assert_eq!(IIDR, 0x0018);
        assert_eq!(AIDR, 0x001C);
        assert_eq!(CR0, 0x0020);
        assert_eq!(CR0ACK, 0x0024);
        assert_eq!(CR1, 0x0028);
        assert_eq!(CR2, 0x002C);
        assert_eq!(GBPA, 0x0044);
        assert_eq!(IRQ_CTRL, 0x0050);
        assert_eq!(IRQ_CTRLACK, 0x0054);
        assert_eq!(GERROR, 0x0060);
        assert_eq!(GERRORN, 0x0064);
        assert_eq!(GERROR_IRQ_CFG0, 0x0068);
        assert_eq!(STRTAB_BASE, 0x0080);
        assert_eq!(STRTAB_BASE_CFG, 0x0088);
        assert_eq!(CMDQ_BASE, 0x0090);
        assert_eq!(CMDQ_PROD, 0x0098);
        assert_eq!(CMDQ_CONS, 0x009C);
        assert_eq!(EVENTQ_BASE, 0x00A0);
        assert_eq!(EVENTQ_IRQ_CFG0, 0x00B0);
        assert_eq!(EVENTQ_PROD_PAGE1, 0x100A8);
        assert_eq!(EVENTQ_CONS_PAGE1, 0x100AC);
    }
}
