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
use spec::invalidation::DescriptorType;
use spec::irte::Irte;
use spec::irte::IrteLo;
use spec::irte::SourceValidationType;
use spec::pte::SlPte;
use spec::registers::CapReg;
use spec::registers::CcmdReg;
use spec::registers::EcapReg;
use spec::registers::FaultReason;
use spec::registers::FectlReg;
use spec::registers::FrcdHi;
use spec::registers::FrcdLo;
use spec::registers::FstsReg;
use spec::registers::GcmdReg;
use spec::registers::GstsReg;
use spec::registers::IcsReg;
use spec::registers::IectlReg;
use spec::registers::IotlbReg;
use spec::registers::IqaReg;
use spec::registers::IqhReg;
use spec::registers::IqtReg;
use spec::registers::IrtaReg;
use spec::registers::MmioRegister as Reg;
use spec::registers::NumDomains;
use spec::registers::RtaddrReg;
use spec::registers::VersionReg;
use spec::root_context::AddressWidth;
use spec::root_context::ContextEntry;
use spec::root_context::RootEntry;
use spec::root_context::TranslationType;
use std::ops::RangeInclusive;
use std::sync::Arc;
use zerocopy::FromBytes;

/// MMIO region size (4KB).
pub const MMIO_REGION_SIZE: u64 = spec::registers::MMIO_REGION_SIZE;

// =============================================================================
// Hardcoded Capability Values (1B.3)
// =============================================================================

/// VT-d version 1.0 (major=1, minor=0).
const VER_VALUE: u32 = VersionReg::new().with_max(1).with_min(0).into_bits();

/// Maximum guest address width in bits.
const MGAW_BITS: u8 = 48;

/// SAGAW bitmask: supported address widths (39-bit + 48-bit).
const SAGAW_MASK: u8 = (1 << AddressWidth::AW_39BIT.0) | (1 << AddressWidth::AW_48BIT.0);

/// Number of fault recording registers.
const NUM_FAULT_RECORDS: u8 = 1;

/// Capability Register value.
const CAP_VALUE: u64 = CapReg::new()
    .with_nd(NumDomains::ND_64K.0)
    .with_afl(false)
    .with_rwbf(false)
    .with_cm(false)
    .with_sagaw(SAGAW_MASK)
    .with_mgaw(MGAW_BITS - 1)
    .with_zlr(true)
    .with_fro(Reg::FRCD_DW0.0 / 16)
    .with_sllps(spec::registers::SLLPS_2MB | spec::registers::SLLPS_1GB)
    .with_psi(true)
    .with_nfr(NUM_FAULT_RECORDS - 1)
    .with_mamv(30 - 12) // max invalidation range = 1GB (2^30) / 4KB page (2^12)
    .with_dwd(true)
    .with_drd(true)
    .into_bits();

/// Extended Capability Register value.
const ECAP_VALUE: u64 = EcapReg::new()
    .with_c(true)
    .with_qi(true)
    .with_ir(true)
    .with_eim(true)
    .with_pt(true)
    .with_sc(true)
    .with_iro(Reg::IVA.0 / 16)
    .with_mhmv(15) // max IM field in interrupt cache invalidation (4-bit max)
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
#[derive(Debug, inspect::Inspect)]
struct VtdState {
    // -- Global status (mirrors GCMD operations) --
    /// Global Status Register value.
    gsts: GstsReg,

    // -- Root table --
    /// Root Table Address Register (raw value, written freely).
    rtaddr: RtaddrReg,
    /// Latched root table address (set when SRTP is processed).
    latched_rtaddr: RtaddrReg,

    // -- Interrupt remapping table --
    /// Interrupt Remapping Table Address Register (raw value).
    irta: IrtaReg,
    /// Latched IRTA (set when SIRTP is processed).
    latched_irta: IrtaReg,

    // -- Context command register (register-based invalidation) --
    ccmd: CcmdReg,

    // -- Fault recording --
    /// Fault Status Register (RW1C bits: PFO, IQE, ICE, ITE).
    fsts: FstsReg,
    /// Fault Event Control Register.
    fectl: FectlReg,
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
    frcd_lo: FrcdLo,
    /// Fault Recording Register — high 64 bits.
    frcd_hi: FrcdHi,

    // -- Invalidation queue --
    /// Invalidation Queue Address Register.
    iqa: IqaReg,
    /// Invalidation Queue Head.
    iqh: IqhReg,
    /// Invalidation Queue Tail.
    iqt: IqtReg,
    /// Invalidation Completion Status Register (IWC bit).
    ics: IcsReg,
    /// Invalidation Event Control Register.
    iectl: IectlReg,
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
    iotlb: IotlbReg,
}

impl VtdState {
    fn new() -> Self {
        Self {
            // Per VT-d spec §10.4.10 and §10.4.21, FECTL.IM and IECTL.IM
            // default to 1 (masked) at power-up reset. Without this, the
            // IOMMU may deliver spurious MSIs with uninitialized FEADDR/IEADDR
            // registers (address=0, data=0) during early initialization, which
            // can inject vector 0 (#DE) into the guest and crash it.
            fectl: FectlReg::new().with_im(true),
            iectl: IectlReg::new().with_im(true),
            gsts: GstsReg::new(),
            rtaddr: RtaddrReg::new(),
            latched_rtaddr: RtaddrReg::new(),
            irta: IrtaReg::new(),
            latched_irta: IrtaReg::new(),
            ccmd: CcmdReg::new(),
            fsts: FstsReg::new(),
            fedata: 0,
            feaddr: 0,
            feuaddr: 0,
            frcd_lo: FrcdLo::new(),
            frcd_hi: FrcdHi::new(),
            iqa: IqaReg::new(),
            iqh: IqhReg::new(),
            iqt: IqtReg::new(),
            ics: IcsReg::new(),
            iedata: 0,
            ieaddr: 0,
            ieuaddr: 0,
            iva: 0,
            iotlb: IotlbReg::new(),
        }
    }
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
    /// Registered interrupt routes for invalidation callbacks.
    retranslate_interrupts: iommu_common::RetranslateInterruptsList,
}

impl VtdSharedState {
    /// Create new shared state.
    fn new(guest_memory: GuestMemory, signal_msi: Arc<dyn SignalMsi>) -> Self {
        Self {
            guest_memory,
            state: RwLock::new(VtdState::new()),
            signal_msi,
            retranslate_interrupts: iommu_common::RetranslateInterruptsList::new(),
        }
    }

    /// Returns whether translation is currently enabled (GSTS.TES).
    pub fn is_enabled(&self) -> bool {
        let state = self.state.read();
        state.gsts.tes()
    }

    /// Returns whether interrupt remapping is currently enabled (GSTS.IRES).
    pub fn is_ir_enabled(&self) -> bool {
        let state = self.state.read();
        state.gsts.ires()
    }

    /// Create a per-device IOVA→GPA translator.
    pub fn translator(self: &Arc<Self>) -> VtdTranslator {
        VtdTranslator {
            shared: self.clone(),
        }
    }

    /// Create a per-device MSI remapping wrapper.
    pub fn wrap_signal_msi(self: &Arc<Self>, inner: Arc<dyn SignalMsi>) -> Arc<VtdSignalMsi> {
        Arc::new(VtdSignalMsi {
            shared: self.clone(),
            inner,
        })
    }

    /// Deliver the IOMMU's own fault event MSI.
    ///
    /// VT-d delivers its own MSIs directly (not through its own interrupt
    /// remapping). Uses `signal_msi(None, ...)` — the `None` devid means
    /// if this MSI were to pass through a VtdSignalMsi wrapper it would be
    /// dropped, which is correct (IOMMU MSIs must not loop through IR).
    fn deliver_fault_interrupt(&self, state: &VtdState) {
        let fectl = state.fectl;
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
        let iectl = state.iectl;
        if iectl.im() {
            return;
        }
        let addr = (state.ieuaddr as u64) << 32 | (state.ieaddr as u64);
        let data = state.iedata;
        self.signal_msi.signal_msi(None, addr, data);
    }

    // =========================================================================
    // Fault Recording (1D)
    // =========================================================================

    /// Record a translation fault in the Fault Recording Register.
    ///
    /// Must be called under a **write lock** on `self.state`.
    /// If FPD is set, the fault is silently suppressed.
    fn record_fault_locked(
        &self,
        state: &mut VtdState,
        source_id: u16,
        fault_reason: FaultReason,
        fault_addr: u64,
        is_read: bool,
        fpd: bool,
    ) {
        if fpd {
            return;
        }

        // Check if FRCD[0].F is already set (fault record occupied).
        if state.frcd_hi.f() {
            // Overflow — set PFO, discard new fault.
            let mut fsts = state.fsts;
            fsts.set_pfo(true);
            state.fsts = fsts;
            return;
        }

        // Write fault recording register.
        let frcd_lo = FrcdLo::new().with_fi(fault_addr >> 12);
        let frcd_hi = FrcdHi::new()
            .with_sid(source_id)
            .with_fr(fault_reason.0)
            .with_t(is_read)
            .with_f(true);

        state.frcd_lo = frcd_lo;
        state.frcd_hi = frcd_hi;

        // Signal fault event MSI if unmasked.
        // PPF transitions 0→1 since we just set F=1 and it was 0 before.
        let fectl = state.fectl;
        if !fectl.im() {
            self.deliver_fault_interrupt(state);
        } else {
            // Masked — set IP (interrupt pending).
            state.fectl = state.fectl.with_ip(true);
        }
    }

    // =========================================================================
    // DMA Translation (1E)
    // =========================================================================

    /// Translate an IOVA to a GPA while holding the read lock.
    ///
    /// Returns `Ok(gpa)` on success or `Err(VtdFault)` on failure.
    fn translate_locked(
        &self,
        state: &VtdState,
        bus: u8,
        devfn: u8,
        iova: u64,
        write: bool,
    ) -> Result<u64, VtdFault> {
        let gsts = state.gsts;
        if !gsts.tes() {
            // Translation disabled — identity mapping.
            return Ok(iova);
        }

        // Check IOVA against MGAW.
        if iova >= (1u64 << MGAW_BITS) {
            return Err(VtdFault::AddressBeyondMgaw {
                source_id: bdf(bus, devfn),
                iova,
            });
        }

        // Root entry lookup.
        let root_table_addr = state.latched_rtaddr.root_table_address();
        let root_entry = self.lookup_root_entry(root_table_addr, bus)?;
        let source_id = bdf(bus, devfn);

        // Context entry lookup.
        let context_table_ptr = root_entry.lo.context_table_address();
        let context_entry = self.lookup_context_entry(context_table_ptr, devfn, source_id)?;

        let fpd = context_entry.lo.fpd();
        let tt = TranslationType(context_entry.lo.tt());
        let aw = AddressWidth(context_entry.hi.aw());

        match tt {
            TranslationType::UNTRANSLATED_ONLY | TranslationType::ALL => {
                let levels = aw.levels().ok_or(VtdFault::InvalidContextEntry {
                    source_id,
                    iova,
                    fpd,
                })?;
                let pt_root = context_entry.lo.page_table_address();
                self.walk_sl_page_table(pt_root, iova, levels, write, source_id, fpd)
            }
            TranslationType::PASS_THROUGH => Ok(iova),
            _ => Err(VtdFault::InvalidContextEntry {
                source_id,
                iova,
                fpd,
            }),
        }
    }

    /// Look up a root entry by bus number.
    fn lookup_root_entry(&self, root_table_addr: u64, bus: u8) -> Result<RootEntry, VtdFault> {
        let entry_addr = root_table_addr + (bus as u64) * 16;
        let entry: RootEntry = self.guest_memory.read_plain(entry_addr).map_err(|_| {
            VtdFault::RootEntryAccessError {
                source_id: (bus as u16) << 8,
                iova: 0,
            }
        })?;

        if !entry.lo.p() {
            return Err(VtdFault::RootNotPresent {
                source_id: (bus as u16) << 8,
                iova: 0,
            });
        }

        Ok(entry)
    }

    /// Look up a context entry by devfn.
    fn lookup_context_entry(
        &self,
        context_table_ptr: u64,
        devfn: u8,
        source_id: u16,
    ) -> Result<ContextEntry, VtdFault> {
        let entry_addr = context_table_ptr + (devfn as u64) * 16;
        let entry: ContextEntry = self
            .guest_memory
            .read_plain(entry_addr)
            .map_err(|_| VtdFault::ContextEntryAccessError { source_id, iova: 0 })?;

        if !entry.lo.p() {
            return Err(VtdFault::ContextNotPresent { source_id, iova: 0 });
        }

        Ok(entry)
    }

