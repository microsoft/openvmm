// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MMIO register definitions for the Intel VT-d IOMMU.
//!
//! Based on the Intel VT-d Specification Rev 4.1, §10.4 (Register
//! Descriptions). VT-d is a pure MMIO device — no PCI config space.

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;

// =============================================================================
// MMIO Register Offsets (§10.4)
// =============================================================================

open_enum! {
    /// MMIO register offsets for the Intel VT-d IOMMU.
    ///
    /// 64-bit registers have separate `_HI` variants for the upper DWORD
    /// (base + 4), enabling flat match dispatch without nested fallbacks.
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum MmioRegister: u16 {
        /// Version Register (32-bit, RO). §10.4.1.
        VER             = 0x000,
        /// Capability Register — lo DWORD (64-bit, RO). §10.4.2.
        CAP             = 0x008,
        /// Capability Register — hi DWORD.
        CAP_HI          = 0x00C,
        /// Extended Capability Register — lo DWORD (64-bit, RO). §10.4.3.
        ECAP            = 0x010,
        /// Extended Capability Register — hi DWORD.
        ECAP_HI         = 0x014,
        /// Global Command Register (32-bit, WO). §10.4.4.
        GCMD            = 0x018,
        /// Global Status Register (32-bit, RO). §10.4.5.
        GSTS            = 0x01C,
        /// Root Table Address Register — lo DWORD (64-bit, RW). §10.4.6.
        RTADDR          = 0x020,
        /// Root Table Address Register — hi DWORD.
        RTADDR_HI       = 0x024,
        /// Context Command Register — lo DWORD (64-bit, RW). §10.4.7.
        CCMD            = 0x028,
        /// Context Command Register — hi DWORD.
        CCMD_HI         = 0x02C,
        /// Fault Status Register (32-bit, RW1C). §10.4.9.
        FSTS            = 0x034,
        /// Fault Event Control Register (32-bit, RW). §10.4.10.
        FECTL           = 0x038,
        /// Fault Event Data Register (32-bit, RW). §10.4.11.
        FEDATA          = 0x03C,
        /// Fault Event Address Register (32-bit, RW). §10.4.12.
        FEADDR          = 0x040,
        /// Fault Event Upper Address Register (32-bit, RW). §10.4.13.
        FEUADDR         = 0x044,
        /// Invalidation Queue Head Register — lo DWORD (64-bit, RO). §10.4.17.
        IQH             = 0x080,
        /// Invalidation Queue Head Register — hi DWORD.
        IQH_HI          = 0x084,
        /// Invalidation Queue Tail Register — lo DWORD (64-bit, RW). §10.4.18.
        IQT             = 0x088,
        /// Invalidation Queue Tail Register — hi DWORD.
        IQT_HI          = 0x08C,
        /// Invalidation Queue Address Register — lo DWORD (64-bit, RW). §10.4.19.
        IQA             = 0x090,
        /// Invalidation Queue Address Register — hi DWORD.
        IQA_HI          = 0x094,
        /// Invalidation Completion Status Register (32-bit, RW1C). §10.4.20.
        ICS             = 0x09C,
        /// Invalidation Event Control Register (32-bit, RW). §10.4.21.
        IECTL           = 0x0A0,
        /// Invalidation Event Data Register (32-bit, RW). §10.4.22.
        IEDATA          = 0x0A4,
        /// Invalidation Event Address Register (32-bit, RW). §10.4.23.
        IEADDR          = 0x0A8,
        /// Invalidation Event Upper Address Register (32-bit, RW). §10.4.24.
        IEUADDR         = 0x0AC,
        /// Interrupt Remapping Table Address Register — lo DWORD (64-bit, RW). §10.4.29.
        IRTA            = 0x0B8,
        /// Interrupt Remapping Table Address Register — hi DWORD.
        IRTA_HI         = 0x0BC,
        /// Invalidate Address Register — lo DWORD (64-bit, RW). §10.4.15.
        IVA             = 0x100,
        /// Invalidate Address Register — hi DWORD.
        IVA_HI          = 0x104,
        /// IOTLB Invalidate Register — lo DWORD (64-bit, RW). §10.4.16.
        IOTLB           = 0x108,
        /// IOTLB Invalidate Register — hi DWORD.
        IOTLB_HI        = 0x10C,
        /// Fault Recording Register DWORD 0 (FrcdLo lo). §10.4.14.
        FRCD_DW0        = 0x120,
        /// Fault Recording Register DWORD 1 (FrcdLo hi).
        FRCD_DW1        = 0x124,
        /// Fault Recording Register DWORD 2 (FrcdHi lo).
        FRCD_DW2        = 0x128,
        /// Fault Recording Register DWORD 3 (FrcdHi hi, contains F bit).
        FRCD_DW3        = 0x12C,
    }
}

