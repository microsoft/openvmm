// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel VT-d (Virtualization Technology for Directed I/O) IOMMU emulator.
//!
//! Provides emulated DMA address translation (IOVA → GPA via root/context
//! tables and second-level page table walking) and interrupt remapping for
//! emulated PCI devices.
//!
//! Unlike the AMD IOMMU (which is a PCI device), VT-d is a pure MMIO platform
//! device discovered via the ACPI DMAR table. It has no PCI config space.

#![forbid(unsafe_code)]

pub mod spec;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use guestmem::GuestMemory;
use inspect::InspectMut;
use parking_lot::RwLock;
use pci_core::msi::SignalMsi;
use spec::registers::CapReg;
use spec::registers::CcmdReg;
use spec::registers::EcapReg;
use spec::registers::FectlReg;
use spec::registers::FstsReg;
use spec::registers::GcmdReg;
use spec::registers::GstsReg;
use spec::registers::IcsReg;
use spec::registers::IectlReg;
use spec::registers::IqaReg;
use spec::registers::IqhReg;
use spec::registers::IqtReg;
use spec::registers::IrtaReg;
use spec::registers::RtaddrReg;
use spec::registers::VersionReg;
use std::ops::RangeInclusive;
use std::sync::Arc;

/// MMIO region size (4KB).
pub const MMIO_REGION_SIZE: u64 = spec::registers::MMIO_REGION_SIZE;

// =============================================================================
// Hardcoded Capability Values (1B.3)
// =============================================================================

/// VT-d version 1.0 (major=1, minor=0).
const VER_VALUE: u32 = VersionReg::new().with_max(1).with_min(0).into_bits();

/// Capability Register value.
///
/// - MGAW=47 (48-bit address width)
/// - SAGAW=0x6 (39-bit + 48-bit)
/// - NFR=0 (1 fault record)
/// - SLLPS=0x3 (2MB + 1GB large pages)
/// - FRO=0x12 (fault recording at MMIO 0x120)
/// - DWD=1, DRD=1, CM=0, ND=0x6 (65536 domains)
/// - RWBF=0, PSI=1, ZLR=1, MAMV=0x12
const CAP_VALUE: u64 = CapReg::new()
    .with_nd(6) // 65536 domains
    .with_afl(false)
    .with_rwbf(false)
    .with_cm(false)
    .with_sagaw(0x6) // 39-bit + 48-bit
    .with_mgaw(47) // 48-bit
    .with_zlr(true)
    .with_fro(0x12) // FRO * 16 = 0x120
    .with_sllps(0x3) // 2MB + 1GB
    .with_psi(true)
    .with_nfr(0) // 1 fault record
    .with_mamv(0x12)
    .with_dwd(true)
    .with_drd(true)
    .into_bits();

/// Extended Capability Register value.
///
/// - C=1 (page-walk coherency)
/// - QI=1 (queued invalidation)
/// - IR=1 (interrupt remapping)
/// - EIM=1 (x2APIC)
/// - IRO=0x10 (IOTLB registers at MMIO 0x100)
/// - MHMV=0xF
const ECAP_VALUE: u64 = EcapReg::new()
    .with_c(true)
    .with_qi(true)
    .with_ir(true)
    .with_eim(true)
    .with_iro(0x10) // IRO * 16 = 0x100
    .with_mhmv(0xF)
    .into_bits();

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for constructing an [`IntelVtdDevice`].
#[derive(Debug, Clone)]
pub struct IntelVtdConfig {
    /// MMIO base address for the VT-d register file.
    pub mmio_base: u64,
}

// =============================================================================
// Internal Register State
// =============================================================================

/// Internal mutable state of the VT-d IOMMU.
///
/// All fields are behind `RwLock<VtdState>` in [`VtdSharedState`].
/// DMA translations take a read lock; MMIO writes take a write lock.
#[derive(Debug, Default, inspect::Inspect)]
struct VtdState {
    // -- Global status (mirrors GCMD operations) --
    /// Global Status Register value.
    gsts: u32,

    // -- Root table --
    /// Root Table Address Register (raw value, written freely).
    #[inspect(hex)]
    rtaddr: u64,
    /// Latched root table address (set when SRTP is processed).
    #[inspect(hex)]
    latched_rtaddr: u64,

    // -- Interrupt remapping table --
    /// Interrupt Remapping Table Address Register (raw value).
    #[inspect(hex)]
    irta: u64,
    /// Latched IRTA (set when SIRTP is processed).
    #[inspect(hex)]
    latched_irta: u64,

    // -- Context command register (register-based invalidation) --
    #[inspect(hex)]
    ccmd: u64,

    // -- Fault recording --
    /// Fault Status Register (RW1C bits: PFO, IQE, ICE, ITE).
    fsts: u32,
    /// Fault Event Control Register.
    fectl: u32,
    /// Fault Event Data Register.
    #[inspect(hex)]
    fedata: u32,
    /// Fault Event Address Register.
    #[inspect(hex)]
    feaddr: u32,
    /// Fault Event Upper Address Register.
    #[inspect(hex)]
    feuaddr: u32,
    /// Fault Recording Register — low 64 bits.
    #[inspect(hex)]
    frcd_lo: u64,
    /// Fault Recording Register — high 64 bits.
    #[inspect(hex)]
    frcd_hi: u64,

    // -- Invalidation queue --
    /// Invalidation Queue Address Register (raw value).
    #[inspect(hex)]
    iqa: u64,
    /// Invalidation Queue Head (byte offset, bits 18:4 shifted).
    #[inspect(hex)]
    iqh: u64,
    /// Invalidation Queue Tail (byte offset).
    #[inspect(hex)]
    iqt: u64,
    /// Invalidation Completion Status Register (IWC bit).
    ics: u32,
    /// Invalidation Event Control Register.
    iectl: u32,
    /// Invalidation Event Data Register.
    #[inspect(hex)]
    iedata: u32,
    /// Invalidation Event Address Register.
    #[inspect(hex)]
    ieaddr: u32,
    /// Invalidation Event Upper Address Register.
    #[inspect(hex)]
    ieuaddr: u32,