    /// Walk second-level page tables to translate an IOVA to a GPA.
    fn walk_sl_page_table(
        &self,
        pt_root: u64,
        iova: u64,
        levels: u8,
        write: bool,
        source_id: u16,
        fpd: bool,
    ) -> Result<u64, VtdFault> {
        let mut table_addr = pt_root;
        let mut can_read = true;
        let mut can_write = true;

        for level in (1..=levels).rev() {
            let index = SlPte::iova_index(iova, level);
            let pte_addr = table_addr + (index as u64) * 8;

            let pte: SlPte = self.guest_memory.read_plain(pte_addr).map_err(|_| {
                VtdFault::PageTableAccessError {
                    source_id,
                    iova,
                    fpd,
                }
            })?;

            if !pte.is_present() {
                // R=W=0: fault reason depends on access type (§7.1.3).
                // Write → 0x05 (write denied), Read → 0x06 (read denied).
                return Err(VtdFault::AccessDenied {
                    source_id,
                    iova,
                    fpd,
                    write,
                });
            }

            // AND-accumulate permissions across levels (§3.7.1).
            can_read &= pte.r();
            can_write &= pte.w();

            // Large page check at level 2 (2MB) and level 3 (1GB).
            if pte.ps() && (level == 2 || level == 3) {
                if (write && !can_write) || (!write && !can_read) {
                    return Err(VtdFault::AccessDenied {
                        source_id,
                        iova,
                        fpd,
                        write,
                    });
                }
                return Ok(pte.large_page_gpa(iova, level));
            }

            if level == 1 {
                // Leaf entry (4KB page).
                if (write && !can_write) || (!write && !can_read) {
                    return Err(VtdFault::AccessDenied {
                        source_id,
                        iova,
                        fpd,
                        write,
                    });
                }
                return Ok(pte.phys_address() | (iova & 0xFFF));
            }

            // Non-leaf: follow to next level.
            table_addr = pte.phys_address();
        }

        // Should not reach here if levels >= 1.
        Err(VtdFault::AccessDenied {
            source_id,
            iova,
            fpd,
            write,
        })
    }

    // =========================================================================
    // Interrupt Remapping (1F)
    // =========================================================================

    /// Remap an MSI using the Interrupt Remapping Table.
    ///
    /// Called under the read lock. On fault, returns `Err(VtdFault)`.
    fn remap_msi_locked(
        &self,
        state: &VtdState,
        source_id: u16,
        address: u64,
        data: u32,
    ) -> Result<(u64, u32), VtdFault> {
        let gsts = state.gsts;
        if !gsts.ires() {
            // IR disabled — pass through.
            return Ok((address, data));
        }

        // Check for compatibility-format MSI (bit 4 of address = 0).
        let is_remappable = (address >> 4) & 1 != 0;
        if !is_remappable {
            return self.handle_compat_format(state, source_id, address, data);
        }

        // Extract IRTE index from remappable-format MSI.
        let irte_index = Self::extract_irte_index(address, data);

        // Validate index against IRT size.
        let irta = state.latched_irta;
        let max_entries = irta.entry_count();
        if irte_index as u32 >= max_entries {
            return Err(VtdFault::IrteIndexExceedsSize {
                source_id,
                irte_index,
            });
        }

        // Look up IRTE.
        let irt_base = irta.irt_base_address();
        let irte = self.lookup_irte(irt_base, irte_index, source_id)?;

        // Validate source ID.
        self.validate_irte_source(source_id, &irte, irte_index)?;

        // Check IRTE mode (IM=0 for remapped, IM=1 for posted — not supported).
        let irte_lo = irte.lo;
        if irte_lo.im() {
            return Err(VtdFault::IrteReservedField {
                source_id,
                irte_index,
                fpd: irte_lo.fpd(),
            });
        }

        // Construct remapped MSI address and data.
        let eime = irta.eime();
        let (new_address, new_data) = Self::construct_remapped_msi(irte_lo, eime);

        Ok((new_address, new_data))
    }

    /// Handle compatibility-format MSI when IR is enabled.
    fn handle_compat_format(
        &self,
        state: &VtdState,
        source_id: u16,
        address: u64,
        data: u32,
    ) -> Result<(u64, u32), VtdFault> {
        let irta = state.latched_irta;
        let gsts = state.gsts;

        // EIME=1 → always block compatibility-format interrupts.
        if irta.eime() {
            return Err(VtdFault::CompatibilityFormatBlocked { source_id });
        }

        // CFIS=1 → pass through.
        if gsts.cfis() {
            return Ok((address, data));
        }

        // CFIS=0 → block.
        Err(VtdFault::CompatibilityFormatBlocked { source_id })
    }

    /// Extract the IRTE index from remappable-format MSI address/data.
    ///
    /// Handle = {address[2], address[19:5]} (16-bit).
    /// SHV = address[3].
    /// If SHV=0: index = handle.
    /// If SHV=1: index = handle + data[15:0].
    fn extract_irte_index(address: u64, data: u32) -> u16 {
        let handle_lo = ((address >> 5) & 0x7FFF) as u16; // address[19:5] = 15 bits
        let handle_hi = ((address >> 2) & 1) as u16; // address[2] = 1 bit
        let handle = (handle_hi << 15) | handle_lo;

        let shv = (address >> 3) & 1 != 0;
        if shv {
            handle.wrapping_add(data as u16)
        } else {
            handle
        }
    }

    /// Look up an IRTE by index.
    fn lookup_irte(
        &self,
        irt_base: u64,
        irte_index: u16,
        source_id: u16,
    ) -> Result<Irte, VtdFault> {
        let entry_addr = irt_base + (irte_index as u64) * 16;
        let irte: Irte =
            self.guest_memory
                .read_plain(entry_addr)
                .map_err(|_| VtdFault::IrteAccessError {
                    source_id,
                    irte_index,
                })?;

        if !irte.lo.p() {
            return Err(VtdFault::IrteNotPresent {
                source_id,
                irte_index,
                fpd: irte.lo.fpd(),
            });
        }

        Ok(irte)
    }

    /// Validate the interrupt source against the IRTE's SVT/SID/SQ fields.
    fn validate_irte_source(
        &self,
        source_id: u16,
        irte: &Irte,
        irte_index: u16,
    ) -> Result<(), VtdFault> {
        let svt = SourceValidationType(irte.hi.svt());
        let irte_sid = irte.hi.sid();
        let sq = irte.hi.sq();
        let fpd = irte.lo.fpd();

        match svt {
            SourceValidationType::NONE => Ok(()),
            SourceValidationType::VERIFY_SID => {
                // Mask based on SQ: SQ=00 all 16 bits, SQ=01 ignore bit 2,
                // SQ=10 ignore bits 2:1, SQ=11 ignore bits 2:0.
                let mask: u16 = match sq {
                    0b00 => 0xFFFF,
                    0b01 => !0x04,
                    0b10 => !0x06,
                    0b11 => !0x07,
                    _ => 0xFFFF,
                };
                if (source_id & mask) != (irte_sid & mask) {
                    Err(VtdFault::SourceValidationFailed {
                        source_id,
                        irte_index,
                        fpd,
                    })
                } else {
                    Ok(())
                }
            }
            SourceValidationType::VERIFY_BUS_RANGE => {
                // SID[7:0] = start bus, SID[15:8] = end bus.
                let start_bus = (irte_sid & 0xFF) as u8;
                let end_bus = ((irte_sid >> 8) & 0xFF) as u8;
                let source_bus = ((source_id >> 8) & 0xFF) as u8;
                if source_bus < start_bus || source_bus > end_bus {
                    Err(VtdFault::SourceValidationFailed {
                        source_id,
                        irte_index,
                        fpd,
                    })
                } else {
                    Ok(())
                }
            }
            _ => {
                // Unknown SVT — treat as no validation.
                Ok(())
            }
        }
    }

    /// Construct remapped MSI address and data from an IRTE.
    fn construct_remapped_msi(irte_lo: IrteLo, eime: bool) -> (u64, u32) {
        let dst = if eime {
            // x2APIC: full 32-bit destination.
            irte_lo.dst()
        } else {
            // xAPIC: 8-bit destination from bits 15:8 of DST field.
            irte_lo.xapic_destination() as u32
        };

        let new_address = x86defs::msi::MsiAddress::new()
            .with_address(x86defs::msi::MSI_ADDRESS)
            .with_virt_destination(dst as u16)
            .with_destination_mode_logical(irte_lo.dm())
            .with_redirection_hint(irte_lo.rh());

        let new_data = x86defs::msi::MsiData::new()
            .with_vector(irte_lo.vector())
            .with_delivery_mode(irte_lo.dlm())
            .with_trigger_mode_level(irte_lo.tm());

        (new_address.into_bits() as u64, new_data.into_bits())
    }
}

// =============================================================================
// VtdFault — IOMMU-specific translation/remapping error
// =============================================================================

/// VT-d translation or interrupt remapping fault.
///
/// Each variant carries the source ID and faulting address/index,
/// and maps to a specific VT-d fault reason code.
#[derive(Debug, thiserror::Error)]
#[expect(missing_docs)]
pub enum VtdFault {
    #[error("root entry not present (source_id={source_id:#06x})")]
    RootNotPresent { source_id: u16, iova: u64 },

    #[error("context entry not present (source_id={source_id:#06x})")]
    ContextNotPresent { source_id: u16, iova: u64 },

    #[error("invalid context entry (source_id={source_id:#06x}, iova={iova:#x})")]
    InvalidContextEntry {
        source_id: u16,
        iova: u64,
        fpd: bool,
    },

    #[error("IOVA beyond MGAW (source_id={source_id:#06x}, iova={iova:#x})")]
    AddressBeyondMgaw { source_id: u16, iova: u64 },

    #[error("{} access denied (source_id={source_id:#06x}, iova={iova:#x})", if *write { "write" } else { "read" })]
    AccessDenied {
        source_id: u16,
        iova: u64,
        fpd: bool,
        write: bool,
    },

    #[error("root entry access error (source_id={source_id:#06x})")]
    RootEntryAccessError { source_id: u16, iova: u64 },

    #[error("context entry access error (source_id={source_id:#06x})")]
    ContextEntryAccessError { source_id: u16, iova: u64 },

    #[error("page table access error (source_id={source_id:#06x}, iova={iova:#x})")]
    PageTableAccessError {
        source_id: u16,
        iova: u64,
        fpd: bool,
    },

    #[error("IRTE not present (source_id={source_id:#06x}, index={irte_index})")]
    IrteNotPresent {
        source_id: u16,
        irte_index: u16,
        fpd: bool,
    },

    #[error("IRTE access error (source_id={source_id:#06x}, index={irte_index})")]
    IrteAccessError { source_id: u16, irte_index: u16 },

    #[error("IRTE index exceeds IRT size (source_id={source_id:#06x}, index={irte_index})")]
    IrteIndexExceedsSize { source_id: u16, irte_index: u16 },

    #[error("IRTE reserved field set (source_id={source_id:#06x}, index={irte_index})")]
    IrteReservedField {
        source_id: u16,
        irte_index: u16,
        fpd: bool,
    },

    #[error("source validation failed (source_id={source_id:#06x})")]
    SourceValidationFailed {
        source_id: u16,
        irte_index: u16,
        fpd: bool,
    },

    #[error("compatibility-format interrupt blocked (source_id={source_id:#06x})")]
    CompatibilityFormatBlocked { source_id: u16 },
}