/// MMIO region size in bytes (4KB, one page).
pub const MMIO_REGION_SIZE: u64 = 0x1000;

// =============================================================================
// Register Bitfield Definitions
// =============================================================================

/// Version Register (MMIO offset 0x000, 32-bit, RO). §10.4.1.
///
/// Reports the architecture version implemented by the hardware.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct VersionReg {
    /// Minor version number.
    #[bits(4)]
    pub min: u8,
    /// Major version number.
    #[bits(4)]
    pub max: u8,
    #[bits(24)]
    _reserved: u32,
}

/// Capability Register (MMIO offset 0x008, 64-bit, RO). §10.4.2.
///
/// Reports hardware capabilities to software. Key fields control the
/// supported address widths, large page sizes, fault recording, and
/// domain count.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct CapReg {
    /// Number of domains supported (3 bits). See [`NumDomains`].
    #[bits(3)]
    pub nd: u8,
    /// Advanced Fault Logging (not supported in emulator).
    pub afl: bool,
    /// Required Write-Buffer Flushing — 0 = not required.
    pub rwbf: bool,
    /// Protected Low-Memory Region support.
    pub plmr: bool,
    /// Protected High-Memory Region support.
    pub phmr: bool,
    /// Caching Mode — 0 = non-caching (walk tables on every access).
    pub cm: bool,
    /// Supported Adjusted Guest Address Widths (5 bits).
    /// Bit 0 (reserved), bit 1 = 39-bit (3-level), bit 2 = 48-bit (4-level),
    /// bit 3 = 57-bit (5-level), bit 4 (reserved).
    #[bits(5)]
    pub sagaw: u8,
    #[bits(3)]
    _reserved1: u64,
    /// Maximum Guest Address Width (6 bits) — value is MGAW-1.
    /// 47 = 48-bit address width.
    #[bits(6)]
    pub mgaw: u8,
    /// Zero-Length Read — 1 = supports zero-length read requests.
    pub zlr: bool,
    /// Deprecated (was FLR — Fault Log Register, pre-spec 2.0).
    #[bits(1)]
    _deprecated: u64,
    /// Fault Recording Register Offset (10 bits).
    /// Offset in 16-byte units from register base. FRO=0x12 → offset 0x120.
    #[bits(10)]
    pub fro: u16,
    /// Second-Level Large Page Support (4 bits).
    /// Bit 0 = 2MB (21-bit), bit 1 = 1GB (30-bit).
    #[bits(4)]
    pub sllps: u8,
    #[bits(1)]
    _reserved_38: u64,
    /// Page-Selective Invalidation support.
    pub psi: bool,
    /// Number of Fault Recording registers minus 1 (8 bits).
    /// NFR=0 means 1 fault record.
    #[bits(8)]
    pub nfr: u8,
    /// Maximum Address Mask Value (6 bits) for page-selective invalidation.
    #[bits(6)]
    pub mamv: u8,
    /// DMA Write Draining — 1 = hardware drains write buffers.
    pub dwd: bool,
    /// DMA Read Draining — 1 = hardware drains read requests.
    pub drd: bool,
    /// First-Level 1GB page support (not used in legacy mode).
    pub fl1gp: bool,
    #[bits(2)]
    _reserved2: u64,
    /// Page-walk Incoherency — 1 = page walk results may be incoherent.
    pub pi: bool,
    #[bits(4)]
    _reserved3: u64,
}