    // -- IOTLB registers (register-based invalidation, pre-QI) --
    /// Invalidate Address Register (IVA_REG at 0x100).
    #[inspect(hex)]
    iva: u64,
    /// IOTLB Invalidate Register (IOTLB_REG at 0x108).
    #[inspect(hex)]
    iotlb: u64,
}

// =============================================================================
// Shared IOMMU State
// =============================================================================

/// Shared VT-d IOMMU state accessible by per-device wrappers.
///
/// This struct holds the MMIO register state and guest memory reference
/// behind a `RwLock`, allowing concurrent reads from per-device translation
/// wrappers while the `IntelVtdDevice` performs exclusive writes via MMIO.
pub struct VtdSharedState {
    /// Guest memory for reading root/context/page tables and IRT.
    guest_memory: GuestMemory,
    /// MMIO register state, protected by a RwLock.
    state: RwLock<VtdState>,
    /// MSI delivery handle for the IOMMU's own interrupts (fault events
    /// and invalidation completion events). VT-d is a platform device
    /// with no PCI MSI capability — it programs MSI address/data directly
    /// into MMIO registers (FEADDR/FEDATA, IEADDR/IEDATA).
    signal_msi: Arc<dyn SignalMsi>,
}

impl VtdSharedState {
    /// Create new shared state.
    fn new(guest_memory: GuestMemory, signal_msi: Arc<dyn SignalMsi>) -> Self {
        Self {
            guest_memory,
            state: RwLock::new(VtdState::default()),
            signal_msi,
        }
    }

    /// Returns whether translation is currently enabled (GSTS.TES).
    pub fn is_enabled(&self) -> bool {
        let state = self.state.read();
        GstsReg::from(state.gsts).tes()
    }

    /// Returns whether interrupt remapping is currently enabled (GSTS.IRES).
    pub fn is_ir_enabled(&self) -> bool {
        let state = self.state.read();
        GstsReg::from(state.gsts).ires()
    }

    /// Deliver the IOMMU's own fault event MSI.
    ///
    /// VT-d delivers its own MSIs directly (not through its own interrupt
    /// remapping). Uses `signal_msi(None, ...)` — the `None` devid means
    /// if this MSI were to pass through a VtdSignalMsi wrapper it would be
    /// dropped, which is correct (IOMMU MSIs must not loop through IR).
    fn deliver_fault_interrupt(&self, state: &VtdState) {
        let fectl = FectlReg::from(state.fectl);
        if fectl.im() {
            // Masked — don't deliver, IP will be set by caller.
            return;
        }
        let addr = (state.feuaddr as u64) << 32 | (state.feaddr as u64);
        let data = state.fedata;
        self.signal_msi.signal_msi(None, addr, data);
    }

    /// Deliver the IOMMU's own invalidation completion event MSI.
    fn deliver_invalidation_interrupt(&self, state: &VtdState) {
        let iectl = IectlReg::from(state.iectl);
        if iectl.im() {
            return;
        }
        let addr = (state.ieuaddr as u64) << 32 | (state.ieaddr as u64);
        let data = state.iedata;
        self.signal_msi.signal_msi(None, addr, data);
    }
}

// =============================================================================
// IntelVtdDevice
// =============================================================================

/// Intel VT-d IOMMU emulator device.
///
/// A pure MMIO platform device (no PCI config space) discovered via the ACPI
/// DMAR table. Implements the VT-d register file for IOMMU control, DMA
/// translation, interrupt remapping, invalidation queue, and fault recording.
pub struct IntelVtdDevice {
    /// Fixed MMIO base address.
    mmio_base: u64,
    /// Static region descriptor for MmioIntercept.
    mmio_region: (&'static str, RangeInclusive<u64>),
    /// Shared IOMMU state (accessible by per-device wrappers).
    shared: Arc<VtdSharedState>,
}

impl IntelVtdDevice {
    /// Create a new Intel VT-d IOMMU device.
    ///
    /// `guest_memory` is used for reading root/context/page tables and IRT.
    /// `signal_msi` is the partition's MSI delivery handle — used for the
    /// IOMMU's own fault event and invalidation completion interrupts.
    pub fn new(
        guest_memory: GuestMemory,
        config: IntelVtdConfig,
        signal_msi: Arc<dyn SignalMsi>,
    ) -> (Self, Arc<VtdSharedState>) {
        let mmio_base = config.mmio_base;
        let shared = Arc::new(VtdSharedState::new(guest_memory, signal_msi));

        let device = Self {
            mmio_base,
            mmio_region: (
                "intel-vtd-mmio",
                mmio_base..=mmio_base + MMIO_REGION_SIZE - 1,
            ),
            shared: shared.clone(),
        };

        (device, shared)
    }

    /// Returns the shared IOMMU state for creating per-device wrappers.
    pub fn shared_state(&self) -> &Arc<VtdSharedState> {
        &self.shared
    }

    // =========================================================================
    // MMIO Register Read (DWORD granularity)
    // =========================================================================

    /// Read a 32-bit register value at a DWORD-aligned MMIO offset.
    ///
    /// All register reads go through this function. 64-bit reads are composed
    /// from two DWORD reads. This avoids alignment issues with 32-bit registers
    /// at non-8-byte-aligned offsets (e.g. GSTS at 0x01C, FSTS at 0x034).
    fn read_register_dword(&self, offset: u16) -> u32 {
        let state = self.shared.state.read();
        self.read_register_dword_locked(&state, offset)
    }