impl VtdFault {
    /// Get the fault reason code for this fault.
    fn fault_reason(&self) -> FaultReason {
        match self {
            Self::RootNotPresent { .. } => FaultReason::ROOT_NOT_PRESENT,
            Self::ContextNotPresent { .. } => FaultReason::CONTEXT_NOT_PRESENT,
            Self::InvalidContextEntry { .. } => FaultReason::INVALID_CONTEXT_ENTRY,
            Self::AddressBeyondMgaw { .. } => FaultReason::ADDRESS_BEYOND_MGAW,
            Self::AccessDenied { write: true, .. } => FaultReason::WRITE_ACCESS_DENIED,
            Self::AccessDenied { write: false, .. } => FaultReason::READ_ACCESS_DENIED,
            Self::RootEntryAccessError { .. } => FaultReason::ROOT_ENTRY_ACCESS_ERROR,
            Self::ContextEntryAccessError { .. } => FaultReason::CONTEXT_ENTRY_ACCESS_ERROR,
            Self::PageTableAccessError { .. } => FaultReason::SL_PTE_ACCESS_ERROR,
            Self::IrteNotPresent { .. } => FaultReason::IRTE_NOT_PRESENT,
            Self::IrteAccessError { .. } => FaultReason::IRTE_ACCESS_ERROR,
            Self::IrteIndexExceedsSize { .. } => FaultReason::IR_INDEX_EXCEEDS_SIZE,
            Self::IrteReservedField { .. } => FaultReason::IRTE_RESERVED_FIELD,
            Self::SourceValidationFailed { .. } => FaultReason::SOURCE_ID_VERIFICATION_FAIL,
            Self::CompatibilityFormatBlocked { .. } => FaultReason::COMPAT_FORMAT_BLOCKED,
        }
    }

    /// Get the source ID from this fault.
    fn source_id(&self) -> u16 {
        match self {
            Self::RootNotPresent { source_id, .. }
            | Self::ContextNotPresent { source_id, .. }
            | Self::InvalidContextEntry { source_id, .. }
            | Self::AddressBeyondMgaw { source_id, .. }
            | Self::AccessDenied { source_id, .. }
            | Self::RootEntryAccessError { source_id, .. }
            | Self::ContextEntryAccessError { source_id, .. }
            | Self::PageTableAccessError { source_id, .. }
            | Self::IrteNotPresent { source_id, .. }
            | Self::IrteAccessError { source_id, .. }
            | Self::IrteIndexExceedsSize { source_id, .. }
            | Self::IrteReservedField { source_id, .. }
            | Self::SourceValidationFailed { source_id, .. }
            | Self::CompatibilityFormatBlocked { source_id, .. } => *source_id,
        }
    }

    /// Get the faulting address (IOVA for DMA, 0 for IR faults).
    fn fault_address(&self) -> u64 {
        match self {
            Self::RootNotPresent { iova, .. }
            | Self::ContextNotPresent { iova, .. }
            | Self::InvalidContextEntry { iova, .. }
            | Self::AddressBeyondMgaw { iova, .. }
            | Self::AccessDenied { iova, .. }
            | Self::RootEntryAccessError { iova, .. }
            | Self::ContextEntryAccessError { iova, .. }
            | Self::PageTableAccessError { iova, .. } => *iova,
            // Interrupt remapping faults don't have an IOVA.
            Self::IrteNotPresent { .. }
            | Self::IrteAccessError { .. }
            | Self::IrteIndexExceedsSize { .. }
            | Self::IrteReservedField { .. }
            | Self::SourceValidationFailed { .. }
            | Self::CompatibilityFormatBlocked { .. } => 0,
        }
    }

    /// Whether fault processing is disabled for this fault.
    fn fpd(&self) -> bool {
        match self {
            Self::InvalidContextEntry { fpd, .. }
            | Self::AccessDenied { fpd, .. }
            | Self::PageTableAccessError { fpd, .. }
            | Self::IrteNotPresent { fpd, .. }
            | Self::IrteReservedField { fpd, .. }
            | Self::SourceValidationFailed { fpd, .. } => *fpd,
            // Faults without FPD context are always recorded.
            Self::RootNotPresent { .. }
            | Self::ContextNotPresent { .. }
            | Self::AddressBeyondMgaw { .. }
            | Self::RootEntryAccessError { .. }
            | Self::ContextEntryAccessError { .. }
            | Self::IrteAccessError { .. }
            | Self::IrteIndexExceedsSize { .. }
            | Self::CompatibilityFormatBlocked { .. } => false,
        }
    }

    /// Record this fault in the IOMMU's fault recording registers.
    ///
    /// `is_write` is the access type of the faulting request. This sets
    /// FRCD.T (0=write, 1=read/non-write) per §7.1. The access type is a
    /// property of the DMA/interrupt request, not of the fault itself.
    ///
    /// Takes a write lock on the shared state.
    fn record(&self, shared: &VtdSharedState, is_write: bool) {
        let mut state = shared.state.write();
        shared.record_fault_locked(
            &mut state,
            self.source_id(),
            self.fault_reason(),
            self.fault_address(),
            !is_write, // FRCD.T: 1=read, 0=write
            self.fpd(),
        );
    }
}

/// Combine bus and devfn into a 16-bit BDF source ID.
fn bdf(bus: u8, devfn: u8) -> u16 {
    ((bus as u16) << 8) | devfn as u16
}

// =============================================================================
// VtdTranslator — IommuTranslator implementation (1G.2)
// =============================================================================

/// Per-device IOVA→GPA translator for Intel VT-d.
///
/// Implements [`iommu_common::IommuTranslator`] using the closure-based API
/// that holds the read lock across translation and the memory access operation.
#[derive(Clone)]
pub struct VtdTranslator {
    shared: Arc<VtdSharedState>,
}

impl iommu_common::IommuTranslator for VtdTranslator {
    type Error = VtdFault;

    fn max_iova(&self) -> u64 {
        // 48-bit address space.
        (1u64 << 48) - 1
    }

    fn translate<R>(
        &self,
        rid: u16,
        iova: u64,
        write: bool,
        op: impl FnOnce(u64) -> R,
    ) -> Result<R, iommu_common::TranslationFault<VtdFault>> {
        let bus = (rid >> 8) as u8;
        let devfn = rid as u8;

        // Hold the read lock across translate + op for TOCTOU safety.
        let state = self.shared.state.read();
        let gpa = match self
            .shared
            .translate_locked(&state, bus, devfn, iova, write)
        {
            Ok(gpa) => gpa,
            Err(fault) => {
                // Drop the read lock before acquiring write lock for fault recording.
                drop(state);
                fault.record(&self.shared, write);
                return Err(iommu_common::TranslationFault { iova, error: fault });
            }
        };

        let result = op(gpa);
        drop(state);
        Ok(result)
    }
}

// =============================================================================
// VtdSignalMsi — MSI remapping wrapper (1G.3)
// =============================================================================

/// MSI remapping wrapper for Intel VT-d interrupt remapping.
///
/// Wraps an inner `SignalMsi` and intercepts MSI writes to remap them
/// through the Interrupt Remapping Table.
pub struct VtdSignalMsi {
    shared: Arc<VtdSharedState>,
    inner: Arc<dyn SignalMsi>,
}

impl SignalMsi for VtdSignalMsi {
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
        let Some(device_id) = devid else {
            // No source ID — drop the MSI. Without a BDF, source validation
            // and IRTE lookup are impossible.
            return;
        };
        let source_id = device_id as u16;

        let state = self.shared.state.read();
        match self
            .shared
            .remap_msi_locked(&state, source_id, address, data)
        {
            Ok((new_address, new_data)) => {
                drop(state);
                self.inner.signal_msi(devid, new_address, new_data);
            }
            Err(fault) => {
                drop(state);
                tracelimit::warn_ratelimited!(
                    source_id,
                    address,
                    data,
                    error = &fault as &dyn std::error::Error,
                    "vtd: MSI remapping fault, interrupt dropped"
                );
                // MSI is a posted write transaction, so is_write=true.
                fault.record(&self.shared, true);
            }
        }
    }
}

impl iommu_common::InterruptRemapper for VtdSharedState {
    fn remap_msi(&self, device_id: u16, address: u64, data: u32) -> Option<(u64, u32)> {
        let state = self.state.read();
        match self.remap_msi_locked(&state, device_id, address, data) {
            Ok(result) => Some(result),
            Err(fault) => {
                drop(state);
                // This runs at interrupt delivery time (the IOAPIC wiring
                // translates its routes lazily, on assertion), so a fault here
                // corresponds to an interrupt actually firing through a bad
                // entry. Record it, matching hardware which performs the IRTE
                // lookup and records faults at delivery time.
                tracelimit::warn_ratelimited!(
                    device_id,
                    address,
                    data,
                    error = &fault as &dyn std::error::Error,
                    "vtd: interrupt remapping fault on registered-route delivery, dropping interrupt"
                );
                fault.record(self, true);
                None
            }
        }
    }

    fn register_route(&self, route: &Arc<dyn iommu_common::RetranslateInterrupts>) {
        self.retranslate_interrupts.register(route);
    }

    fn unregister_route(&self, route: &Arc<dyn iommu_common::RetranslateInterrupts>) {
        self.retranslate_interrupts.unregister(route);
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
        /// Extract the lo DWORD from a 64-bit value.
        fn lo(val: u64) -> u32 {
            val as u32
        }
        /// Extract the hi DWORD from a 64-bit value.
        fn hi(val: u64) -> u32 {
            (val >> 32) as u32
        }

        match Reg(offset) {
            Reg::VER => VER_VALUE,
            Reg::CAP => lo(CAP_VALUE),
            Reg::CAP_HI => hi(CAP_VALUE),
            Reg::ECAP => lo(ECAP_VALUE),
            Reg::ECAP_HI => hi(ECAP_VALUE),
            Reg::GCMD => 0, // write-only
            Reg::GSTS => state.gsts.into_bits(),
            Reg::RTADDR => lo(state.rtaddr.into_bits()),
            Reg::RTADDR_HI => hi(state.rtaddr.into_bits()),
            Reg::CCMD => lo(state.ccmd.into_bits()),
            Reg::CCMD_HI => hi(state.ccmd.into_bits()),
            Reg::FSTS => self.read_fsts(state),
            Reg::FECTL => state.fectl.into_bits(),
            Reg::FEDATA => state.fedata,
            Reg::FEADDR => state.feaddr,
            Reg::FEUADDR => state.feuaddr,
            Reg::IQH => lo(state.iqh.into_bits()),
            Reg::IQH_HI => hi(state.iqh.into_bits()),
            Reg::IQT => lo(state.iqt.into_bits()),
            Reg::IQT_HI => hi(state.iqt.into_bits()),
            Reg::IQA => lo(state.iqa.into_bits()),
            Reg::IQA_HI => hi(state.iqa.into_bits()),
            Reg::ICS => state.ics.into_bits(),
            Reg::IECTL => state.iectl.into_bits(),
            Reg::IEDATA => state.iedata,
            Reg::IEADDR => state.ieaddr,
            Reg::IEUADDR => state.ieuaddr,
            Reg::IRTA => lo(state.irta.into_bits()),
            Reg::IRTA_HI => hi(state.irta.into_bits()),
            Reg::IVA => lo(state.iva),
            Reg::IVA_HI => hi(state.iva),
            Reg::IOTLB => lo(state.iotlb.into_bits()),
            Reg::IOTLB_HI => hi(state.iotlb.into_bits()),
            Reg::FRCD_DW0 => lo(state.frcd_lo.into_bits()),
            Reg::FRCD_DW1 => hi(state.frcd_lo.into_bits()),
            Reg::FRCD_DW2 => lo(state.frcd_hi.into_bits()),
            Reg::FRCD_DW3 => hi(state.frcd_hi.into_bits()),
            _ => 0,
        }
    }

    /// Read FSTS with PPF dynamically computed as OR of FRCD[n].F bits.
    fn read_fsts(&self, state: &VtdState) -> u32 {
        let mut fsts = state.fsts;
        // PPF = OR of all FRCD[n].F bits. With NFR=0 (1 record), this
        // is just FRCD[0].F.
        fsts.set_ppf(state.frcd_hi.f());
        fsts.into_bits()
    }

    // =========================================================================
    // MMIO Register Write (DWORD granularity)
    // =========================================================================