open_enum! {
    /// CAP.ND encoding: number of supported domain IDs. §10.4.2, Table 10-2.
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum NumDomains: u8 {
        /// 16 domains (4-bit domain ID).
        ND_16       = 0,
        /// 64 domains (6-bit domain ID).
        ND_64       = 1,
        /// 256 domains (8-bit domain ID).
        ND_256      = 2,
        /// 1024 domains (10-bit domain ID).
        ND_1K       = 3,
        /// 4096 domains (12-bit domain ID).
        ND_4K       = 4,
        /// 16384 domains (14-bit domain ID).
        ND_16K      = 5,
        /// 65536 domains (16-bit domain ID).
        ND_64K      = 6,
    }
}

/// CAP.SLLPS bit: 2MB large page support (21-bit page offset).
pub const SLLPS_2MB: u8 = 1 << 0;
/// CAP.SLLPS bit: 1GB large page support (30-bit page offset).
pub const SLLPS_1GB: u8 = 1 << 1;

/// Extended Capability Register (MMIO offset 0x010, 64-bit, RO). §10.4.3.
///
/// Reports extended capabilities. Key fields: queued invalidation, interrupt
/// remapping, extended interrupt mode (x2APIC).
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct EcapReg {
    /// Page-walk Coherency — 1 = hardware ensures coherency.
    pub c: bool,
    /// Queued Invalidation support.
    pub qi: bool,
    /// Device-TLB support (not implemented).
    pub dt: bool,
    /// Interrupt Remapping support.
    pub ir: bool,
    /// Extended Interrupt Mode — 1 = x2APIC (32-bit destination IDs).
    pub eim: bool,
    #[bits(1)]
    _reserved1: u64,
    /// Pass Through support.
    pub pt: bool,
    /// Snoop Control (not used in emulator).
    pub sc: bool,
    /// IOTLB Register Offset (10 bits).
    /// Offset in 16-byte units from register base. IRO=0x10 → offset 0x100.
    #[bits(10)]
    pub iro: u16,
    #[bits(2)]
    _reserved2: u64,
    /// Maximum Handle Mask Value (4 bits) for interrupt remapping.
    #[bits(4)]
    pub mhmv: u8,
    #[bits(1)]
    _reserved3: u64,
    /// Memory Type Support (not used).
    pub mts: bool,
    /// Nested Translation support (not implemented).
    pub nest: bool,
    #[bits(2)]
    _reserved4: u64,
    /// Page Request support (not implemented).
    pub prs: bool,
    /// Execute Request support (not implemented).
    pub ers: bool,
    /// Supervisor Request support (not implemented).
    pub srs: bool,
    #[bits(1)]
    _reserved5: u64,
    /// No Write Flag support.
    pub nwfs: bool,
    /// Extended Accessed Flag support.
    pub eafs: bool,
    /// Process Address Space ID Size (5 bits, not used).
    #[bits(5)]
    pub pss: u8,
    /// PASID Translation support (not implemented).
    pub pasid: bool,
    /// Device-TLB Invalidation Throttle (not used).
    pub dit: bool,
    /// Page-walk Coherency for Nested Translation (not used).
    pub pds: bool,
    /// Scalable Mode Translation support (not implemented).
    pub smts: bool,
    /// Virtual Command support (not used).
    pub vcs: bool,
    /// Second-stage Accessed/Dirty Support (not used).
    pub ssads: bool,
    /// Second-stage Translation Support (scalable mode, not used).
    pub ssts: bool,
    /// First-stage Translation Support (scalable mode, not used).
    pub flts: bool,
    /// Scalable-Mode Page-walk Coherency Support (not used).
    pub smpwcs: bool,
    /// RID-PASID Support (not used).
    pub rps: bool,
    #[bits(1)]
    _reserved6: u64,
    /// Performance Monitoring Support (not used).
    pub pms: bool,
    /// Abort DMA Mode Support (not used).
    pub adms: bool,
    /// RID_PRIV Support (not used).
    pub rprivs: bool,
    #[bits(4)]
    _reserved7: u64,
    /// Stop Marker Support (not used).
    pub sms: bool,
    #[bits(5)]
    _reserved8: u64,
}