    /// Read a DWORD register while already holding the state lock.
    fn read_register_dword_locked(&self, state: &VtdState, offset: u16) -> u32 {
        match offset {
            // VER (32-bit at 0x000)
            0x000 => VER_VALUE,
            // CAP (64-bit at 0x008)
            0x008 => CAP_VALUE as u32,
            0x00C => (CAP_VALUE >> 32) as u32,
            // ECAP (64-bit at 0x010)
            0x010 => ECAP_VALUE as u32,
            0x014 => (ECAP_VALUE >> 32) as u32,
            // GCMD (write-only, reads return 0)
            0x018 => 0,
            // GSTS (32-bit at 0x01C)
            0x01C => state.gsts,
            // RTADDR (64-bit at 0x020)
            0x020 => state.rtaddr as u32,
            0x024 => (state.rtaddr >> 32) as u32,
            // CCMD (64-bit at 0x028)
            0x028 => state.ccmd as u32,
            0x02C => (state.ccmd >> 32) as u32,
            // FSTS (32-bit at 0x034)
            0x034 => self.read_fsts(state),
            // FECTL (32-bit at 0x038)
            0x038 => state.fectl,
            // FEDATA (32-bit at 0x03C)
            0x03C => state.fedata,
            // FEADDR (32-bit at 0x040)
            0x040 => state.feaddr,
            // FEUADDR (32-bit at 0x044)
            0x044 => state.feuaddr,
            // IQH (64-bit at 0x080)
            0x080 => state.iqh as u32,
            0x084 => (state.iqh >> 32) as u32,
            // IQT (64-bit at 0x088)
            0x088 => state.iqt as u32,
            0x08C => (state.iqt >> 32) as u32,
            // IQA (64-bit at 0x090)
            0x090 => state.iqa as u32,
            0x094 => (state.iqa >> 32) as u32,
            // ICS (32-bit at 0x09C)
            0x09C => state.ics,
            // IECTL (32-bit at 0x0A0)
            0x0A0 => state.iectl,
            // IEDATA (32-bit at 0x0A4)
            0x0A4 => state.iedata,
            // IEADDR (32-bit at 0x0A8)
            0x0A8 => state.ieaddr,
            // IEUADDR (32-bit at 0x0AC)
            0x0AC => state.ieuaddr,
            // IRTA (64-bit at 0x0B8)
            0x0B8 => state.irta as u32,
            0x0BC => (state.irta >> 32) as u32,
            // IVA (64-bit at IRO*16)
            o if o == spec::registers::IVA_REG_OFFSET => state.iva as u32,
            o if o == spec::registers::IVA_REG_OFFSET + 4 => (state.iva >> 32) as u32,
            // IOTLB (64-bit at IRO*16+8)
            o if o == spec::registers::IOTLB_REG_OFFSET => state.iotlb as u32,
            o if o == spec::registers::IOTLB_REG_OFFSET + 4 => (state.iotlb >> 32) as u32,
            // FRCD low (64-bit at FRO*16)
            o if o == spec::registers::FRCD_LO_OFFSET => state.frcd_lo as u32,
            o if o == spec::registers::FRCD_LO_OFFSET + 4 => (state.frcd_lo >> 32) as u32,
            // FRCD high (64-bit at FRO*16+8)
            o if o == spec::registers::FRCD_HI_OFFSET => state.frcd_hi as u32,
            o if o == spec::registers::FRCD_HI_OFFSET + 4 => (state.frcd_hi >> 32) as u32,
            // Unmapped offsets return 0
            _ => 0,
        }
    }

    /// Read FSTS with PPF dynamically computed as OR of FRCD[n].F bits.
    fn read_fsts(&self, state: &VtdState) -> u32 {
        let mut fsts = FstsReg::from(state.fsts);
        // PPF = OR of all FRCD[n].F bits. With NFR=0 (1 record), this
        // is just bit 127 of FRCD (bit 63 of frcd_hi).
        let frcd_f = (state.frcd_hi >> 63) & 1 != 0;
        fsts.set_ppf(frcd_f);
        fsts.into_bits()
    }

    // =========================================================================
    // MMIO Register Write (DWORD granularity)
    // =========================================================================

    /// Write a 32-bit value at a DWORD-aligned MMIO offset.
    ///
    /// All register writes go through this function. 64-bit writes are split
    /// into two DWORD writes.
    fn write_register_dword(&mut self, offset: u16, value: u32) {
        let mut state = self.shared.state.write();
        self.write_register_dword_locked(&mut state, offset, value);
    }