    /// Write a 32-bit value at a DWORD-aligned MMIO offset.
    ///
    /// Acquires the write lock, performs the register write, and releases it.
    /// 64-bit MMIO writes call this twice (once per DWORD) — this is safe
    /// because every 64-bit VT-d register either has its trigger bit in one
    /// specific DWORD, or is a config register latched by a separate GCMD
    /// write. No register requires atomic writes across both DWORDs.
    fn write_register_dword(&self, offset: u16, value: u32) {
        let mut state = self.shared.state.write();
        let mut retranslate_interrupts = false;
        tracing::trace!(offset, value, "vtd mmio_write_dword");

        /// Merge a DWORD write into the lo or hi half of a 64-bit value.
        fn write_lo(old: u64, value: u32) -> u64 {
            (old & !0xFFFF_FFFF) | value as u64
        }
        fn write_hi(old: u64, value: u32) -> u64 {
            (old & 0xFFFF_FFFF) | ((value as u64) << 32)
        }

        match Reg(offset) {
            // Read-only registers.
            Reg::VER
            | Reg::CAP
            | Reg::CAP_HI
            | Reg::ECAP
            | Reg::ECAP_HI
            | Reg::GSTS
            | Reg::IQH
            | Reg::IQH_HI
            | Reg::FRCD_DW0
            | Reg::FRCD_DW1
            | Reg::FRCD_DW2 => {}

            Reg::GCMD => self.process_gcmd(&mut state, value),

            Reg::RTADDR => {
                state.rtaddr = RtaddrReg::from(write_lo(state.rtaddr.into_bits(), value));
            }
            Reg::RTADDR_HI => {
                state.rtaddr = RtaddrReg::from(write_hi(state.rtaddr.into_bits(), value));
            }

            Reg::CCMD => {
                state.ccmd = CcmdReg::from(write_lo(state.ccmd.into_bits(), value));
            }
            Reg::CCMD_HI => {
                // ICC is bit 63 — process on hi DWORD write.
                let full = write_hi(state.ccmd.into_bits(), value);
                self.process_ccmd(&mut state, full);
            }

            Reg::FSTS => {
                let write_val = FstsReg::from(value);
                let mut fsts = state.fsts;
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
                state.fsts = fsts;
            }

            Reg::FECTL => {
                let new = FectlReg::from(value);
                let old = state.fectl;
                state.fectl = FectlReg::new().with_im(new.im()).with_ip(old.ip());
                if old.im() && !new.im() && old.ip() {
                    state.fectl = state.fectl.with_ip(false);
                    self.shared.deliver_fault_interrupt(&state);
                }
            }

            Reg::FEDATA => state.fedata = value,
            Reg::FEADDR => state.feaddr = value,
            Reg::FEUADDR => state.feuaddr = value,

            Reg::IQT => {
                // Trigger queue processing on lo DWORD write.
                let full = write_lo(state.iqt.into_bits(), value);
                let iqt = IqtReg::from(full);
                state.iqt = IqtReg::new().with_qt(iqt.qt());
                retranslate_interrupts = self.process_invalidation_queue(&mut state);
            }
            Reg::IQT_HI => {
                state.iqt = IqtReg::from(write_hi(state.iqt.into_bits(), value));
            }

            Reg::IQA => {
                if !state.gsts.qies() {
                    state.iqa = IqaReg::from(write_lo(state.iqa.into_bits(), value));
                } else {
                    tracelimit::warn_ratelimited!("vtd: write to IQA while QIE=1, ignored");
                }
            }
            Reg::IQA_HI => {
                if !state.gsts.qies() {
                    state.iqa = IqaReg::from(write_hi(state.iqa.into_bits(), value));
                } else {
                    tracelimit::warn_ratelimited!("vtd: write to IQA while QIE=1, ignored");
                }
            }

            Reg::ICS => {
                let write_val = IcsReg::from(value);
                let mut ics = state.ics;
                if write_val.iwc() {
                    ics.set_iwc(false);
                }
                state.ics = ics;
            }

            Reg::IECTL => {
                let new = IectlReg::from(value);
                let old = state.iectl;
                state.iectl = IectlReg::new().with_im(new.im()).with_ip(old.ip());
                if old.im() && !new.im() && old.ip() {
                    state.iectl = state.iectl.with_ip(false);
                    self.shared.deliver_invalidation_interrupt(&state);
                }
            }

            Reg::IEDATA => state.iedata = value,
            Reg::IEADDR => state.ieaddr = value,
            Reg::IEUADDR => state.ieuaddr = value,

            Reg::IRTA => {
                if !state.gsts.ires() {
                    state.irta = IrtaReg::from(write_lo(state.irta.into_bits(), value));
                } else {
                    tracelimit::warn_ratelimited!("vtd: write to IRTA while IRE=1, ignored");
                }
            }
            Reg::IRTA_HI => {
                if !state.gsts.ires() {
                    state.irta = IrtaReg::from(write_hi(state.irta.into_bits(), value));
                } else {
                    tracelimit::warn_ratelimited!("vtd: write to IRTA while IRE=1, ignored");
                }
            }

            Reg::IVA => state.iva = write_lo(state.iva, value),
            Reg::IVA_HI => state.iva = write_hi(state.iva, value),

            Reg::IOTLB => {
                state.iotlb = IotlbReg::from(write_lo(state.iotlb.into_bits(), value));
            }
            Reg::IOTLB_HI => {
                // IVT is bit 63 — process on hi DWORD write.
                let full = write_hi(state.iotlb.into_bits(), value);
                self.process_iotlb_reg(&mut state, full);
            }

            Reg::FRCD_DW3 => {
                // F bit (bit 31 of this DWORD = bit 63 of FRCD_HI) is RW1C.
                if (value >> 31) & 1 != 0 {
                    state.frcd_hi = state.frcd_hi.with_f(false);
                }
            }

            _ => {} // Unmapped offsets: silently ignored.
        }

        drop(state);
        if retranslate_interrupts {
            self.shared.retranslate_interrupts.invalidate(None);
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
        let mut gsts = state.gsts;

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
                state.iqh = IqhReg::new();
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

        state.gsts = gsts;
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
            state.ccmd = ccmd.with_icc(false).with_caig(ccmd.cirg());
        } else {
            state.ccmd = CcmdReg::from(value);
        }
    }

    /// Process a write to the IOTLB Invalidate Register (0x108).
    ///
    /// Register-based IOTLB invalidation. No-op since we don't cache.
    fn process_iotlb_reg(&self, state: &mut VtdState, value: u64) {
        let reg = IotlbReg::from(value);
        if reg.ivt() {
            // Clear IVT and echo IAIG = IIRG.
            state.iotlb = reg.with_ivt(false).with_iaig(reg.iirg());
        } else {
            state.iotlb = IotlbReg::from(value);
        }
    }

    // =========================================================================
    // Invalidation Queue Processing (1C)
    // =========================================================================