/// Global Command Register (MMIO offset 0x018, 32-bit, WO). §10.4.4.
///
/// Write-only register for controlling IOMMU operation. Reads return 0.
/// Each bit triggers a corresponding action; status is reflected in GSTS.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct GcmdReg {
    #[bits(23)]
    _reserved: u32,
    /// Compatibility Format Interrupt — pass through compatibility-format
    /// MSIs when IR is enabled.
    pub cfi: bool,
    /// Set Interrupt Remapping Table Pointer — latch IRTA register.
    pub sirtp: bool,
    /// Interrupt Remapping Enable.
    pub ire: bool,
    /// Queued Invalidation Enable.
    pub qie: bool,
    /// Write Buffer Flush — flush internal write buffers.
    pub wbf: bool,
    /// Enable Advanced Fault Logging (not supported, treated as no-op).
    pub eafl: bool,
    /// Set Fault Log (not supported, treated as no-op).
    pub sfl: bool,
    /// Set Root Table Pointer — latch RTADDR register.
    pub srtp: bool,
    /// Translation Enable — enable/disable DMA translation.
    pub te: bool,
}

/// Global Status Register (MMIO offset 0x01C, 32-bit, RO). §10.4.5.
///
/// Reports the status corresponding to GCMD operations.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct GstsReg {
    #[bits(23)]
    _reserved: u32,
    /// Compatibility Format Interrupt Status.
    pub cfis: bool,
    /// Interrupt Remapping Table Pointer Status — IRTA has been latched.
    pub irtps: bool,
    /// Interrupt Remapping Enable Status.
    pub ires: bool,
    /// Queued Invalidation Enable Status.
    pub qies: bool,
    /// Write Buffer Flush Status.
    pub wbfs: bool,
    /// Advanced Fault Logging Status.
    pub afls: bool,
    /// Fault Log Status.
    pub fls: bool,
    /// Root Table Pointer Status — RTADDR has been latched.
    pub rtps: bool,
    /// Translation Enable Status.
    pub tes: bool,
}

/// Root Table Address Register (MMIO offset 0x020, 64-bit, RW). §10.4.6.
///
/// Holds the physical address of the root table. The value is consumed when
/// GCMD.SRTP is written. Bits 11:10 select translation type (00=legacy).
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct RtaddrReg {
    #[bits(10)]
    _reserved: u64,
    /// Translation table mode (bits 11:10).
    /// 00 = legacy mode, 01 = scalable mode.
    #[bits(2)]
    pub ttm: u8,
    /// Root table address, bits [63:12]. 4KB-aligned.
    #[bits(52)]
    pub rta: u64,
}

impl RtaddrReg {
    /// Get the full root table physical address (bits 63:12 shifted).
    pub fn root_table_address(&self) -> u64 {
        self.rta() << 12
    }
}

/// Context Command Register (MMIO offset 0x028, 64-bit, RW). §10.4.7.
///
/// Register-based context-cache invalidation (used before QI is enabled).
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct CcmdReg {
    /// Domain ID for domain-selective invalidation.
    #[bits(16)]
    pub did: u16,
    /// Source ID for device-selective invalidation.
    #[bits(16)]
    pub sid: u16,
    /// Function Mask for device-selective invalidation.
    #[bits(2)]
    pub fm: u8,
    #[bits(25)]
    _reserved: u64,
    /// Context Actual Invalidation Granularity (RO, set by HW).
    #[bits(2)]
    pub caig: u8,
    /// Context Invalidation Request Granularity.
    /// 01=global, 10=domain, 11=device.
    #[bits(2)]
    pub cirg: u8,
    /// Invalidate Context-Cache — set by SW, cleared by HW when done.
    pub icc: bool,
}