    /// Write a DWORD register while already holding the state write lock.
    fn write_register_dword_locked(&self, state: &mut VtdState, offset: u16, value: u32) {
        tracing::trace!(offset, value, "vtd mmio_write_dword");

        match offset {
            // Read-only registers: ignore writes.
            0x000 | 0x004 | 0x008 | 0x00C | 0x010 | 0x014 | 0x01C | 0x080 | 0x084 => {}

            // GCMD (32-bit WO at 0x018)
            0x018 => {
                self.process_gcmd(state, value);
            }

            // RTADDR (64-bit at 0x020) — freely writable
            0x020 => {
                state.rtaddr = (state.rtaddr & 0xFFFF_FFFF_0000_0000) | value as u64;
            }
            0x024 => {
                state.rtaddr = (state.rtaddr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }

            // CCMD (64-bit at 0x028) — only process on upper DWORD write
            // (ICC is bit 63, in the upper DWORD)
            0x028 => {
                state.ccmd = (state.ccmd & 0xFFFF_FFFF_0000_0000) | value as u64;
            }
            0x02C => {
                let full = (state.ccmd & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                self.process_ccmd(state, full);
            }

            // FSTS (32-bit RW1C at 0x034)
            0x034 => {
                let write_val = FstsReg::from(value);
                let mut fsts = FstsReg::from(state.fsts);
                if write_val.pfo() {
                    fsts.set_pfo(false);
                }
                if write_val.iqe() {
                    fsts.set_iqe(false);
                }
                if write_val.ice() {
                    fsts.set_ice(false);
                }
                if write_val.ite() {
                    fsts.set_ite(false);
                }
                state.fsts = fsts.into_bits();
            }

            // FECTL (32-bit RW at 0x038, IP is RO)
            0x038 => {
                let new = FectlReg::from(value);
                let old = FectlReg::from(state.fectl);
                state.fectl = FectlReg::new()
                    .with_im(new.im())
                    .with_ip(old.ip())
                    .into_bits();
                if old.im() && !new.im() && old.ip() {
                    state.fectl = FectlReg::from(state.fectl).with_ip(false).into_bits();
                    self.shared.deliver_fault_interrupt(state);
                }
            }

            // FEDATA, FEADDR, FEUADDR (32-bit RW)
            0x03C => state.fedata = value,
            0x040 => state.feaddr = value,
            0x044 => state.feuaddr = value,

            // IQT (64-bit at 0x088) — trigger queue processing on low DWORD
            // (tail bits 18:4 are in the lower 32 bits)
            0x088 => {
                let full = (state.iqt & 0xFFFF_FFFF_0000_0000) | value as u64;
                let iqt = IqtReg::from(full);
                state.iqt = IqtReg::new().with_qt(iqt.qt()).into_bits();
                self.process_invalidation_queue(state);
            }
            0x08C => {
                state.iqt = (state.iqt & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }

            // IQA (64-bit at 0x090) — only writable when QIE=0
            0x090 => {
                if !GstsReg::from(state.gsts).qies() {
                    state.iqa = (state.iqa & 0xFFFF_FFFF_0000_0000) | value as u64;
                } else {
                    tracelimit::warn_ratelimited!("vtd: write to IQA while QIE=1, ignored");
                }
            }
            0x094 => {
                if !GstsReg::from(state.gsts).qies() {
                    state.iqa = (state.iqa & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                } else {
                    tracelimit::warn_ratelimited!("vtd: write to IQA while QIE=1, ignored");
                }
            }

            // ICS (32-bit RW1C at 0x09C)
            0x09C => {
                let write_val = IcsReg::from(value);
                let mut ics = IcsReg::from(state.ics);
                if write_val.iwc() {
                    ics.set_iwc(false);
                }
                state.ics = ics.into_bits();
            }

            // IECTL (32-bit RW at 0x0A0, IP is RO)
            0x0A0 => {
                let new = IectlReg::from(value);
                let old = IectlReg::from(state.iectl);
                state.iectl = IectlReg::new()
                    .with_im(new.im())
                    .with_ip(old.ip())
                    .into_bits();
                if old.im() && !new.im() && old.ip() {
                    state.iectl = IectlReg::from(state.iectl).with_ip(false).into_bits();
                    self.shared.deliver_invalidation_interrupt(state);
                }
            }

            // IEDATA, IEADDR, IEUADDR (32-bit RW)
            0x0A4 => state.iedata = value,
            0x0A8 => state.ieaddr = value,
            0x0AC => state.ieuaddr = value,

            // IRTA (64-bit at 0x0B8) — only writable when IRE=0
            0x0B8 => {
                if !GstsReg::from(state.gsts).ires() {
                    state.irta = (state.irta & 0xFFFF_FFFF_0000_0000) | value as u64;
                } else {
                    tracelimit::warn_ratelimited!("vtd: write to IRTA while IRE=1, ignored");
                }
            }
            0x0BC => {
                if !GstsReg::from(state.gsts).ires() {
                    state.irta = (state.irta & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                } else {
                    tracelimit::warn_ratelimited!("vtd: write to IRTA while IRE=1, ignored");
                }
            }

            _ => {
                // Non-standard offsets (IOTLB, fault recording).
                let iva_lo = spec::registers::IVA_REG_OFFSET;
                let iva_hi = spec::registers::IVA_REG_OFFSET + 4;
                let iotlb_lo = spec::registers::IOTLB_REG_OFFSET;
                let iotlb_hi = spec::registers::IOTLB_REG_OFFSET + 4;
                let frcd_lo_lo = spec::registers::FRCD_LO_OFFSET;
                let frcd_lo_hi = spec::registers::FRCD_LO_OFFSET + 4;
                let frcd_hi_lo = spec::registers::FRCD_HI_OFFSET;
                let frcd_hi_hi = spec::registers::FRCD_HI_OFFSET + 4;

                match offset {
                    o if o == iva_lo => {
                        state.iva = (state.iva & 0xFFFF_FFFF_0000_0000) | value as u64;
                    }
                    o if o == iva_hi => {
                        state.iva = (state.iva & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                    }
                    o if o == iotlb_lo => {
                        // Store low half; IVT is in the high DWORD.
                        state.iotlb = (state.iotlb & 0xFFFF_FFFF_0000_0000) | value as u64;
                    }
                    o if o == iotlb_hi => {
                        // IVT (bit 63) is in this DWORD. Merge and process.
                        let full = (state.iotlb & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                        self.process_iotlb_reg(state, full);
                    }
                    o if o == frcd_lo_lo || o == frcd_lo_hi => {
                        // FRCD low is RO.
                    }
                    o if o == frcd_hi_lo => {
                        // Lower DWORD of FRCD_HI — RO fields.
                    }
                    o if o == frcd_hi_hi => {
                        // F bit (bit 31 of this DWORD = bit 63 of FRCD_HI) is RW1C.
                        if (value >> 31) & 1 != 0 {
                            state.frcd_hi &= !(1u64 << 63);
                        }
                    }
                    _ => {
                        // Unmapped offsets: silently ignored.
                    }
                }
            }
        }
    }

    // =========================================================================
    // GCMD Processing (1B.2)
    // =========================================================================

    /// Process a write to the Global Command Register (GCMD).
    ///
    /// GCMD is write-only. Each bit triggers an action; status is reflected
    /// in GSTS. Toggle bits (TE, QIE, IRE, CFI) compare against current GSTS.
    /// One-shot bits (SRTP, SIRTP, WBF) fire if set.
    fn process_gcmd(&self, state: &mut VtdState, value: u32) {
        let gcmd = GcmdReg::from(value);
        let mut gsts = GstsReg::from(state.gsts);

        // -- One-shot: Set Root Table Pointer (SRTP) --
        if gcmd.srtp() {
            state.latched_rtaddr = state.rtaddr;
            gsts.set_rtps(true);
        }

        // -- One-shot: Set Interrupt Remapping Table Pointer (SIRTP) --
        if gcmd.sirtp() {
            state.latched_irta = state.irta;
            gsts.set_irtps(true);
        }

        // -- One-shot: Write Buffer Flush (WBF) --
        if gcmd.wbf() {
            // No write buffer in emulator — set status immediately.
            gsts.set_wbfs(true);
        }

        // -- One-shot: Set Fault Log / Enable Advanced Fault Logging --
        // AFL=0 in CAP, so these are no-ops, but set status for compatibility.
        if gcmd.sfl() {
            gsts.set_fls(true);
        }
        if gcmd.eafl() {
            gsts.set_afls(true);
        }

        // -- Toggle: Queued Invalidation Enable (QIE) --
        if gcmd.qie() != gsts.qies() {
            if gcmd.qie() {
                // Enable QI.
                gsts.set_qies(true);
                // Reset head on enable.
                state.iqh = 0;
            } else {
                // Disable QI — reject if TE or IRE is still enabled.
                if gsts.tes() || gsts.ires() {
                    tracelimit::warn_ratelimited!(
                        "vtd: cannot disable QIE while TE or IRE is enabled"
                    );
                } else {
                    gsts.set_qies(false);
                }
            }
        }

        // -- Toggle: Translation Enable (TE) --
        if gcmd.te() != gsts.tes() {
            if gcmd.te() {
                // Enable TE — reject if RTPS=0.
                if !gsts.rtps() {
                    tracelimit::warn_ratelimited!(
                        "vtd: cannot enable TE without root table pointer set (RTPS=0)"
                    );
                } else {
                    gsts.set_tes(true);
                }
            } else {
                gsts.set_tes(false);
            }
        }

        // -- Toggle: Interrupt Remapping Enable (IRE) --
        if gcmd.ire() != gsts.ires() {
            if gcmd.ire() {
                // Enable IRE — reject if IRTPS=0.
                if !gsts.irtps() {
                    tracelimit::warn_ratelimited!(
                        "vtd: cannot enable IRE without IRT pointer set (IRTPS=0)"
                    );
                } else {
                    gsts.set_ires(true);
                }
            } else {
                gsts.set_ires(false);
            }
        }

        // -- Toggle: Compatibility Format Interrupt (CFI) --
        if gcmd.cfi() != gsts.cfis() {
            gsts.set_cfis(gcmd.cfi());
        }

        state.gsts = gsts.into_bits();
    }

    // =========================================================================
    // Register-based invalidation (pre-QI)
    // =========================================================================

    /// Process a write to the Context Command Register (CCMD).
    ///
    /// Register-based context-cache invalidation. Linux writes this during
    /// early init before QI is enabled. No-op since we don't cache.
    fn process_ccmd(&self, state: &mut VtdState, value: u64) {
        let ccmd = CcmdReg::from(value);
        if ccmd.icc() {
            // Clear ICC, set CAIG = CIRG (echo back granularity).
            let result = CcmdReg::from(value).with_icc(false).with_caig(ccmd.cirg());
            state.ccmd = result.into_bits();
        } else {
            state.ccmd = value;
        }
    }

    /// Process a write to the IOTLB Invalidate Register (0x108).
    ///
    /// Register-based IOTLB invalidation. No-op since we don't cache.
    fn process_iotlb_reg(&self, state: &mut VtdState, value: u64) {
        // IVT is bit 63. If set, clear it and echo IAIG = IIRG.
        if (value >> 63) & 1 != 0 {
            // IIRG is bits 61:60, IAIG is bits 58:57.
            let iirg = (value >> 60) & 0x3;
            let result = (value & !(1u64 << 63)) // Clear IVT
                & !(0x3u64 << 57)                 // Clear IAIG
                | (iirg << 57); // Set IAIG = IIRG
            state.iotlb = result;
        } else {
            state.iotlb = value;
        }
    }

    // =========================================================================
    // Invalidation Queue Processing (1C)
    // =========================================================================

    /// Process the invalidation queue.
    ///
    /// Consumes descriptors from head to tail. Called when the guest writes
    /// IQT.
    fn process_invalidation_queue(&self, state: &mut VtdState) {
        let gsts = GstsReg::from(state.gsts);
        if !gsts.qies() {
            return;
        }

        // Check for IQE — don't process if error is outstanding.
        let fsts = FstsReg::from(state.fsts);
        if fsts.iqe() {
            return;
        }

        let iqa = IqaReg::from(state.iqa);

        // Validate DW=0 (128-bit descriptors only).
        if iqa.dw() {
            tracelimit::warn_ratelimited!("vtd: IQA.DW=1 (256-bit descriptors) not supported");
            let mut fsts = FstsReg::from(state.fsts);
            fsts.set_iqe(true);
            state.fsts = fsts.into_bits();
            return;
        }

        let queue_base = iqa.queue_base_address();
        let queue_size = iqa.queue_size_bytes();
        let head = IqhReg::from(state.iqh).head_offset();
        let tail = IqtReg::from(state.iqt).tail_offset();

        let mut current_head = head;

        while current_head != tail {
            let entry_addr = queue_base + current_head;

            // Read 16-byte descriptor from guest memory.
            let descriptor: [u8; 16] = match self.shared.guest_memory.read_plain(entry_addr) {
                Ok(d) => d,
                Err(e) => {
                    tracelimit::warn_ratelimited!(
                        error = &e as &dyn std::error::Error,
                        addr = entry_addr,
                        "vtd: failed to read invalidation queue descriptor"
                    );
                    let mut fsts = FstsReg::from(state.fsts);
                    fsts.set_iqe(true);
                    state.fsts = fsts.into_bits();
                    break;
                }
            };

            let desc_type = descriptor[0] & 0x0F;

            match desc_type {
                // CONTEXT_CACHE_INVALIDATE (0x01) — no-op
                0x01 => {}
                // IOTLB_INVALIDATE (0x02) — no-op
                0x02 => {}
                // DEVICE_TLB_INVALIDATE (0x03) — not supported
                0x03 => {
                    tracelimit::warn_ratelimited!(
                        "vtd: unsupported DEVICE_TLB_INVALIDATE descriptor"
                    );
                }
                // INTERRUPT_ENTRY_CACHE_INVALIDATE (0x04) — no-op
                0x04 => {}
                // INVALIDATION_WAIT (0x05)
                0x05 => {
                    self.process_invalidation_wait(state, &descriptor);
                }
                // Unknown type — set IQE, halt.
                _ => {
                    tracelimit::warn_ratelimited!(
                        desc_type,
                        "vtd: unknown invalidation descriptor type"
                    );
                    let mut fsts = FstsReg::from(state.fsts);
                    fsts.set_iqe(true);
                    state.fsts = fsts.into_bits();
                    break;
                }
            }

            // Advance head with wrap-around.
            current_head = (current_head + 16) % queue_size;
        }

        // Update head register.
        state.iqh = IqhReg::new()
            .with_qh((current_head >> 4) as u32)
            .into_bits();
    }

    /// Process an INVALIDATION_WAIT descriptor (type 0x05).
    fn process_invalidation_wait(&self, state: &mut VtdState, descriptor: &[u8; 16]) {
        let dw0 = u32::from_le_bytes([descriptor[0], descriptor[1], descriptor[2], descriptor[3]]);

        let sw = (dw0 >> 5) & 1 != 0; // Status Write
        let _fn_bit = (dw0 >> 6) & 1 != 0; // Fence (no-op for us)
        let if_bit = (dw0 >> 7) & 1 != 0; // Interrupt Flag

        if sw {
            // Write status_data (bits 63:32 of qw0) to status_address
            // (bits 127:66 of qw1, shifted left by 2).
            let status_data =
                u32::from_le_bytes([descriptor[4], descriptor[5], descriptor[6], descriptor[7]]);

            let qw1 = u64::from_le_bytes([
                descriptor[8],
                descriptor[9],
                descriptor[10],
                descriptor[11],
                descriptor[12],
                descriptor[13],
                descriptor[14],
                descriptor[15],
            ]);
            let status_address = (qw1 >> 2) & !0x3; // Align to 4 bytes

            if let Err(e) = self
                .shared
                .guest_memory
                .write_at(status_address, &status_data.to_le_bytes())
            {
                tracelimit::warn_ratelimited!(
                    error = &e as &dyn std::error::Error,
                    addr = status_address,
                    "vtd: failed to write invalidation wait status"
                );
            }
        }

        if if_bit {
            // Set IWC in ICS.
            let mut ics = IcsReg::from(state.ics);
            ics.set_iwc(true);
            state.ics = ics.into_bits();

            // Signal invalidation completion interrupt.
            let iectl = IectlReg::from(state.iectl);
            if !iectl.im() {
                self.shared.deliver_invalidation_interrupt(state);
            } else {
                // Masked — set IP.
                state.iectl = IectlReg::from(state.iectl).with_ip(true).into_bits();
            }
        }
    }
}

// =============================================================================
// ChipsetDevice trait implementation
// =============================================================================

impl ChipsetDevice for IntelVtdDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

// =============================================================================
// MMIO Register Access
// =============================================================================

impl MmioIntercept for IntelVtdDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        let offset = addr - self.mmio_base;

        // VT-d supports 4-byte and 8-byte naturally aligned accesses.
        match data.len() {
            8 => {
                // Acquire the read lock once for both DWORD reads to avoid
                // a torn read if a writer intervenes between the two halves.
                let state = self.shared.state.read();
                let lo = self.read_register_dword_locked(&state, offset as u16);
                let hi = self.read_register_dword_locked(&state, (offset + 4) as u16);
                let val = lo as u64 | ((hi as u64) << 32);
                data.copy_from_slice(&val.to_le_bytes());
            }
            4 => {
                let val = self.read_register_dword(offset as u16);
                data.copy_from_slice(&val.to_le_bytes());
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    addr,
                    len = data.len(),
                    "vtd: unsupported MMIO read size"
                );
                data.fill(0xff);
                return IoResult::Err(IoError::InvalidAccessSize);
            }
        }

        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        let offset = addr - self.mmio_base;

        match data.len() {
            8 => {
                let val = u64::from_le_bytes(data.try_into().unwrap());
                // Acquire lock once for both DWORD writes.
                let mut state = self.shared.state.write();
                self.write_register_dword_locked(&mut state, offset as u16, val as u32);
                self.write_register_dword_locked(
                    &mut state,
                    (offset + 4) as u16,
                    (val >> 32) as u32,
                );
            }
            4 => {
                let val = u32::from_le_bytes(data.try_into().unwrap());
                self.write_register_dword(offset as u16, val);
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    addr,
                    len = data.len(),
                    "vtd: unsupported MMIO write size"
                );
                return IoResult::Err(IoError::InvalidAccessSize);
            }
        }

        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        std::slice::from_ref(&self.mmio_region)
    }
}

// =============================================================================
// ChangeDeviceState
// =============================================================================

impl vmcore::device_state::ChangeDeviceState for IntelVtdDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let mut state = self.shared.state.write();
        *state = VtdState::default();
    }
}

// =============================================================================
// SaveRestore (stub)
// =============================================================================

impl vmcore::save_restore::SaveRestore for IntelVtdDevice {
    type SavedState = vmcore::save_restore::SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
        Err(vmcore::save_restore::SaveError::NotSupported)
    }