    /// Process the invalidation queue.
    ///
    /// Consumes descriptors from head to tail. Called when the guest writes
    /// IQT.
    fn process_invalidation_queue(&self, state: &mut VtdState) -> bool {
        let gsts = state.gsts;
        if !gsts.qies() {
            return false;
        }

        // Check for IQE — don't process if error is outstanding.
        let fsts = state.fsts;
        if fsts.iqe() {
            return false;
        }

        let iqa = state.iqa;

        // Validate DW=0 (128-bit descriptors only).
        if iqa.dw() {
            tracelimit::warn_ratelimited!("vtd: IQA.DW=1 (256-bit descriptors) not supported");
            let mut fsts = state.fsts;
            fsts.set_iqe(true);
            state.fsts = fsts;
            return false;
        }

        let queue_base = iqa.queue_base_address();
        let queue_size = iqa.queue_size_bytes();
        let head = state.iqh.head_offset();
        let tail = state.iqt.tail_offset();

        let mut current_head = head;
        let mut retranslate_interrupts = false;

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
                    let mut fsts = state.fsts;
                    fsts.set_iqe(true);
                    state.fsts = fsts;
                    break;
                }
            };

            let desc = spec::invalidation::InvalidationDescriptor::read_from_bytes(&descriptor)
                .expect("descriptor is 16 bytes");

            match desc.descriptor_type() {
                DescriptorType::CONTEXT_CACHE_INVALIDATE => {} // no-op
                DescriptorType::IOTLB_INVALIDATE => {}         // no-op
                DescriptorType::DEVICE_TLB_INVALIDATE => {
                    tracelimit::warn_ratelimited!(
                        "vtd: unsupported DEVICE_TLB_INVALIDATE descriptor"
                    );
                }
                DescriptorType::INTERRUPT_ENTRY_CACHE_INVALIDATE => {
                    let desc = spec::invalidation::parse_interrupt_cache_invalidate(&desc);
                    tracing::trace!(
                        granularity = desc.granularity(),
                        im = desc.im(),
                        iidx = desc.iidx(),
                        "vtd: interrupt entry cache invalidate"
                    );
                    retranslate_interrupts = true;
                }
                DescriptorType::INVALIDATION_WAIT => {
                    self.process_invalidation_wait(state, &descriptor);
                }
                dt => {
                    tracelimit::warn_ratelimited!(?dt, "vtd: unknown invalidation descriptor type");
                    let mut fsts = state.fsts;
                    fsts.set_iqe(true);
                    state.fsts = fsts;
                    break;
                }
            }

            // Advance head with wrap-around.
            current_head = (current_head + 16) % queue_size;
        }

        // Update head register.
        state.iqh = IqhReg::new().with_qh((current_head >> 4) as u32);
        retranslate_interrupts
    }

    /// Process an INVALIDATION_WAIT descriptor (type 0x05).
    fn process_invalidation_wait(&self, state: &mut VtdState, descriptor: &[u8; 16]) {
        let desc = spec::invalidation::InvalidationDescriptor::read_from_bytes(descriptor)
            .expect("descriptor is 16 bytes");
        let (lo, hi) = spec::invalidation::parse_invalidation_wait(&desc);

        if lo.sw() {
            let status_address = hi.status_address();

            if let Err(e) = self
                .shared
                .guest_memory
                .write_at(status_address, &lo.status_data().to_le_bytes())
            {
                tracelimit::warn_ratelimited!(
                    error = &e as &dyn std::error::Error,
                    addr = status_address,
                    "vtd: failed to write invalidation wait status"
                );
            }
        }

        if lo.iflag() {
            // Set IWC in ICS.
            let mut ics = state.ics;
            ics.set_iwc(true);
            state.ics = ics;

            // Signal invalidation completion interrupt.
            let iectl = state.iectl;
            if !iectl.im() {
                self.shared.deliver_invalidation_interrupt(state);
            } else {
                // Masked — set IP.
                state.iectl = state.iectl.with_ip(true);
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
                self.write_register_dword(offset as u16, val as u32);
                self.write_register_dword((offset + 4) as u16, (val >> 32) as u32);
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
        *state = VtdState::new();
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
        let gsts = state.gsts;
        req.respond()
            .hex("mmio_base", self.mmio_base)
            .field("translation_enabled", gsts.tes())
            .field("ir_enabled", gsts.ires())
            .field("qi_enabled", gsts.qies())
            .hex("root_table_addr", state.latched_rtaddr.root_table_address())
            .hex("irt_addr", state.latched_irta.irt_base_address())
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
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

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
            state.fsts = FstsReg::new().with_pfo(true).with_iqe(true);
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
            state.frcd_hi = FrcdHi::new().with_f(true);
        }

        // Now PPF should be 1.
        let fsts = FstsReg::from(read32(&mut dev, 0x034));
        assert!(fsts.ppf());

        // Clear F bit via RW1C on FRCD_HI.
        write64(&mut dev, Reg::FRCD_DW2.0, 1u64 << 63);

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
            state.ics = IcsReg::new().with_iwc(true);
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

        // Write IOTLB_REG with IVT=1, IIRG=01 (global).
        let iotlb_val = IotlbReg::new().with_ivt(true).with_iirg(1).into_bits();
        write64(&mut dev, Reg::IOTLB.0, iotlb_val);

        // Read back — IVT should be cleared, IAIG should match IIRG.
        let result = IotlbReg::from(read64(&mut dev, Reg::IOTLB.0));
        assert!(!result.ivt()); // IVT cleared
        assert_eq!(result.iaig(), 1); // IAIG = IIRG = 01
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
            state.frcd_hi = FrcdHi::new().with_f(true);
        }

        // Verify it reads back.
        let frcd_hi = read64(&mut dev, Reg::FRCD_DW2.0);
        assert_eq!((frcd_hi >> 63) & 1, 1);

        // Clear F via RW1C.
        write64(&mut dev, Reg::FRCD_DW2.0, 1u64 << 63);
        let frcd_hi = read64(&mut dev, Reg::FRCD_DW2.0);
        assert_eq!((frcd_hi >> 63) & 1, 0);
    }

    // =========================================================================
    // Translation tests (1E)
    // =========================================================================

    use spec::pte::SlPte;
    use spec::root_context::AddressWidth;
    use spec::root_context::ContextEntry;
    use spec::root_context::ContextEntryHi;
    use spec::root_context::ContextEntryLo;
    use spec::root_context::RootEntry;
    use spec::root_context::RootEntryLo;
    use zerocopy::IntoBytes;

    const ROOT_TABLE_ADDR: u64 = 0x10_0000; // 1 MiB
    const CONTEXT_TABLE_ADDR: u64 = 0x11_0000;
    const PAGE_TABLE_L4_ADDR: u64 = 0x12_0000;
    const PAGE_TABLE_L3_ADDR: u64 = 0x13_0000;
    const PAGE_TABLE_L2_ADDR: u64 = 0x14_0000;
    const PAGE_TABLE_L1_ADDR: u64 = 0x15_0000;
    const TARGET_GPA: u64 = 0x20_0000; // Where IOVA 0 maps to

    /// Create a device with guest memory and pre-populate page tables
    /// for a 4-level walk mapping IOVA 0 → TARGET_GPA.
    fn create_test_device_with_translation() -> (IntelVtdDevice, Arc<VtdSharedState>) {
        // 4 MiB of guest memory.
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);

        // Set up root entry: bus 0 → CONTEXT_TABLE_ADDR.
        let root_entry = RootEntry {
            lo: RootEntryLo::new()
                .with_p(true)
                .with_ctp(CONTEXT_TABLE_ADDR >> 12),
            hi: 0,
        };
        gm.write_at(ROOT_TABLE_ADDR, root_entry.as_bytes()).unwrap();

        // Set up context entry: devfn 0 → page table at L4, AW=2 (48-bit/4-level).
        let context_entry = ContextEntry {
            lo: ContextEntryLo::new()
                .with_p(true)
                .with_tt(TranslationType::UNTRANSLATED_ONLY.0)
                .with_ssptptr(PAGE_TABLE_L4_ADDR >> 12),
            hi: ContextEntryHi::new()
                .with_aw(AddressWidth::AW_48BIT.0)
                .with_did(1), // 48-bit / 4-level
        };
        gm.write_at(CONTEXT_TABLE_ADDR, context_entry.as_bytes())
            .unwrap();

        // Build 4-level page tables: L4 → L3 → L2 → L1 → TARGET_GPA.
        // Level 4 entry 0 → L3
        let pte_l4 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(PAGE_TABLE_L3_ADDR >> 12);
        gm.write_at(PAGE_TABLE_L4_ADDR, pte_l4.into_bits().as_bytes())
            .unwrap();

        // Level 3 entry 0 → L2
        let pte_l3 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(PAGE_TABLE_L2_ADDR >> 12);
        gm.write_at(PAGE_TABLE_L3_ADDR, pte_l3.into_bits().as_bytes())
            .unwrap();

        // Level 2 entry 0 → L1
        let pte_l2 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(PAGE_TABLE_L1_ADDR >> 12);
        gm.write_at(PAGE_TABLE_L2_ADDR, pte_l2.into_bits().as_bytes())
            .unwrap();

        // Level 1 entry 0 → TARGET_GPA (4KB page)
        let pte_l1 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(TARGET_GPA >> 12);
        gm.write_at(PAGE_TABLE_L1_ADDR, pte_l1.into_bits().as_bytes())
            .unwrap();

        let (dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        // Enable translation: set root table, SRTP, TE.
        {
            let mut state = shared.state.write();
            state.rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.latched_rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.gsts = GstsReg::new().with_rtps(true).with_tes(true);
        }

        (dev, shared)
    }

    #[test]
    fn test_translate_4level_4kb() {
        let (_dev, shared) = create_test_device_with_translation();
        let state = shared.state.read();
        let gpa = shared
            .translate_locked(&state, 0, 0, 0x0000, false)
            .unwrap();
        assert_eq!(gpa, TARGET_GPA);
    }

    #[test]
    fn test_translate_4level_4kb_with_offset() {
        let (_dev, shared) = create_test_device_with_translation();
        let state = shared.state.read();
        // IOVA 0x123 → TARGET_GPA + 0x123
        let gpa = shared
            .translate_locked(&state, 0, 0, 0x0123, false)
            .unwrap();
        assert_eq!(gpa, TARGET_GPA + 0x123);
    }

    #[test]
    fn test_translate_disabled_is_identity() {
        let (_dev, shared) = create_test_device_with_translation();
        // Disable translation.
        {
            let mut state = shared.state.write();
            state.gsts = GstsReg::new().with_rtps(true);
        }
        let state = shared.state.read();
        let gpa = shared
            .translate_locked(&state, 0, 0, 0xDEAD_BEEF, false)
            .unwrap();
        assert_eq!(gpa, 0xDEAD_BEEF);
    }

    #[test]
    fn test_translate_3level() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);

        // Set up root entry and context entry with AW=001 (39-bit/3-level).
        let root_entry = RootEntry {
            lo: RootEntryLo::new()
                .with_p(true)
                .with_ctp(CONTEXT_TABLE_ADDR >> 12),
            hi: 0,
        };
        gm.write_at(ROOT_TABLE_ADDR, root_entry.as_bytes()).unwrap();

        let context_entry = ContextEntry {
            lo: ContextEntryLo::new()
                .with_p(true)
                .with_tt(TranslationType::UNTRANSLATED_ONLY.0)
                .with_ssptptr(PAGE_TABLE_L3_ADDR >> 12),
            hi: ContextEntryHi::new()
                .with_aw(AddressWidth::AW_39BIT.0)
                .with_did(1), // 39-bit / 3-level
        };
        gm.write_at(CONTEXT_TABLE_ADDR, context_entry.as_bytes())
            .unwrap();

        // 3-level walk: L3 → L2 → L1 → TARGET_GPA.
        let pte_l3 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(PAGE_TABLE_L2_ADDR >> 12);
        gm.write_at(PAGE_TABLE_L3_ADDR, pte_l3.into_bits().as_bytes())
            .unwrap();

        let pte_l2 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(PAGE_TABLE_L1_ADDR >> 12);
        gm.write_at(PAGE_TABLE_L2_ADDR, pte_l2.into_bits().as_bytes())
            .unwrap();

        let pte_l1 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(TARGET_GPA >> 12);
        gm.write_at(PAGE_TABLE_L1_ADDR, pte_l1.into_bits().as_bytes())
            .unwrap();

        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );
        {
            let mut state = shared.state.write();
            state.rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.latched_rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.gsts = GstsReg::new().with_rtps(true).with_tes(true);
        }

        let state = shared.state.read();
        let gpa = shared
            .translate_locked(&state, 0, 0, 0x0000, false)
            .unwrap();
        assert_eq!(gpa, TARGET_GPA);
    }

    #[test]
    fn test_translate_2mb_large_page() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);

        let root_entry = RootEntry {
            lo: RootEntryLo::new()
                .with_p(true)
                .with_ctp(CONTEXT_TABLE_ADDR >> 12),
            hi: 0,
        };
        gm.write_at(ROOT_TABLE_ADDR, root_entry.as_bytes()).unwrap();

        let context_entry = ContextEntry {
            lo: ContextEntryLo::new()
                .with_p(true)
                .with_tt(TranslationType::UNTRANSLATED_ONLY.0)
                .with_ssptptr(PAGE_TABLE_L4_ADDR >> 12),
            hi: ContextEntryHi::new()
                .with_aw(AddressWidth::AW_48BIT.0)
                .with_did(1),
        };
        gm.write_at(CONTEXT_TABLE_ADDR, context_entry.as_bytes())
            .unwrap();

        // L4 → L3 → L2 (PS=1, 2MB page)
        let pte_l4 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(PAGE_TABLE_L3_ADDR >> 12);
        gm.write_at(PAGE_TABLE_L4_ADDR, pte_l4.into_bits().as_bytes())
            .unwrap();

        let pte_l3 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(PAGE_TABLE_L2_ADDR >> 12);
        gm.write_at(PAGE_TABLE_L3_ADDR, pte_l3.into_bits().as_bytes())
            .unwrap();

        // 2MB page at 0x200000
        let pte_l2 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_ps(true)
            .with_address(0x20_0000 >> 12); // Base GPA = 0x200000
        gm.write_at(PAGE_TABLE_L2_ADDR, pte_l2.into_bits().as_bytes())
            .unwrap();

        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );
        {
            let mut state = shared.state.write();
            state.rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.latched_rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.gsts = GstsReg::new().with_rtps(true).with_tes(true);
        }

        let state = shared.state.read();
        // IOVA 0x1234 within the 2MB page → GPA 0x201234.
        let gpa = shared
            .translate_locked(&state, 0, 0, 0x1234, false)
            .unwrap();
        assert_eq!(gpa, 0x20_1234);
    }

    #[test]
    fn test_translate_passthrough() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);

        let root_entry = RootEntry {
            lo: RootEntryLo::new()
                .with_p(true)
                .with_ctp(CONTEXT_TABLE_ADDR >> 12),
            hi: 0,
        };
        gm.write_at(ROOT_TABLE_ADDR, root_entry.as_bytes()).unwrap();

        // Context entry with TT=10 (pass-through).
        let context_entry = ContextEntry {
            lo: ContextEntryLo::new()
                .with_p(true)
                .with_tt(TranslationType::PASS_THROUGH.0)
                .with_ssptptr(0),
            hi: ContextEntryHi::new()
                .with_aw(AddressWidth::AW_48BIT.0)
                .with_did(1),
        };
        gm.write_at(CONTEXT_TABLE_ADDR, context_entry.as_bytes())
            .unwrap();

        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );
        {
            let mut state = shared.state.write();
            state.rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.latched_rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.gsts = GstsReg::new().with_rtps(true).with_tes(true);
        }

        let state = shared.state.read();
        let gpa = shared
            .translate_locked(&state, 0, 0, 0xABCD_0000, false)
            .unwrap();
        assert_eq!(gpa, 0xABCD_0000);
    }

    #[test]
    fn test_translate_root_not_present() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);

        // Root entry with P=0.
        let root_entry = RootEntry {
            lo: RootEntryLo::new().with_p(false),
            hi: 0,
        };
        gm.write_at(ROOT_TABLE_ADDR, root_entry.as_bytes()).unwrap();

        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );
        {
            let mut state = shared.state.write();
            state.rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.latched_rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.gsts = GstsReg::new().with_rtps(true).with_tes(true);
        }

        let state = shared.state.read();
        let err = shared
            .translate_locked(&state, 0, 0, 0x1000, false)
            .unwrap_err();
        assert!(matches!(err, VtdFault::RootNotPresent { .. }));
    }

    #[test]
    fn test_translate_context_not_present() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);

        let root_entry = RootEntry {
            lo: RootEntryLo::new()
                .with_p(true)
                .with_ctp(CONTEXT_TABLE_ADDR >> 12),
            hi: 0,
        };
        gm.write_at(ROOT_TABLE_ADDR, root_entry.as_bytes()).unwrap();

        // Context entry with P=0.
        let context_entry = ContextEntry {
            lo: ContextEntryLo::new().with_p(false),
            hi: ContextEntryHi::new(),
        };
        gm.write_at(CONTEXT_TABLE_ADDR, context_entry.as_bytes())
            .unwrap();

        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );
        {
            let mut state = shared.state.write();
            state.rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.latched_rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.gsts = GstsReg::new().with_rtps(true).with_tes(true);
        }

        let state = shared.state.read();
        let err = shared
            .translate_locked(&state, 0, 0, 0x1000, false)
            .unwrap_err();
        assert!(matches!(err, VtdFault::ContextNotPresent { .. }));
    }

    #[test]
    fn test_translate_pte_not_present() {
        let (_dev, shared) = create_test_device_with_translation();

        // Write a non-present PTE at L1 entry 1 (IOVA 0x1000).
        let pte = SlPte::new(); // R=W=0
        shared
            .guest_memory
            .write_at(PAGE_TABLE_L1_ADDR + 8, pte.into_bits().as_bytes())
            .unwrap();

        let state = shared.state.read();
        let err = shared
            .translate_locked(&state, 0, 0, 0x1000, false)
            .unwrap_err();
        // Non-present PTE (R=W=0) on a read → AccessDenied with write=false (fault 0x06).
        assert!(matches!(err, VtdFault::AccessDenied { write: false, .. }));
    }

    #[test]
    fn test_translate_write_to_readonly() {
        let (_dev, shared) = create_test_device_with_translation();

        // Write a read-only PTE at L1 entry 0 (R=1, W=0).
        let pte = SlPte::new().with_r(true).with_w(false);
        shared
            .guest_memory
            .write_at(PAGE_TABLE_L1_ADDR, pte.into_bits().as_bytes())
            .unwrap();

        let state = shared.state.read();
        // Read should succeed.
        let gpa = shared
            .translate_locked(&state, 0, 0, 0x0000, false)
            .unwrap();
        assert_eq!(gpa, 0); // PTE address field is 0
        // Write should fail.
        let err = shared
            .translate_locked(&state, 0, 0, 0x0000, true)
            .unwrap_err();
        assert!(matches!(err, VtdFault::AccessDenied { write: true, .. }));
    }

    #[test]
    fn test_translate_pde_permission_restricts() {
        let (_dev, shared) = create_test_device_with_translation();

        // Make L2 entry 0 read-only (R=1, W=0), L1 entry 0 has W=1.
        // AND-accumulation means write should fail.
        let pte_l2 = SlPte::new()
            .with_r(true)
            .with_w(false) // No write at L2
            .with_address(PAGE_TABLE_L1_ADDR >> 12);
        shared
            .guest_memory
            .write_at(PAGE_TABLE_L2_ADDR, pte_l2.into_bits().as_bytes())
            .unwrap();

        let state = shared.state.read();
        let err = shared
            .translate_locked(&state, 0, 0, 0x0000, true)
            .unwrap_err();
        assert!(matches!(err, VtdFault::AccessDenied { write: true, .. }));
    }

    #[test]
    fn test_translate_fpd_suppresses_fault() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);

        let root_entry = RootEntry {
            lo: RootEntryLo::new()
                .with_p(true)
                .with_ctp(CONTEXT_TABLE_ADDR >> 12),
            hi: 0,
        };
        gm.write_at(ROOT_TABLE_ADDR, root_entry.as_bytes()).unwrap();

        // Context entry with FPD=1, pointing to a non-existent page table.
        let context_entry = ContextEntry {
            lo: ContextEntryLo::new()
                .with_p(true)
                .with_fpd(true)
                .with_tt(TranslationType::UNTRANSLATED_ONLY.0)
                .with_ssptptr(0x30_0000 >> 12), // Points to zeroed memory
            hi: ContextEntryHi::new()
                .with_aw(AddressWidth::AW_48BIT.0)
                .with_did(1),
        };
        gm.write_at(CONTEXT_TABLE_ADDR, context_entry.as_bytes())
            .unwrap();

        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );
        {
            let mut state = shared.state.write();
            state.rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.latched_rtaddr = RtaddrReg::from(ROOT_TABLE_ADDR);
            state.gsts = GstsReg::new().with_rtps(true).with_tes(true);
        }

        // Translation should fail (PTE not present in zeroed memory).
        let state = shared.state.read();
        let err = shared
            .translate_locked(&state, 0, 0, 0x0000, false)
            .unwrap_err();
        assert!(err.fpd()); // FPD should be true

        // Record the fault — it should be suppressed by FPD.
        drop(state);
        err.record(&shared, false); // read access

        // FRCD.F should NOT be set (fault suppressed).
        let state = shared.state.read();
        assert!(!state.frcd_hi.f());
    }

    // =========================================================================
    // Interrupt remapping tests (1F)
    // =========================================================================

    use spec::irte::Irte;
    use spec::irte::IrteHi;
    use spec::irte::IrteLo;

    const IRT_BASE_ADDR: u64 = 0x18_0000;

    #[test]
    fn test_extract_irte_index_no_shv() {
        // Handle = {addr[2], addr[19:5]}. SHV = addr[3] = 0.
        // addr = 0xFEE0_0010 (bit 4=1, remappable format, handle=0x0000).
        let index = VtdSharedState::extract_irte_index(0xFEE0_0010, 0);
        assert_eq!(index, 0);

        // addr[19:5] = 0x7 (handle[14:0] = 7), addr[2] = 0 (handle[15]=0).
        // addr = 0xFEE0_00F0 (bits 7:5 = 111).
        let addr = 0xFEE0_0010 | (0x7 << 5);
        let index = VtdSharedState::extract_irte_index(addr, 0);
        assert_eq!(index, 7);
    }

    #[test]
    fn test_extract_irte_index_with_shv() {
        // SHV = addr[3] = 1. handle = {addr[2], addr[19:5]}.
        // index = handle + data[15:0].
        let addr = 0xFEE0_0018 | (0x5 << 5); // handle=5, SHV=1
        let index = VtdSharedState::extract_irte_index(addr, 3);
        assert_eq!(index, 8); // 5 + 3
    }

    fn setup_ir_state(shared: &Arc<VtdSharedState>, eime: bool) {
        let mut state = shared.state.write();
        // Set up IRT: base at IRT_BASE_ADDR, size=0xF (65536 entries).
        state.irta = IrtaReg::new()
            .with_irta(IRT_BASE_ADDR >> 12)
            .with_s(0xF)
            .with_eime(eime);
        state.latched_irta = state.irta;
        // Enable IR.
        let mut gsts = state.gsts;
        gsts.set_ires(true);
        gsts.set_irtps(true);
        state.gsts = gsts;
    }

    struct CountingRoute {
        device_id: u16,
        retranslate_count: AtomicU32,
    }

    impl iommu_common::RetranslateInterrupts for CountingRoute {
        fn device_id(&self) -> u16 {
            self.device_id
        }

        fn retranslate(&self) {
            self.retranslate_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn test_remap_msi_basic() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        setup_ir_state(&shared, false);

        // Write IRTE at index 0: vector=0x42, DST xAPIC ID=3, fixed delivery.
        let irte = Irte {
            lo: IrteLo::new()
                .with_p(true)
                .with_vector(0x42)
                .with_dst(0x0300) // xAPIC: APIC ID in bits 15:8
                .with_dlm(0) // Fixed
                .with_dm(false), // Physical
            hi: IrteHi::new().with_svt(0), // No source validation
        };
        gm.write_at(IRT_BASE_ADDR, irte.as_bytes()).unwrap();

        // Remappable MSI: addr bit4=1, handle=0, SHV=0 → IRTE index 0.
        let addr = 0xFEE0_0010u64;
        let data = 0u32;
        let state = shared.state.read();
        let (new_addr, new_data) = shared.remap_msi_locked(&state, 0x0000, addr, data).unwrap();

        // Expected: addr = 0xFEE03000 (DST=3 in bits 19:12).
        assert_eq!(new_addr & 0xFFF0_0000, 0xFEE0_0000);
        assert_eq!((new_addr >> 12) & 0xFF, 3); // APIC ID = 3
        assert_eq!(new_data & 0xFF, 0x42); // Vector = 0x42
    }

    #[test]
    fn test_remap_msi_x2apic_destination() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        // Enable IR with EIME=1 (x2APIC mode).
        setup_ir_state(&shared, true);

        // Write IRTE with destination APIC ID = 300 (> 255).
        let apic_id: u32 = 300;
        let irte = Irte {
            lo: IrteLo::new()
                .with_p(true)
                .with_vector(0x50)
                .with_dst(apic_id)
                .with_dlm(0)
                .with_dm(false),
            hi: IrteHi::new().with_svt(0),
        };
        gm.write_at(IRT_BASE_ADDR, irte.as_bytes()).unwrap();

        let addr = 0xFEE0_0010u64;
        let state = shared.state.read();
        let (new_addr, new_data) = shared.remap_msi_locked(&state, 0x0000, addr, 0).unwrap();

        // Verify the destination is preserved via virt_destination encoding.
        let msi_addr = x86defs::msi::MsiAddress::from(new_addr as u32);
        assert_eq!(msi_addr.virt_destination(), apic_id as u16);
        assert_eq!(new_data & 0xFF, 0x50);
    }

    #[test]
    fn test_remap_msi_ir_disabled_passthrough() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );
        // IR not enabled — pass through.
        let state = shared.state.read();
        let (a, d) = shared
            .remap_msi_locked(&state, 0, 0xFEE0_1234, 0x42)
            .unwrap();
        assert_eq!(a, 0xFEE0_1234);
        assert_eq!(d, 0x42);
    }

    #[test]
    fn test_interrupt_remapper_transitions_from_passthrough_to_remap() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        let compat_addr = 0xFEE0_0000u64;
        assert_eq!(
            iommu_common::InterruptRemapper::remap_msi(&*shared, 0x00A0, compat_addr, 0x31),
            Some((compat_addr, 0x31))
        );

        setup_ir_state(&shared, false);
        let irte = Irte {
            lo: IrteLo::new()
                .with_p(true)
                .with_vector(0x45)
                .with_dst(0x0200)
                .with_dlm(0)
                .with_dm(false),
            hi: IrteHi::new().with_svt(0b01).with_sq(0).with_sid(0x00A0),
        };
        gm.write_at(IRT_BASE_ADDR, irte.as_bytes()).unwrap();

        let (new_addr, new_data) =
            iommu_common::InterruptRemapper::remap_msi(&*shared, 0x00A0, 0xFEE0_0010, 0).unwrap();
        assert_eq!((new_addr >> 12) & 0xFF, 2);
        assert_eq!(new_data & 0xFF, 0x45);
    }

    #[test]
    fn test_remap_msi_irte_not_present() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        setup_ir_state(&shared, false);

        // IRTE at index 0 is all zeros (P=0).
        let addr = 0xFEE0_0010u64;
        let state = shared.state.read();
        let err = shared
            .remap_msi_locked(&state, 0x0000, addr, 0)
            .unwrap_err();
        assert!(matches!(err, VtdFault::IrteNotPresent { .. }));
    }

    #[test]
    fn test_remap_msi_posted_irte_rejected() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        setup_ir_state(&shared, false);

        let irte = Irte {
            lo: IrteLo::new().with_p(true).with_im(true).with_vector(0x40),
            hi: IrteHi::new().with_svt(0),
        };
        gm.write_at(IRT_BASE_ADDR, irte.as_bytes()).unwrap();

        let state = shared.state.read();
        let err = shared
            .remap_msi_locked(&state, 0x0000, 0xFEE0_0010, 0)
            .unwrap_err();
        assert!(matches!(err, VtdFault::IrteReservedField { .. }));
    }

    #[test]
    fn test_interrupt_entry_cache_invalidate_retranslates_routes() {
        const IQ_BASE: u64 = 0x20_0000;

        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (mut dev, shared) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        let route = Arc::new(CountingRoute {
            device_id: 0x00A0,
            retranslate_count: AtomicU32::new(0),
        });
        let route_dyn: Arc<dyn iommu_common::RetranslateInterrupts> = route.clone();
        iommu_common::InterruptRemapper::register_route(&*shared, &route_dyn);

        let iqa = IqaReg::new().with_qs(0).with_iqa(IQ_BASE >> 12);
        write64(&mut dev, Reg::IQA.0, iqa.into_bits());
        write32(
            &mut dev,
            Reg::GCMD.0,
            GcmdReg::new().with_qie(true).into_bits(),
        );

        let desc = spec::invalidation::InvalidationDescriptor {
            dw0: DescriptorType::INTERRUPT_ENTRY_CACHE_INVALIDATE.0 as u32,
            dw1: 0,
            dw2: 0,
            dw3: 0,
        };
        gm.write_at(IQ_BASE, desc.as_bytes()).unwrap();

        write64(&mut dev, Reg::IQT.0, IqtReg::new().with_qt(1).into_bits());

        assert_eq!(route.retranslate_count.load(Ordering::SeqCst), 1);
        assert_eq!(IqhReg::from(read64(&mut dev, Reg::IQH.0)).head_offset(), 16);
    }

    #[test]
    fn test_remap_msi_compat_format_blocked() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        setup_ir_state(&shared, false);
        // CFIS=0 (default) → compat-format blocked.

        // Compatibility format: bit 4 = 0.
        let addr = 0xFEE0_0000u64; // bit 4 = 0
        let state = shared.state.read();
        let err = shared
            .remap_msi_locked(&state, 0x0000, addr, 0)
            .unwrap_err();
        assert!(matches!(err, VtdFault::CompatibilityFormatBlocked { .. }));
    }

    #[test]
    fn test_remap_msi_compat_format_passthrough_cfis() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        setup_ir_state(&shared, false);

        // Set CFIS=1 → pass-through for compat-format.
        {
            let mut state = shared.state.write();
            let mut gsts = state.gsts;
            gsts.set_cfis(true);
            state.gsts = gsts;
        }

        let addr = 0xFEE0_0000u64; // bit 4 = 0
        let state = shared.state.read();
        let (a, d) = shared.remap_msi_locked(&state, 0x0000, addr, 0x42).unwrap();
        assert_eq!(a, addr);
        assert_eq!(d, 0x42);
    }

    #[test]
    fn test_remap_msi_source_validation_verify_sid() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        setup_ir_state(&shared, false);

        // IRTE with SVT=01 (verify SID), SQ=00 (exact match), SID=0x0100.
        let irte = Irte {
            lo: IrteLo::new()
                .with_p(true)
                .with_vector(0x30)
                .with_dst(0x0100)
                .with_dlm(0)
                .with_dm(false),
            hi: IrteHi::new().with_svt(0b01).with_sq(0b00).with_sid(0x0100),
        };
        gm.write_at(IRT_BASE_ADDR, irte.as_bytes()).unwrap();

        let addr = 0xFEE0_0010u64;
        let state = shared.state.read();

        // Matching SID should succeed.
        assert!(shared.remap_msi_locked(&state, 0x0100, addr, 0).is_ok());

        // Non-matching SID should fail.
        let err = shared
            .remap_msi_locked(&state, 0x0200, addr, 0)
            .unwrap_err();
        assert!(matches!(err, VtdFault::SourceValidationFailed { .. }));
    }

    #[test]
    fn test_record_fault() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        let fault = VtdFault::RootNotPresent {
            source_id: 0x0100,
            iova: 0x1000,
        };
        fault.record(&shared, false); // simulate a read fault

        let state = shared.state.read();
        let frcd_hi = state.frcd_hi;
        assert!(frcd_hi.f());
        assert_eq!(frcd_hi.sid(), 0x0100);
        assert_eq!(frcd_hi.fr(), FaultReason::ROOT_NOT_PRESENT.0);
        assert!(frcd_hi.t()); // T=1 for read

        let frcd_lo = state.frcd_lo;
        assert_eq!(frcd_lo.fault_address(), 0x1000);
    }

    #[test]
    fn test_record_fault_overflow() {
        let gm = GuestMemory::allocate(0x40_0000);
        let signal_msi = Arc::new(TestSignalMsi);
        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        // Record first fault.
        let fault1 = VtdFault::RootNotPresent {
            source_id: 0x0100,
            iova: 0x1000,
        };
        fault1.record(&shared, true); // write fault

        // Record second fault — should overflow.
        let fault2 = VtdFault::ContextNotPresent {
            source_id: 0x0200,
            iova: 0x2000,
        };
        fault2.record(&shared, false);

        let state = shared.state.read();
        // PFO should be set.
        let fsts = state.fsts;
        assert!(fsts.pfo());
        // FRCD should still have first fault.
        let frcd_hi = state.frcd_hi;
        assert_eq!(frcd_hi.sid(), 0x0100);
    }

    #[test]
    fn test_vtd_translator() {
        let (_dev, shared) = create_test_device_with_translation();
        let translator = shared.translator();

        // Test via IommuTranslator trait.
        let result = iommu_common::IommuTranslator::translate(
            &translator,
            0x0000, // RID: bus=0, devfn=0
            0x0000, // IOVA
            false,
            |gpa| gpa,
        )
        .unwrap();
        assert_eq!(result, TARGET_GPA);
    }

    #[test]
    fn test_vtd_signal_msi() {
        use std::sync::atomic::{AtomicU64, Ordering};

        struct RecordingMsi {
            last_addr: AtomicU64,
        }
        impl SignalMsi for RecordingMsi {
            fn signal_msi(&self, _devid: Option<u32>, address: u64, _data: u32) {
                self.last_addr.store(address, Ordering::SeqCst);
            }
        }

        let gm = GuestMemory::allocate(0x40_0000);
        let inner_msi = Arc::new(RecordingMsi {
            last_addr: AtomicU64::new(0),
        });
        let signal_msi = Arc::new(TestSignalMsi);

        let (_dev, shared) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        // No IR enabled → pass-through.
        let wrapper = shared.wrap_signal_msi(inner_msi.clone());
        wrapper.signal_msi(Some(0x0100), 0xFEE0_1234, 0x42);
        assert_eq!(inner_msi.last_addr.load(Ordering::SeqCst), 0xFEE0_1234);
    }

    #[test]
    fn test_vtd_signal_msi_no_devid_dropped() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingMsi {
            count: AtomicU32,
        }
        impl SignalMsi for CountingMsi {
            fn signal_msi(&self, _devid: Option<u32>, _address: u64, _data: u32) {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let gm = GuestMemory::allocate(0x40_0000);
        let inner_msi = Arc::new(CountingMsi {
            count: AtomicU32::new(0),
        });
        let signal_msi = Arc::new(TestSignalMsi);

        let (_dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            signal_msi,
        );

        let wrapper = shared.wrap_signal_msi(inner_msi.clone());

        // devid=None should be silently dropped.
        wrapper.signal_msi(None, 0xFEE0_1234, 0x42);
        assert_eq!(inner_msi.count.load(Ordering::SeqCst), 0);
    }

    // =========================================================================
    // End-to-end integration test (1J.1)
    // =========================================================================
    //
    // This test constructs the full VT-d stack and programs it via MMIO,
    // mimicking a Linux `intel-iommu` driver init sequence.

    use spec::invalidation::InvalidationWaitDw0Dw1;
    use spec::invalidation::InvalidationWaitDw2Dw3;

    /// Guest memory layout for the end-to-end test.
    ///
    /// 8 MiB of guest memory. Addresses chosen to be well-separated.
    const E2E_MEM_SIZE: usize = 0x80_0000;
    const E2E_ROOT_TABLE: u64 = 0x10_0000; // 1 MiB
    const E2E_CONTEXT_TABLE: u64 = 0x11_0000; // 1 MiB + 64 KiB
    const E2E_PT_L4: u64 = 0x12_0000;
    const E2E_PT_L3: u64 = 0x13_0000;
    const E2E_PT_L2: u64 = 0x14_0000;
    const E2E_PT_L1: u64 = 0x15_0000;
    const E2E_TARGET_GPA: u64 = 0x20_0000; // 2 MiB — where IOVA 0 maps to
    const E2E_IQ_BASE: u64 = 0x30_0000; // Invalidation queue
    const E2E_IQ_STATUS: u64 = 0x31_0000; // Status write address for INVALIDATION_WAIT
    const E2E_IRT_BASE: u64 = 0x40_0000; // Interrupt remapping table
    /// Test device BDF: bus=1, dev=0, fn=0 → devfn=0x00, RID=0x0100.
    const E2E_TEST_BUS: u8 = 1;
    const E2E_TEST_DEVFN: u8 = 0x00;
    const E2E_TEST_RID: u16 = 0x0100;

    /// End-to-end test mimicking a Linux intel-iommu driver init sequence.
    ///
    /// Steps:
    ///  1. Read CAP/ECAP, verify capabilities.
    ///  2. Allocate root table, context tables, page tables in guest memory.
    ///  3. Write RTADDR with root table GPA.
    ///  4. Write GCMD with SRTP=1, verify RTPS in GSTS.
    ///  5. Register-based invalidation (CCMD, IOTLB) before QI.
    ///  6. Allocate invalidation queue, write IQA.
    ///  7. Write GCMD with QIE=1, verify QIES.
    ///  8. Program context entries for test devices.
    ///  9. Build 4-level page table mapping IOVA 0x0 → E2E_TARGET_GPA.
    /// 10. Write GCMD with TE=1, verify TES.
    /// 11. Submit invalidation queue descriptors (context + IOTLB + wait).
    /// 12. DMA read/write via VtdTranslator, verify translation.
    /// 13. Allocate IRT, program IRTE entries.
    /// 14. Write IRTA with IRT base, GCMD with SIRTP=1 then IRE=1.
    /// 15. MSI via VtdSignalMsi, verify remapping.
    /// 16. Access unmapped IOVA, verify fault recording.
    #[test]
    fn test_end_to_end_linux_init_sequence() {
        use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

        // -- Recording MSI sink to verify interrupt delivery --
        struct RecordingMsi {
            last_addr: AtomicU64,
            last_data: AtomicU32,
            count: AtomicU32,
        }
        impl RecordingMsi {
            fn new() -> Self {
                Self {
                    last_addr: AtomicU64::new(0),
                    last_data: AtomicU32::new(0),
                    count: AtomicU32::new(0),
                }
            }
        }
        impl SignalMsi for RecordingMsi {
            fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
                self.last_addr.store(address, Ordering::SeqCst);
                self.last_data.store(data, Ordering::SeqCst);
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let gm = GuestMemory::allocate(E2E_MEM_SIZE);
        let iommu_msi = Arc::new(RecordingMsi::new());
        let (mut dev, shared) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            iommu_msi.clone(),
        );

        // =====================================================================
        // Step 1: Read and verify capabilities
        // =====================================================================
        let ver = read32(&mut dev, 0x000);
        assert_eq!(VersionReg::from(ver).max(), 1);
        assert_eq!(VersionReg::from(ver).min(), 0);

        let cap = read64(&mut dev, 0x008);
        let cap_reg = CapReg::from(cap);
        assert_eq!(cap_reg.mgaw(), 47); // 48-bit
        assert!(cap_reg.sagaw() & 0x4 != 0); // 48-bit/4-level supported
        assert!(cap_reg.sagaw() & 0x2 != 0); // 39-bit/3-level supported
        assert_eq!(cap_reg.nfr(), 0); // 1 fault record

        let ecap = read64(&mut dev, 0x010);
        let ecap_reg = EcapReg::from(ecap);
        assert!(ecap_reg.qi()); // Queued invalidation
        assert!(ecap_reg.ir()); // Interrupt remapping
        assert!(ecap_reg.eim()); // x2APIC
        assert!(ecap_reg.c()); // Page-walk coherency

        // Initial GSTS should be all zeros.
        assert_eq!(read32(&mut dev, 0x01C), 0);

        // =====================================================================
        // Step 2–4: Set root table pointer
        // =====================================================================

        // Write root table, context table entries, and page tables into guest
        // memory for bus 1, devfn 0.

        // Root table entry for bus 1 → context table.
        let root_entry = RootEntry {
            lo: RootEntryLo::new()
                .with_p(true)
                .with_ctp(E2E_CONTEXT_TABLE >> 12),
            hi: 0,
        };
        gm.write_at(
            E2E_ROOT_TABLE + (E2E_TEST_BUS as u64) * 16,
            root_entry.as_bytes(),
        )
        .unwrap();

        // Context entry for devfn 0 → 4-level page table.
        let context_entry = ContextEntry {
            lo: ContextEntryLo::new()
                .with_p(true)
                .with_tt(TranslationType::UNTRANSLATED_ONLY.0)
                .with_ssptptr(E2E_PT_L4 >> 12),
            hi: ContextEntryHi::new()
                .with_aw(AddressWidth::AW_48BIT.0)
                .with_did(1), // 48-bit/4-level
        };
        gm.write_at(
            E2E_CONTEXT_TABLE + (E2E_TEST_DEVFN as u64) * 16,
            context_entry.as_bytes(),
        )
        .unwrap();

        // Build 4-level page tables: L4 → L3 → L2 → L1 → TARGET_GPA.
        let pte_l4 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(E2E_PT_L3 >> 12);
        gm.write_at(E2E_PT_L4, pte_l4.into_bits().as_bytes())
            .unwrap();

        let pte_l3 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(E2E_PT_L2 >> 12);
        gm.write_at(E2E_PT_L3, pte_l3.into_bits().as_bytes())
            .unwrap();

        let pte_l2 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(E2E_PT_L1 >> 12);
        gm.write_at(E2E_PT_L2, pte_l2.into_bits().as_bytes())
            .unwrap();

        let pte_l1 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_address(E2E_TARGET_GPA >> 12);
        gm.write_at(E2E_PT_L1, pte_l1.into_bits().as_bytes())
            .unwrap();

        // Write RTADDR register.
        write64(&mut dev, 0x020, E2E_ROOT_TABLE);
        assert_eq!(read64(&mut dev, 0x020), E2E_ROOT_TABLE);

        // GCMD: SRTP (set root table pointer).
        write32(&mut dev, 0x018, GcmdReg::new().with_srtp(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.rtps(), "RTPS must be set after SRTP");

        // =====================================================================
        // Step 5: Register-based invalidation (pre-QI, like Linux early init)
        // =====================================================================

        // Context-cache invalidation: global.
        let ccmd = CcmdReg::new().with_icc(true).with_cirg(1); // global
        write64(&mut dev, 0x028, ccmd.into_bits());
        let ccmd_result = CcmdReg::from(read64(&mut dev, 0x028));
        assert!(!ccmd_result.icc(), "ICC must be cleared after invalidation");
        assert_eq!(ccmd_result.caig(), 1, "CAIG must echo CIRG");

        // IOTLB invalidation: global.
        let iotlb_val = IotlbReg::new().with_ivt(true).with_iirg(1).into_bits();
        write64(&mut dev, Reg::IOTLB.0, iotlb_val);
        let iotlb_result = IotlbReg::from(read64(&mut dev, Reg::IOTLB.0));
        assert!(!iotlb_result.ivt(), "IVT must be cleared");
        assert_eq!(iotlb_result.iaig(), 1, "IAIG must echo IIRG");

        // =====================================================================
        // Step 6–7: Enable queued invalidation
        // =====================================================================

        // Write IQA: base address, QS=0 (256 entries, 4096 bytes).
        let iqa = IqaReg::new().with_qs(0).with_iqa(E2E_IQ_BASE >> 12);
        write64(&mut dev, 0x090, iqa.into_bits());
        assert_eq!(read64(&mut dev, 0x090), iqa.into_bits());

        // Enable QI.
        write32(&mut dev, 0x018, GcmdReg::new().with_qie(true).into_bits());
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.qies(), "QIES must be set after QIE");

        // Verify IQH starts at 0.
        assert_eq!(read64(&mut dev, 0x080), 0, "IQH must start at 0");

        // =====================================================================
        // Step 10: Enable translation
        // =====================================================================

        // GCMD is a write-only register where toggle bits are compared against
        // GSTS. Must include the current state of all toggle bits (QIE=1) plus
        // the new bit (TE=1) to avoid inadvertently disabling QI.
        write32(
            &mut dev,
            0x018,
            GcmdReg::new().with_te(true).with_qie(true).into_bits(),
        );
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.tes(), "TES must be set after TE");
        assert!(gsts.qies(), "QIES must remain set");

        // =====================================================================
        // Step 11: Submit invalidation queue descriptors
        // =====================================================================

        // Write 3 descriptors: context-cache invalidate, IOTLB invalidate,
        // invalidation-wait with status write.

        // Descriptor 0: context-cache invalidation (global, type=0x01).
        let desc0 = spec::invalidation::InvalidationDescriptor {
            dw0: DescriptorType::CONTEXT_CACHE_INVALIDATE.0 as u32 | (1u32 << 4), // global granularity
            dw1: 0,
            dw2: 0,
            dw3: 0,
        };
        gm.write_at(E2E_IQ_BASE, desc0.as_bytes()).unwrap();

        // Descriptor 1: IOTLB invalidation (global, type=0x02).
        let desc1 = spec::invalidation::InvalidationDescriptor {
            dw0: DescriptorType::IOTLB_INVALIDATE.0 as u32 | (1u32 << 4), // global
            dw1: 0,
            dw2: 0,
            dw3: 0,
        };
        gm.write_at(E2E_IQ_BASE + 16, desc1.as_bytes()).unwrap();

        // Descriptor 2: invalidation-wait with status write (type=0x05).
        let iw_lo = InvalidationWaitDw0Dw1::new()
            .with_desc_type(DescriptorType::INVALIDATION_WAIT.0)
            .with_sw(true)
            .with_status_data(0xDEAD_BEEF);
        let iw_hi = InvalidationWaitDw2Dw3::new().with_sal(E2E_IQ_STATUS >> 2);
        let desc2 = spec::invalidation::InvalidationDescriptor {
            dw0: iw_lo.into_bits() as u32,
            dw1: (iw_lo.into_bits() >> 32) as u32,
            dw2: iw_hi.into_bits() as u32,
            dw3: (iw_hi.into_bits() >> 32) as u32,
        };
        gm.write_at(E2E_IQ_BASE + 32, desc2.as_bytes()).unwrap();

        // Clear status location.
        gm.write_at(E2E_IQ_STATUS, &0u32.to_le_bytes()).unwrap();

        // Write IQT: 3 descriptors × 16 bytes = 48 bytes → tail offset 48.
        let iqt = IqtReg::new().with_qt(3); // 3 * 16 = 48 byte offset
        write64(&mut dev, 0x088, iqt.into_bits());

        // Verify head advanced to tail.
        let iqh = IqhReg::from(read64(&mut dev, 0x080));
        assert_eq!(
            iqh.head_offset(),
            48,
            "IQH must advance to tail after processing"
        );

        // Verify status data was written by INVALIDATION_WAIT.
        let status: u32 = gm.read_plain(E2E_IQ_STATUS).unwrap();
        assert_eq!(
            status, 0xDEAD_BEEF,
            "INVALIDATION_WAIT must write status data"
        );

        // =====================================================================
        // Step 12: DMA translation via VtdTranslator
        // =====================================================================
        let translator = shared.translator();

        // Translate IOVA 0x0 → should map to E2E_TARGET_GPA.
        let gpa = iommu_common::IommuTranslator::translate(
            &translator,
            E2E_TEST_RID,
            0x0000,
            false, // read
            |gpa| gpa,
        )
        .unwrap();
        assert_eq!(gpa, E2E_TARGET_GPA, "IOVA 0x0 must map to target GPA");

        // Translate IOVA 0x0 with write.
        let gpa = iommu_common::IommuTranslator::translate(
            &translator,
            E2E_TEST_RID,
            0x0000,
            true, // write
            |gpa| gpa,
        )
        .unwrap();
        assert_eq!(gpa, E2E_TARGET_GPA, "IOVA 0x0 write must map to target GPA");

        // Translate with page offset.
        let gpa = iommu_common::IommuTranslator::translate(
            &translator,
            E2E_TEST_RID,
            0x0ABC,
            false,
            |gpa| gpa,
        )
        .unwrap();
        assert_eq!(
            gpa,
            E2E_TARGET_GPA + 0xABC,
            "IOVA page offset must be preserved"
        );

        // =====================================================================
        // Step 13–14: Set up interrupt remapping
        // =====================================================================

        // Program IRTE at index 0: vector=0x42, DST=5 (xAPIC), fixed delivery.
        let irte = Irte {
            lo: IrteLo::new()
                .with_p(true)
                .with_vector(0x42)
                .with_dst(0x0500) // xAPIC: APIC ID in bits 15:8
                .with_dlm(0) // Fixed delivery
                .with_dm(false), // Physical
            hi: IrteHi::new()
                .with_svt(0b01) // Verify SID
                .with_sq(0b00) // Exact match
                .with_sid(E2E_TEST_RID),
        };
        gm.write_at(E2E_IRT_BASE, irte.as_bytes()).unwrap();

        // Write IRTA: base at E2E_IRT_BASE, S=0 (2 entries), EIME=0.
        let irta = IrtaReg::new()
            .with_irta(E2E_IRT_BASE >> 12)
            .with_s(0) // 2 entries
            .with_eime(false);
        write64(&mut dev, 0x0B8, irta.into_bits());

        // GCMD: SIRTP (set IRT pointer). Must preserve toggle bits.
        write32(
            &mut dev,
            0x018,
            GcmdReg::new()
                .with_sirtp(true)
                .with_te(true)
                .with_qie(true)
                .into_bits(),
        );
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.irtps(), "IRTPS must be set after SIRTP");

        // GCMD: IRE (enable interrupt remapping). Must preserve toggle bits.
        write32(
            &mut dev,
            0x018,
            GcmdReg::new()
                .with_ire(true)
                .with_te(true)
                .with_qie(true)
                .into_bits(),
        );
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(gsts.ires(), "IRES must be set after IRE");

        // =====================================================================
        // Step 15: MSI remapping via VtdSignalMsi
        // =====================================================================
        let inner_msi = Arc::new(RecordingMsi::new());
        let wrapper = shared.wrap_signal_msi(inner_msi.clone());

        // Send remappable-format MSI: addr bit4=1, handle=0, SHV=0 → IRTE index 0.
        let msi_addr = 0xFEE0_0010u64;
        let msi_data = 0u32;
        wrapper.signal_msi(Some(E2E_TEST_RID as u32), msi_addr, msi_data);

        // Verify remapped MSI was delivered.
        assert_eq!(
            inner_msi.count.load(Ordering::SeqCst),
            1,
            "Remapped MSI must be delivered"
        );
        let delivered_addr = inner_msi.last_addr.load(Ordering::SeqCst);
        let delivered_data = inner_msi.last_data.load(Ordering::SeqCst);
        // Destination should be APIC ID 5 in bits 19:12.
        assert_eq!(
            (delivered_addr >> 12) & 0xFF,
            5,
            "Remapped MSI dest must be APIC ID 5"
        );
        // Vector should be 0x42.
        assert_eq!(
            delivered_data & 0xFF,
            0x42,
            "Remapped MSI vector must be 0x42"
        );

        // =====================================================================
        // Step 16: Fault recording — unmapped IOVA
        // =====================================================================

        // IOVA 0x1000 maps to L1 entry index 1, which is not populated (zeros).
        let fault_result = iommu_common::IommuTranslator::translate(
            &translator,
            E2E_TEST_RID,
            0x1000,
            false,
            |gpa| gpa,
        );
        assert!(
            fault_result.is_err(),
            "Unmapped IOVA must produce a translation fault"
        );

        // Verify fault was recorded in FRCD.
        let frcd_hi = read64(&mut dev, Reg::FRCD_DW2.0);
        let frcd = FrcdHi::from(frcd_hi);
        assert!(frcd.f(), "Fault must be recorded (F=1)");
        assert_eq!(
            frcd.sid(),
            E2E_TEST_RID,
            "Fault source ID must match test device"
        );

        // Verify FSTS.PPF is set.
        let fsts = FstsReg::from(read32(&mut dev, 0x034));
        assert!(fsts.ppf(), "PPF must be set when fault is pending");

        // Clear fault by writing 1 to F bit (RW1C).
        write64(&mut dev, Reg::FRCD_DW2.0, 1u64 << 63);
        let fsts = FstsReg::from(read32(&mut dev, 0x034));
        assert!(!fsts.ppf(), "PPF must clear when F is cleared");

        // =====================================================================
        // Verify source validation: wrong BDF should be rejected
        // =====================================================================
        let wrong_rid_wrapper = shared.wrap_signal_msi(inner_msi.clone());
        let pre_count = inner_msi.count.load(Ordering::SeqCst);
        wrong_rid_wrapper.signal_msi(Some(0x0200), msi_addr, msi_data); // Wrong BDF
        assert_eq!(
            inner_msi.count.load(Ordering::SeqCst),
            pre_count,
            "MSI with wrong source ID must be dropped"
        );

        // =====================================================================
        // Verify disable: turn off translation, check identity mapping
        // =====================================================================
        // Write GCMD with TE=0, preserving QIE and IRE.
        write32(
            &mut dev,
            0x018,
            GcmdReg::new()
                .with_te(false)
                .with_qie(true)
                .with_ire(true)
                .into_bits(),
        );
        let gsts = GstsReg::from(read32(&mut dev, 0x01C));
        assert!(!gsts.tes(), "TES must clear when TE=0");
        assert!(gsts.qies(), "QIES must remain set");
        assert!(gsts.ires(), "IRES must remain set");

        // With translation disabled, IOVA = GPA (identity mapping).
        let gpa = iommu_common::IommuTranslator::translate(
            &translator,
            E2E_TEST_RID,
            0xDEAD_0000,
            false,
            |gpa| gpa,
        )
        .unwrap();
        assert_eq!(
            gpa, 0xDEAD_0000,
            "Disabled translation must return identity mapping"
        );
    }
}