/// Fault Status Register (MMIO offset 0x034, 32-bit, RW1C). §10.4.9.
///
/// Reports fault recording status. PPF is dynamically computed as OR of all
/// FRCD\[n\].F bits.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct FstsReg {
    /// Primary Fault Overflow — a fault was discarded because all records
    /// were occupied.
    pub pfo: bool,
    /// Primary Fault Pending — dynamically computed as OR of FRCD[n].F.
    /// Not independently stored; always reflects current FRCD state.
    pub ppf: bool,
    /// Advanced Fault Overflow (not used).
    pub afo: bool,
    /// Advanced Fault Pending (not used).
    pub apf: bool,
    /// Invalidation Queue Error — illegal descriptor in the queue.
    pub iqe: bool,
    /// Invalidation Completion Error (not used).
    pub ice: bool,
    /// Invalidation Timeout Error (not used).
    pub ite: bool,
    #[bits(1)]
    _reserved1: u32,
    /// Fault Record Index — index of the first pending fault record.
    #[bits(8)]
    pub fri: u8,
    #[bits(16)]
    _reserved2: u32,
}

/// Fault Event Control Register (MMIO offset 0x038, 32-bit, RW). §10.4.10.
///
/// Controls fault event MSI signaling.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct FectlReg {
    #[bits(30)]
    _reserved: u32,
    /// Interrupt Pending (RO) — set when fault interrupt is pending.
    pub ip: bool,
    /// Interrupt Mask — 1 = mask fault event interrupt.
    pub im: bool,
}

/// Fault Event Data Register (MMIO offset 0x03C, 32-bit, RW). §10.4.11.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct FedataReg {
    /// Interrupt Message Data.
    #[bits(16)]
    pub imd: u16,
    /// Extended Interrupt Message Data (for x2APIC).
    #[bits(16)]
    pub eimd: u16,
}

/// Fault Event Address Register (MMIO offset 0x040, 32-bit, RW). §10.4.12.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct FeaddrReg {
    #[bits(2)]
    _reserved: u32,
    /// Message Address (bits 31:2).
    #[bits(30)]
    pub ma: u32,
}

impl FeaddrReg {
    /// Get the full message address (bits 31:2 shifted).
    pub fn message_address(&self) -> u32 {
        self.ma() << 2
    }
}

/// Invalidation Queue Head Register (MMIO offset 0x080, 64-bit). §10.4.17.
///
/// Read-only from software. Points to the next descriptor to be fetched.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IqhReg {
    #[bits(4)]
    _reserved1: u64,
    /// Queue head offset (bits 18:4), 16-byte aligned.
    #[bits(15)]
    pub qh: u32,
    #[bits(45)]
    _reserved2: u64,
}

impl IqhReg {
    /// Get the byte offset of the head pointer.
    pub fn head_offset(&self) -> u64 {
        (self.qh() as u64) << 4
    }
}

/// Invalidation Queue Tail Register (MMIO offset 0x088, 64-bit, RW). §10.4.18.
///
/// Software writes this to submit new descriptors.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IqtReg {
    #[bits(4)]
    _reserved1: u64,
    /// Queue tail offset (bits 18:4), 16-byte aligned.
    #[bits(15)]
    pub qt: u32,
    #[bits(45)]
    _reserved2: u64,
}

impl IqtReg {
    /// Get the byte offset of the tail pointer.
    pub fn tail_offset(&self) -> u64 {
        (self.qt() as u64) << 4
    }
}

/// Invalidation Queue Address Register (MMIO offset 0x090, 64-bit, RW). §10.4.19.
///
/// Holds the base address and size of the invalidation queue. Only writable
/// when QIE=0.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IqaReg {
    /// Queue size: number of 4KB pages = 2^QS. 0=256 entries, 7=32768 entries.
    #[bits(3)]
    pub qs: u8,
    /// Descriptor width — 0=128-bit, 1=256-bit (not supported in legacy mode).
    pub dw: bool,
    #[bits(8)]
    _reserved: u64,
    /// Queue base address, bits [63:12]. 4KB-aligned.
    #[bits(52)]
    pub iqa: u64,
}

impl IqaReg {
    /// Get the queue base physical address.
    pub fn queue_base_address(&self) -> u64 {
        self.iqa() << 12
    }

    /// Get the queue size in bytes: 2^(QS+8) * 16 bytes.
    pub fn queue_size_bytes(&self) -> u64 {
        (1u64 << (self.qs() as u64 + 8)) * 16
    }
}