    fn restore(
        &mut self,
        state: Self::SavedState,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        match state {}
    }
}

// =============================================================================
// InspectMut
// =============================================================================

impl InspectMut for IntelVtdDevice {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        let state = self.shared.state.read();
        let gsts = GstsReg::from(state.gsts);
        req.respond()
            .hex("mmio_base", self.mmio_base)
            .field("translation_enabled", gsts.tes())
            .field("ir_enabled", gsts.ires())
            .field("qi_enabled", gsts.qies())
            .hex(
                "root_table_addr",
                RtaddrReg::from(state.latched_rtaddr).root_table_address(),
            )
            .hex(
                "irt_addr",
                IrtaReg::from(state.latched_irta).irt_base_address(),
            )
            .field("state", &*state);
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use guestmem::GuestMemory;

    const TEST_MMIO_BASE: u64 = 0xFED9_0000;

    struct TestSignalMsi;
    impl SignalMsi for TestSignalMsi {
        fn signal_msi(&self, _devid: Option<u32>, _address: u64, _data: u32) {}
    }

    fn create_test_device() -> IntelVtdDevice {
        let gm = GuestMemory::empty();
        let signal_msi = Arc::new(TestSignalMsi);
        let (device, _shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );
        device
    }

    /// Helper to read a 32-bit register.
    fn read32(dev: &mut IntelVtdDevice, reg_offset: u16) -> u32 {
        let mut data = [0u8; 4];
        let result = dev.mmio_read(TEST_MMIO_BASE + reg_offset as u64, &mut data);
        assert!(matches!(result, IoResult::Ok));
        u32::from_le_bytes(data)
    }

    /// Helper to write a 32-bit register.
    fn write32(dev: &mut IntelVtdDevice, reg_offset: u16, value: u32) {
        let data = value.to_le_bytes();
        let result = dev.mmio_write(TEST_MMIO_BASE + reg_offset as u64, &data);
        assert!(matches!(result, IoResult::Ok));
    }

    /// Helper to read a 64-bit register.
    fn read64(dev: &mut IntelVtdDevice, reg_offset: u16) -> u64 {
        let mut data = [0u8; 8];
        let result = dev.mmio_read(TEST_MMIO_BASE + reg_offset as u64, &mut data);
        assert!(matches!(result, IoResult::Ok));
        u64::from_le_bytes(data)
    }

    /// Helper to write a 64-bit register.
    fn write64(dev: &mut IntelVtdDevice, reg_offset: u16, value: u64) {
        let data = value.to_le_bytes();
        let result = dev.mmio_write(TEST_MMIO_BASE + reg_offset as u64, &data);
        assert!(matches!(result, IoResult::Ok));
    }

    #[test]
    fn test_ver_register() {
        let mut dev = create_test_device();
        let ver = read32(&mut dev, 0x000);
        let ver_reg = VersionReg::from(ver);
        assert_eq!(ver_reg.max(), 1);
        assert_eq!(ver_reg.min(), 0);
    }