/// Invalidation Completion Status Register (MMIO offset 0x09C, 32-bit, RW1C).
/// §10.4.20.
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IcsReg {
    /// Invalidation Wait Descriptor Complete.
    pub iwc: bool,
    #[bits(31)]
    _reserved: u32,
}

/// Invalidation Event Control Register (MMIO offset 0x0A0, 32-bit, RW).
/// §10.4.21.
///
/// Controls invalidation completion event MSI signaling. Separate from the
/// fault event registers (FECTL/FEDATA).
#[bitfield(u32)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IectlReg {
    #[bits(30)]
    _reserved: u32,
    /// Interrupt Pending (RO).
    pub ip: bool,
    /// Interrupt Mask — 1 = mask invalidation completion interrupt.
    pub im: bool,
}

/// Interrupt Remapping Table Address Register (MMIO offset 0x0B8, 64-bit, RW).
/// §10.4.29.
///
/// Holds the base address, size, and mode of the interrupt remapping table.
/// Only writable when IRE=0.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IrtaReg {
    /// Size: number of entries = 2^(S+1). 0=2, 15=65536.
    #[bits(4)]
    pub s: u8,
    #[bits(7)]
    _reserved: u64,
    /// Extended Interrupt Mode Enable — 1 = x2APIC 32-bit destinations.
    pub eime: bool,
    /// IRT base address, bits [63:12]. 4KB-aligned.
    #[bits(52)]
    pub irta: u64,
}

impl IrtaReg {
    /// Get the IRT base physical address.
    pub fn irt_base_address(&self) -> u64 {
        self.irta() << 12
    }

    /// Get the number of entries in the IRT: 2^(S+1).
    pub fn entry_count(&self) -> u32 {
        1u32 << (self.s() as u32 + 1)
    }
}

/// IOTLB Invalidate Register (ECAP.IRO*16 + 0x08, 64-bit, RW). §10.4.16.
///
/// Register-based IOTLB invalidation (used before QI is enabled).
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IotlbReg {
    #[bits(32)]
    _reserved1: u64,
    /// Domain ID for domain-selective invalidation.
    #[bits(16)]
    pub did: u16,
    /// Drain Writes.
    pub dw: bool,
    /// Drain Reads.
    pub dr: bool,
    #[bits(7)]
    _reserved2: u64,
    /// IOTLB Actual Invalidation Granularity (RO, set by HW).
    #[bits(2)]
    pub iaig: u8,
    #[bits(1)]
    _reserved3: u64,
    /// IOTLB Invalidation Request Granularity.
    /// 01=global, 10=domain, 11=page.
    #[bits(2)]
    pub iirg: u8,
    #[bits(1)]
    _reserved4: u64,
    /// Invalidate IOTLB — set by SW, cleared by HW when done.
    pub ivt: bool,
}

/// Fault Recording Register — high 64 bits (CAP.FRO*16 + 8). §10.4.14.
///
/// Contains fault metadata: source ID, fault reason, type, and the fault bit.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct FrcdHi {
    /// Source ID (BDF) of the faulting device.
    #[bits(16)]
    pub sid: u16,
    #[bits(32)]
    _reserved1: u64,
    /// Fault Reason code. See `FaultReason` enum.
    #[bits(8)]
    pub fr: u8,
    #[bits(4)]
    _reserved2: u64,
    /// Address Type (2 bits).
    #[bits(2)]
    pub at: u8,
    /// Type — 0 = write request, 1 = read request (or other non-write).
    pub t: bool,
    /// Fault — 1 = this record contains a valid fault. RW1C (write 1 to clear).
    pub f: bool,
}

/// Fault Recording Register — low 64 bits (CAP.FRO*16). §10.4.14.
///
/// Contains the faulting address (bits 63:12) and reserved/PASID fields.
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct FrcdLo {
    #[bits(12)]
    _reserved1: u64,
    /// Fault Info — the faulting IOVA address, bits [63:12].
    #[bits(52)]
    pub fi: u64,
}

impl FrcdLo {
    /// Get the full faulting address (bits 63:12 shifted).
    pub fn fault_address(&self) -> u64 {
        self.fi() << 12
    }
}

// =============================================================================
// Fault Reason Codes (§7.1.3, §5.1.4.1)
// =============================================================================