    #[test]
    fn test_cap_register() {
        let mut dev = create_test_device();
        let cap = read64(&mut dev, 0x008);
        let cap_reg = CapReg::from(cap);
        assert_eq!(cap_reg.mgaw(), 47); // 48-bit
        assert_eq!(cap_reg.sagaw(), 0x6); // 39-bit + 48-bit
        assert_eq!(cap_reg.nfr(), 0); // 1 fault record
        assert_eq!(cap_reg.fro(), 0x12); // 0x120
        assert_eq!(cap_reg.sllps(), 0x3); // 2MB + 1GB
        assert!(cap_reg.dwd());
        assert!(cap_reg.drd());
        assert!(!cap_reg.cm());
        assert_eq!(cap_reg.nd(), 6);
    }

    #[test]
    fn test_ecap_register() {
        let mut dev = create_test_device();
        let ecap = read64(&mut dev, 0x010);
        let ecap_reg = EcapReg::from(ecap);
        assert!(ecap_reg.c());
        assert!(ecap_reg.qi());
        assert!(ecap_reg.ir());
        assert!(ecap_reg.eim());
        assert_eq!(ecap_reg.iro(), 0x10); // 0x100
        assert_eq!(ecap_reg.mhmv(), 0xF);
    }

    #[test]
    fn test_gcmd_read_returns_zero() {
        let mut dev = create_test_device();
        assert_eq!(read32(&mut dev, 0x018), 0);
    }

    #[test]
    fn test_gsts_initial() {
        let mut dev = create_test_device();
        let gsts = read32(&mut dev, 0x01C);
        assert_eq!(gsts, 0); // All disabled initially
    }

    #[test]
    fn test_srtp_and_te_enable() {
        let mut dev = create_test_device();

        // Write RTADDR.
        let root_table_addr = 0x1000_0000u64;
        write64(&mut dev, 0x020, root_table_addr);
        assert_eq!(read64(&mut dev, 0x020), root_table_addr);

        // GCMD: SRTP (bit 30).
        write32(&mut dev, 0x018, GcmdReg::new().with_srtp(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.rtps());

        // GCMD: TE (bit 31).
        write32(&mut dev, 0x018, GcmdReg::new().with_te(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.tes());
    }

    #[test]
    fn test_te_rejected_without_rtps() {
        let mut dev = create_test_device();

        // Try to enable TE without setting root table pointer.
        write32(&mut dev, 0x018, GcmdReg::new().with_te(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(!gsts.tes()); // Should be rejected
    }

    #[test]
    fn test_ire_rejected_without_irtps() {
        let mut dev = create_test_device();

        // Try to enable IRE without setting IRT pointer.
        write32(&mut dev, 0x018, GcmdReg::new().with_ire(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(!gsts.ires()); // Should be rejected
    }

    #[test]
    fn test_sirtp_and_ire_enable() {
        let mut dev = create_test_device();

        // Write IRTA.
        let irt_addr = 0x2000_0000u64;
        write64(&mut dev, 0x0B8, irt_addr);
        assert_eq!(read64(&mut dev, 0x0B8), irt_addr);

        // GCMD: SIRTP.
        write32(&mut dev, 0x018, GcmdReg::new().with_sirtp(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.irtps());

        // GCMD: IRE.
        write32(&mut dev, 0x018, GcmdReg::new().with_ire(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.ires());
    }

    #[test]
    fn test_wbf() {
        let mut dev = create_test_device();

        // GCMD: WBF (bit 27).
        write32(&mut dev, 0x018, GcmdReg::new().with_wbf(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.wbfs());
    }

    #[test]
    fn test_qie_enable_disable() {
        let mut dev = create_test_device();

        // Write IQA.
        write64(&mut dev, 0x090, 0x3000_0000u64);

        // Enable QIE.
        write32(&mut dev, 0x018, GcmdReg::new().with_qie(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.qies());

        // Disable QIE (no TE or IRE active).
        write32(&mut dev, 0x018, GcmdReg::new().with_qie(false).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(!gsts.qies());
    }

    #[test]
    fn test_ccmd_register_invalidation() {
        let mut dev = create_test_device();

        // Write CCMD with ICC=1, CIRG=01 (global).
        let ccmd = CcmdReg::new().with_icc(true).with_cirg(1);
        write64(&mut dev, 0x028, ccmd.into_bits());

        // Read back — ICC should be cleared, CAIG should equal CIRG.
        let result = CcmdReg::from(read64(&mut dev, 0x028));
        assert!(!result.icc());
        assert_eq!(result.caig(), 1);
    }

    #[test]
    fn test_fsts_rw1c() {
        let mut dev = create_test_device();

        // Manually set fault status bits via shared state.
        {
            let mut state = dev.shared.state.write();
            state.fsts = FstsReg::new().with_pfo(true).with_iqe(true).into_bits();
        }

        let fsts = FstsReg::from(read32(&mut dev, 0x034));
        assert!(fsts.pfo());
        assert!(fsts.iqe());

        // Clear PFO by writing 1.
        write32(&mut dev, 0x034, FstsReg::new().with_pfo(true).into_bits());
        let fsts = FstsReg::from(read32(&mut dev, 0x034));
        assert!(!fsts.pfo());
        assert!(fsts.iqe()); // IQE should still be set
    }

    #[test]
    fn test_ppf_dynamic_computation() {
        let mut dev = create_test_device();

        // Initially PPF should be 0.
        let fsts = FstsReg::from(read32(&mut dev, 0x034));
        assert!(!fsts.ppf());

        // Set FRCD[0].F (bit 63 of frcd_hi).
        {
            let mut state = dev.shared.state.write();
            state.frcd_hi |= 1u64 << 63;
        }

        // Now PPF should be 1.
        let fsts = FstsReg::from(read32(&mut dev, 0x034));
        assert!(fsts.ppf());

        // Clear F bit via RW1C on FRCD_HI.
        write64(&mut dev, spec::registers::FRCD_HI_OFFSET, 1u64 << 63);

        // PPF should be 0 again.
        let fsts = FstsReg::from(read32(&mut dev, 0x034));
        assert!(!fsts.ppf());
    }

    #[test]
    fn test_ics_rw1c() {
        let mut dev = create_test_device();

        // Set IWC.
        {
            let mut state = dev.shared.state.write();
            state.ics = IcsReg::new().with_iwc(true).into_bits();
        }

        let ics = IcsReg::from(read32(&mut dev, 0x09C));
        assert!(ics.iwc());

        // Clear IWC.
        write32(&mut dev, 0x09C, IcsReg::new().with_iwc(true).into_bits());
        let ics = IcsReg::from(read32(&mut dev, 0x09C));
        assert!(!ics.iwc());
    }

    #[test]
    fn test_iqa_write_guard() {
        let mut dev = create_test_device();

        // Write IQA before QIE is enabled — should succeed.
        write64(&mut dev, 0x090, 0x5000_0000u64);
        assert_eq!(read64(&mut dev, 0x090), 0x5000_0000u64);

        // Enable QIE.
        write32(&mut dev, 0x018, GcmdReg::new().with_qie(true).into_bits());

        // Write IQA while QIE=1 — should be ignored.
        write64(&mut dev, 0x090, 0x6000_0000u64);
        assert_eq!(read64(&mut dev, 0x090), 0x5000_0000u64);
    }

    #[test]
    fn test_irta_write_guard() {
        let mut dev = create_test_device();

        // Write IRTA before IRE is enabled — should succeed.
        write64(&mut dev, 0x0B8, 0x7000_0000u64);
        assert_eq!(read64(&mut dev, 0x0B8), 0x7000_0000u64);

        // Enable IRE (need SIRTP first).
        write32(&mut dev, 0x018, GcmdReg::new().with_sirtp(true).into_bits());
        write32(&mut dev, 0x018, GcmdReg::new().with_ire(true).into_bits());

        // Write IRTA while IRE=1 — should be ignored.
        write64(&mut dev, 0x0B8, 0x8000_0000u64);
        assert_eq!(read64(&mut dev, 0x0B8), 0x7000_0000u64);
    }

    #[test]
    fn test_unmapped_offset_returns_zero() {
        let mut dev = create_test_device();
        // Read an offset that doesn't correspond to any register.
        assert_eq!(read32(&mut dev, 0x050), 0);
        assert_eq!(read64(&mut dev, 0x060), 0);
    }

    #[test]
    fn test_unsupported_access_size() {
        let mut dev = create_test_device();
        let mut data = [0u8; 2];
        let result = dev.mmio_read(TEST_MMIO_BASE, &mut data);
        assert!(matches!(result, IoResult::Err(IoError::InvalidAccessSize)));
    }

    #[test]
    fn test_32bit_access_to_64bit_register() {
        let mut dev = create_test_device();

        // CAP is 64-bit at offset 0x008.
        let cap_lo = read32(&mut dev, 0x008);
        let cap_hi = read32(&mut dev, 0x00C);
        let cap_full = read64(&mut dev, 0x008);

        assert_eq!(cap_full as u32, cap_lo);
        assert_eq!((cap_full >> 32) as u32, cap_hi);
    }

    #[test]
    fn test_iotlb_register_invalidation() {
        let mut dev = create_test_device();

        // Write IOTLB_REG with IVT=1 (bit 63), IIRG=01 (bits 61:60).
        let iotlb_val = (1u64 << 63) | (1u64 << 60);
        write64(&mut dev, spec::registers::IOTLB_REG_OFFSET, iotlb_val);

        // Read back — IVT should be cleared, IAIG should match IIRG.
        let result = read64(&mut dev, spec::registers::IOTLB_REG_OFFSET);
        assert_eq!((result >> 63) & 1, 0); // IVT cleared
        assert_eq!((result >> 57) & 0x3, 1); // IAIG = IIRG = 01
    }

    #[test]
    fn test_cfi_toggle() {
        let mut dev = create_test_device();

        // Enable CFI.
        write32(&mut dev, 0x018, GcmdReg::new().with_cfi(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.cfis());

        // Disable CFI.
        write32(&mut dev, 0x018, GcmdReg::new().with_cfi(false).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(!gsts.cfis());
    }

    #[test]
    fn test_frcd_hi_rw1c() {
        let mut dev = create_test_device();

        // Set F bit (bit 63) in FRCD_HI.
        {
            let mut state = dev.shared.state.write();
            state.frcd_hi = 1u64 << 63;
        }

        // Verify it reads back.
        let frcd_hi = read64(&mut dev, spec::registers::FRCD_HI_OFFSET);
        assert_eq!((frcd_hi >> 63) & 1, 1);

        // Clear F via RW1C.
        write64(&mut dev, spec::registers::FRCD_HI_OFFSET, 1u64 << 63);
        let frcd_hi = read64(&mut dev, spec::registers::FRCD_HI_OFFSET);
        assert_eq!((frcd_hi >> 63) & 1, 0);
    }
}