open_enum! {
    /// VT-d fault reason codes.
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum FaultReason: u8 {
        // DMA remapping faults (§7.1.3)

        /// Root entry not present (P=0).
        ROOT_NOT_PRESENT            = 0x01,
        /// Context entry not present (P=0).
        CONTEXT_NOT_PRESENT         = 0x02,
        /// Invalid context entry programming (reserved TT value).
        INVALID_CONTEXT_ENTRY       = 0x03,
        /// Address beyond MGAW (IOVA overflow).
        ADDRESS_BEYOND_MGAW         = 0x04,
        /// Write permission fault (W=0 in page table path).
        WRITE_ACCESS_DENIED         = 0x05,
        /// Read permission fault (R=0 in page table path).
        READ_ACCESS_DENIED          = 0x06,
        /// Error accessing second-level paging entry (GPA read failure).
        SL_PTE_ACCESS_ERROR         = 0x07,
        /// Error accessing root entry (GPA read failure).
        ROOT_ENTRY_ACCESS_ERROR     = 0x08,
        /// Error accessing context entry (GPA read failure).
        CONTEXT_ENTRY_ACCESS_ERROR  = 0x09,
        /// Reserved bit set in root entry.
        ROOT_ENTRY_RESERVED_BIT     = 0x0A,
        /// Reserved bit set in context entry.
        CONTEXT_ENTRY_RESERVED_BIT  = 0x0B,
        /// Reserved bit set in second-level paging entry.
        SL_PTE_RESERVED_BIT         = 0x0C,
        /// Context entry blocks translation/translated requests.
        CONTEXT_ENTRY_TT_BLOCK      = 0x0D,
        /// Output address in interrupt address range.
        OUTPUT_ADDR_IN_INTR_RANGE   = 0x0E,

        // Interrupt remapping faults (§5.1.4.1)

        /// Reserved field set in remappable interrupt request.
        IR_RESERVED_FIELD           = 0x20,
        /// Interrupt index exceeds IRT size.
        IR_INDEX_EXCEEDS_SIZE       = 0x21,
        /// IRTE not present (P=0).
        IRTE_NOT_PRESENT            = 0x22,
        /// Error accessing IRTE (GPA read failure).
        IRTE_ACCESS_ERROR           = 0x23,
        /// Reserved field set in present IRTE.
        IRTE_RESERVED_FIELD         = 0x24,
        /// Compatibility-format interrupt blocked.
        COMPAT_FORMAT_BLOCKED       = 0x25,
        /// Source-ID verification failure.
        SOURCE_ID_VERIFICATION_FAIL = 0x26,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gcmd_gsts_bits_aligned() {
        // GCMD and GSTS must have matching bit positions for the status bits.
        let gcmd = GcmdReg::new()
            .with_te(true)
            .with_srtp(true)
            .with_qie(true)
            .with_ire(true)
            .with_sirtp(true)
            .with_cfi(true)
            .with_wbf(true);

        let gsts = GstsReg::new()
            .with_tes(true)
            .with_rtps(true)
            .with_qies(true)
            .with_ires(true)
            .with_irtps(true)
            .with_cfis(true)
            .with_wbfs(true);

        // The bit positions should match between command and status.
        let gcmd_val = u32::from(gcmd);
        let gsts_val = u32::from(gsts);
        assert_eq!(gcmd_val, gsts_val);
    }

    #[test]
    fn test_iqa_queue_size() {
        assert_eq!(IqaReg::new().with_qs(0).queue_size_bytes(), 256 * 16);
        assert_eq!(IqaReg::new().with_qs(7).queue_size_bytes(), 32768 * 16);
    }

    #[test]
    fn test_irta_entry_count() {
        assert_eq!(IrtaReg::new().with_s(0).entry_count(), 2);
        assert_eq!(IrtaReg::new().with_s(15).entry_count(), 65536);
    }

    #[test]
    fn test_iqh_iqt_offsets() {
        assert_eq!(IqhReg::new().with_qh(16).head_offset(), 256);
        assert_eq!(IqtReg::new().with_qt(32).tail_offset(), 512);
    }
}
