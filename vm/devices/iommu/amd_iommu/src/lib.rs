// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AMD IOMMU (AMD-Vi) emulator for OpenVMM.
//!
//! Provides emulated DMA address translation (IOVA → GPA via device table and
//! page table walking) and interrupt remapping for emulated PCI devices.
//!
//! The AMD IOMMU appears as a PCI function with an AMD IOMMU capability block
//! (CapID 0x0F) pointing to a fixed MMIO register region. The guest discovers
//! the IOMMU via PCI enumeration and reads the capability to find the MMIO base.

#![forbid(unsafe_code)]

pub mod spec;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::RwLock;
use pci_core::capabilities::ReadOnlyCapability;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::msi::SignalMsi;
use pci_core::spec::caps::CapabilityId;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use spec::commands::CommandEntry;
use spec::commands::CommandOpcode;
use spec::commands::CompletionWaitDw0Dw1;
use spec::commands::completion_wait_store_data;
use spec::dte::DTE_SIZE;
use spec::dte::Dte;
use spec::dte::IntCtl;
use spec::dte::PAGING_MODE_RESERVED;
use spec::dte::PagingMode;
use spec::events::EventEntry;
use spec::irte::IRTE_SIZE;
use spec::irte::Irte;
use spec::pte::IommuPte;
use spec::registers::BaseAddrHigh;
use spec::registers::BaseAddrLow;
use spec::registers::CapHeader;
use spec::registers::CmdBufBase;
use spec::registers::CmdBufHead;
use spec::registers::CmdBufTail;
use spec::registers::DevTabBase;
use spec::registers::EvtLogBase;
use spec::registers::EvtLogTail;
use spec::registers::ExclBase;
use spec::registers::ExclLimit;
use spec::registers::IommuCtrl;
use spec::registers::IommuStatus;
use spec::registers::MiscInfo0;
use spec::registers::MmioRegister;
use spec::registers::Range;
use std::ops::RangeInclusive;
use std::sync::Arc;
use vmcore::interrupt::Interrupt;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// MMIO region size (16KB per AMD IOMMU spec §3.4).
pub const MMIO_REGION_SIZE: u64 = 0x4000;

/// PCI config space offset of the IOMMU capability block.
///
/// Capabilities start at 0x40 and the IOMMU capability is the first (only)
/// capability registered.
pub const PCI_CAP_OFFSET: u16 = 0x40;

/// PCI capability ID for AMD IOMMU (§3.2).
const AMD_IOMMU_CAP_ID: CapabilityId = CapabilityId(spec::registers::CAP_ID);

/// `ExtFeat` value advertised by this emulator.
///
/// This is an emulator policy choice, not a spec value. Bits set:
///
/// - Bit 2: `XTSup` — x2APIC support. Truthful because the 128-bit GA-format
///   IRTE walker (`lookup_irte_inner`) already reassembles the 32-bit
///   destination from `destination_lo` (24 bits) and `destination_hi` (8 bits)
///   per §2.2.5.2, and `CONTROL[XTEn]`, `CONTROL[IntCapXTEn]`, and the XT
///   Interrupt Control Registers (§3.4.13) are implemented as software-visible
///   state. The emulator does not source MSIs from the XT Interrupt Control
///   Registers because IOMMU-internal interrupts are not delivered to the
///   guest — events are surfaced through `IOMMU_STATUS` only.
/// - Bit 6: `IASup` — INVALIDATE_IOMMU_ALL command supported.
/// - Bit 7: `GASup` — Guest virtual APIC support. Required so the Linux
///   driver's `init_iommu_one()` reads the IVHD EFR via
///   `early_iommu_features_init()`. Without this bit, the driver breaks out
///   of the type-40h switch before reading the EFR, and
///   `check_feature(FEATURE_IA)` falls back to per-entry invalidation
///   (65536 MMIO writes = ~20s boot). The driver also enables GA_EN and uses
///   128-bit GA-format IRTEs, which the emulator handles in
///   `lookup_irte_inner`.
pub const ADVERTISED_EXT_FEAT: u64 = 0x0000_0000_0000_00C4;

/// Physical address size (in bits) supported by this emulator.
pub const PA_SIZE: u8 = 48;

/// Virtual address size (in bits) supported by this emulator.
pub const VA_SIZE: u8 = 48;

// =============================================================================
// PCI Capability Data
// =============================================================================

/// Raw data for the AMD IOMMU PCI capability block (5 DWORDs = 20 bytes).
///
/// This struct is stored in a [`ReadOnlyCapability`] and provides the PCI
/// capability block contents. The config space emulator patches byte 1 of the
/// first DWORD with the next-capability pointer.
///
/// Layout (§3.2):
/// - DWORD 0: Capability header (CapID, CapPtr=0, CapType, CapRev, flags)
/// - DWORD 1: Base Address Low (Enable, MMIO base bits 31:14)
/// - DWORD 2: Base Address High (MMIO base bits 63:32)
/// - DWORD 3: Range (RngValid, BusNumber, FirstDevice, LastDevice)
/// - DWORD 4: MiscInfo0 (MsiNum, PA/VA size)
#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout)]
struct IommuCapabilityData {
    header: u32,
    base_addr_low: u32,
    base_addr_high: u32,
    range: u32,
    misc_info_0: u32,
}

impl IommuCapabilityData {
    /// Build the capability data for the given MMIO base address.
    fn new(mmio_base: u64) -> Self {
        let header = CapHeader::new()
            .with_cap_id(spec::registers::CAP_ID)
            .with_cap_ptr(0) // Patched by config space emulator
            .with_cap_type(0b011) // IOMMU capability type
            .with_cap_rev(0b00001) // Revision 1
            .with_iotlb_sup(false)
            .with_ht_tunnel(false)
            .with_np_cache(false)
            .with_efr_sup(true)
            .with_cap_ext(false);

        let base_addr_low = BaseAddrLow::new()
            .with_enable(true) // Pre-enabled (firmware has set this)
            .with_base_addr((mmio_base >> 14) as u32);

        let base_addr_high = BaseAddrHigh::new().with_base_addr((mmio_base >> 32) as u32);

        let range = Range::new()
            .with_rng_valid(true)
            .with_bus_number(0)
            .with_first_device(0x00)
            .with_last_device(0xFF);

        let misc_info_0 = MiscInfo0::new()
            .with_msi_num(0)
            .with_pa_size(48)
            .with_va_size(48);

        Self {
            header: header.into_bits(),
            base_addr_low: base_addr_low.into_bits(),
            base_addr_high: base_addr_high.into_bits(),
            range: range.into_bits(),
            misc_info_0: misc_info_0.into_bits(),
        }
    }
}

// =============================================================================
// AmdIommuDevice
// =============================================================================

/// Configuration for constructing an [`AmdIommuDevice`].
#[derive(Debug, Clone)]
pub struct AmdIommuConfig {
    /// MMIO base address for IOMMU registers.
    pub mmio_base: u64,
    /// PCI bus/device/function for the IOMMU.
    pub pci_bdf: (u8, u8, u8),
}

/// AMD IOMMU emulator device.
///
/// Appears as a PCI function on bus 0 with an AMD IOMMU capability block
/// pointing to a fixed MMIO register region. Implements the MMIO register
/// file for IOMMU control, command buffer, event log, and device table
/// configuration.
pub struct AmdIommuDevice {
    /// PCI config space emulator with IOMMU capability block.
    cfg_space: ConfigSpaceType0Emulator,

    /// Fixed MMIO base address.
    mmio_base: u64,

    /// Static region descriptor for MmioIntercept.
    static_regions: [(&'static str, RangeInclusive<u64>); 1],

    /// PCI bus/device/function.
    pci_bdf: (u8, u8, u8),

    /// Shared IOMMU state (accessible by per-device wrappers).
    shared: Arc<IommuSharedState>,
}

/// Internal register state of the AMD IOMMU.
#[derive(Debug, Default, Inspect)]
struct AmdIommuState {
    /// Device Table Base Address Register (MMIO 0x0000).
    #[inspect(hex)]
    dev_tab_base: u64,
    /// Command Buffer Base Address Register (MMIO 0x0008).
    #[inspect(hex)]
    cmd_buf_base: u64,
    /// Event Log Base Address Register (MMIO 0x0010).
    #[inspect(hex)]
    evt_log_base: u64,
    /// IOMMU Control Register (MMIO 0x0018).
    #[inspect(hex)]
    iommu_ctrl: u64,
    /// Exclusion Base Register (MMIO 0x0020).
    #[inspect(hex)]
    excl_base: u64,
    /// Exclusion Range Limit Register (MMIO 0x0028).
    #[inspect(hex)]
    excl_limit: u64,
    /// Command Buffer Head Pointer (MMIO 0x2000).
    #[inspect(hex)]
    cmd_buf_head: u64,
    /// Command Buffer Tail Pointer (MMIO 0x2008).
    #[inspect(hex)]
    cmd_buf_tail: u64,
    /// Event Log Head Pointer (MMIO 0x2010).
    #[inspect(hex)]
    evt_log_head: u64,
    /// Event Log Tail Pointer (MMIO 0x2018).
    #[inspect(hex)]
    evt_log_tail: u64,
    /// IOMMU Status Register (MMIO 0x2020).
    #[inspect(hex)]
    iommu_status: u64,
    /// General XT Interrupt Control Register (MMIO 0x0170).
    ///
    /// IOMMU's own MSI destination in x2APIC format. Per spec, sourced for
    /// the event log / general interrupt when `CONTROL[IntCapXTEn]=1`. The
    /// emulator stores the value for round-trip reads but does not source
    /// MSIs from it, since IOMMU-internal interrupts are not delivered to
    /// the guest.
    #[inspect(hex)]
    gen_xt_int_ctrl: u64,
    /// PPR XT Interrupt Control Register (MMIO 0x0178). See `gen_xt_int_ctrl`.
    #[inspect(hex)]
    ppr_xt_int_ctrl: u64,
}

// =============================================================================
// Shared IOMMU State
// =============================================================================

/// Shared IOMMU state accessible by per-device wrappers.
///
/// This struct holds the MMIO register state and guest memory reference
/// behind a `RwLock`, allowing concurrent reads from per-device
/// `IommuTranslatingMemory` and `IommuSignalMsi` wrappers while the
/// `AmdIommuDevice` performs exclusive writes via MMIO.
pub struct IommuSharedState {
    /// Guest memory for reading device/page tables.
    guest_memory: GuestMemory,
    /// MMIO register state, protected by a RwLock.
    state: RwLock<AmdIommuState>,
    /// MSI interrupt for the IOMMU's own interrupts (completion wait,
    /// event log). Configured by the guest via the PCI MSI capability.
    msi_interrupt: Option<Interrupt>,
}

impl IommuSharedState {
    /// Create new shared state with the given guest memory and MSI interrupt.
    fn new(guest_memory: GuestMemory, msi_interrupt: Option<Interrupt>) -> Self {
        Self {
            guest_memory,
            state: RwLock::new(AmdIommuState::default()),
            msi_interrupt,
        }
    }

    /// Deliver the IOMMU's own MSI interrupt (if configured by the guest).
    fn deliver_interrupt(&self) {
        if let Some(interrupt) = &self.msi_interrupt {
            interrupt.deliver();
        }
    }

    /// Returns whether the IOMMU is currently enabled.
    pub fn is_enabled(&self) -> bool {
        let state = self.state.read();
        IommuCtrl::from_bits(state.iommu_ctrl).iommu_en()
    }

    /// Translate an IOVA to a GPA for the given device.
    pub fn translate(&self, device_id: u16, iova: u64, write: bool) -> Result<u64, IommuFault> {
        let state = self.state.read();
        self.translate_inner(&state, device_id, iova, write)
    }

    /// Translate an IOVA while the caller already holds the state read lock.
    ///
    /// Use this when the caller needs to hold the lock across both translation
    /// and a subsequent memory access to prevent TOCTOU races with
    /// invalidations or config changes.
    fn translate_locked(
        &self,
        state: &AmdIommuState,
        device_id: u16,
        iova: u64,
        write: bool,
    ) -> Result<u64, IommuFault> {
        self.translate_inner(state, device_id, iova, write)
    }

    /// Remap an MSI address/data pair for the given device.
    pub fn remap_msi(
        &self,
        device_id: u16,
        address: u64,
        data: u32,
    ) -> Result<(u64, u32), IommuFault> {
        let state = self.state.read();
        self.remap_msi_inner(&state, device_id, address, data)
    }

    /// Queue a fault event to the event log.
    pub fn queue_event(&self, event: EventEntry) {
        let mut state = self.state.write();
        Self::write_event_inner(
            &self.guest_memory,
            &mut state,
            event,
            self.msi_interrupt.as_ref(),
        );
    }

    /// Look up a Device Table Entry for the given DeviceID.
    pub fn lookup_dte(&self, device_id: u16) -> Result<Dte, IommuFault> {
        let state = self.state.read();
        self.lookup_dte_inner(&state, device_id)
    }

    /// Look up an IRTE for the given DTE and MSI data.
    pub fn lookup_irte(
        &self,
        device_id: u16,
        dte: &Dte,
        msi_data: u32,
    ) -> Result<RemappedInterrupt, IommuFault> {
        let state = self.state.read();
        self.lookup_irte_inner(&state, device_id, dte, msi_data)
    }

    /// Create a translator for PCI devices behind this IOMMU.
    ///
    /// The translator uses the requester ID (RID / BDF) passed at each
    /// translation call directly as the AMD IOMMU DeviceID.
    pub fn translator(self: &Arc<Self>) -> AmdTranslator {
        AmdTranslator {
            shared: self.clone(),
        }
    }

    /// Create an `IommuSignalMsi` that remaps MSIs through the IOMMU's
    /// Interrupt Remapping Table.
    pub fn wrap_signal_msi(self: &Arc<Self>, inner_msi: Arc<dyn SignalMsi>) -> Arc<IommuSignalMsi> {
        Arc::new(IommuSignalMsi {
            shared: self.clone(),
            inner: inner_msi,
        })
    }

    // -- Internal methods that operate with a lock already held --

    fn lookup_dte_inner(&self, state: &AmdIommuState, device_id: u16) -> Result<Dte, IommuFault> {
        let dtb = DevTabBase::from_bits(state.dev_tab_base);
        let base_gpa = dtb.base_addr() << 12;
        let max_entries = ((dtb.size() as u64) + 1) * 4096 / (DTE_SIZE as u64);

        if (device_id as u64) >= max_entries {
            return Err(IommuFault::IllegalDevTableEntry {
                device_id,
                address: 0,
                is_interrupt: false,
                is_write: false,
            });
        }

        let dte_gpa = base_gpa + (device_id as u64) * (DTE_SIZE as u64);
        let dte: Dte =
            self.guest_memory
                .read_plain(dte_gpa)
                .map_err(|_| IommuFault::DevTabHardwareError {
                    device_id,
                    address: dte_gpa,
                })?;

        Ok(dte)
    }

    fn translate_inner(
        &self,
        state: &AmdIommuState,
        device_id: u16,
        iova: u64,
        write: bool,
    ) -> Result<u64, IommuFault> {
        let ctrl = IommuCtrl::from_bits(state.iommu_ctrl);
        if !ctrl.iommu_en() {
            return Ok(iova);
        }

        let dte = self.lookup_dte_inner(state, device_id)?;

        // Per AMD IOMMU spec §2.2.2 Table 8: when V=0, all addresses
        // are forwarded without translation.
        if !dte.dw0.v() {
            tracing::trace!(device_id, iova, "translate: DTE V=0, pass-through");
            return Ok(iova);
        }

        // V=1, TV=0: DTE control fields are valid but page translation
        // info is not. Per spec Table 8: "If the request requires a table
        // walk, the table walk is terminated." This is a target abort.
        if !dte.dw0.tv() {
            tracing::trace!(device_id, iova, "translate: V=1 TV=0, target abort");
            return Err(IommuFault::IoPageFault {
                device_id,
                domain_id: dte.dw1.domain_id(),
                address: iova,
                is_write: write,
            });
        }

        // Check exclusion range — addresses in the exclusion range
        // bypass page table walking and are passed through untranslated.
        let excl_base_reg = ExclBase::from_bits(state.excl_base);
        if excl_base_reg.ex_en() {
            let excl_start = excl_base_reg.base_addr() << 12;
            let excl_limit_reg = ExclLimit::from_bits(state.excl_limit);
            let excl_end = (excl_limit_reg.limit_addr() << 12) | 0xFFF;
            // Per spec §2.2.4: if Allow=1, all devices bypass in the range.
            // If Allow=0, only devices with DTE.EX=1 bypass.
            if excl_base_reg.allow() || dte.dw1.ex() {
                if iova >= excl_start && iova <= excl_end {
                    return Ok(iova);
                }
            }
        }

        let mode = dte.dw0.mode();
        if mode == PagingMode::DISABLED.0 {
            tracing::trace!(device_id, iova, "translate: mode=0, pass-through");
            return Ok(iova);
        }

        if mode == PAGING_MODE_RESERVED {
            return Err(IommuFault::IllegalDevTableEntry {
                device_id,
                address: 0,
                is_interrupt: false,
                is_write: write,
            });
        }

        let levels = mode;

        // §2.2.3: VA width = levels * 9 + 12. If any upper bits of the
        // IOVA are non-zero, the address exceeds the configured VA width
        // and must be rejected.
        let va_width = (levels as u32) * 9 + 12;
        if va_width < 64 && (iova >> va_width) != 0 {
            return Err(IommuFault::IoPageFault {
                device_id,
                domain_id: dte.dw1.domain_id(),
                address: iova,
                is_write: write,
            });
        }

        let root_addr = dte.dw0.page_table_root_address();
        let dte_ir = dte.dw0.ir();
        let dte_iw = dte.dw0.iw();
        let domain_id = dte.dw1.domain_id();

        tracing::trace!(
            device_id,
            iova,
            write,
            levels,
            root_addr,
            dte_ir,
            dte_iw,
            domain_id,
            "translate: walking page table"
        );

        self.walk_page_table(
            device_id, domain_id, root_addr, iova, levels, write, dte_ir, dte_iw,
        )
    }

    fn walk_page_table(
        &self,
        device_id: u16,
        domain_id: u16,
        root_addr: u64,
        iova: u64,
        levels: u8,
        write: bool,
        dte_ir: bool,
        dte_iw: bool,
    ) -> Result<u64, IommuFault> {
        let mut table_addr = root_addr;
        let mut current_level = levels;
        let mut can_read = dte_ir;
        let mut can_write = dte_iw;

        loop {
            let index = IommuPte::va_index(iova, current_level);
            let pte_gpa = table_addr + (index as u64) * 8;

            let pte: IommuPte = self.guest_memory.read_plain(pte_gpa).map_err(|_| {
                IommuFault::PageTabHardwareError {
                    device_id,
                    address: pte_gpa,
                }
            })?;

            tracing::trace!(
                device_id,
                iova,
                current_level,
                index,
                pte_gpa,
                pte_raw = pte.into_bits(),
                pte_pr = pte.is_present(),
                pte_next_level = pte.next_level(),
                pte_addr = pte.phys_address(),
                "walk: PTE step"
            );

            if !pte.is_present() {
                return Err(IommuFault::IoPageFault {
                    device_id,
                    domain_id,
                    address: iova,
                    is_write: write,
                });
            }

            can_read = can_read && pte.has_read();
            can_write = can_write && pte.has_write();

            if pte.is_leaf() || current_level == 1 {
                // Mode-7 leaves (NextLevel = 7) encode an arbitrary
                // power-of-two page size in the address field; mode-0 leaves
                // use the level's natural default page size. See
                // `IommuPte::mode7_page_size` for the encoding details.
                let (page_base, page_size_mask) = if pte.next_level() == 7 {
                    let raw = pte.into_bits();
                    let Some(page_size) = IommuPte::mode7_page_size(raw) else {
                        return Err(IommuFault::IoPageFault {
                            device_id,
                            domain_id,
                            address: iova,
                            is_write: write,
                        });
                    };
                    let mask = page_size - 1;
                    (IommuPte::mode7_page_base(raw, page_size), mask)
                } else {
                    (
                        pte.phys_address(),
                        IommuPte::page_offset_mask(current_level),
                    )
                };
                let gpa = page_base | (iova & page_size_mask);

                tracing::trace!(device_id, iova, gpa, current_level, "walk: leaf → GPA");

                if write && !can_write {
                    return Err(IommuFault::IoPageFault {
                        device_id,
                        domain_id,
                        address: iova,
                        is_write: true,
                    });
                }
                if !write && !can_read {
                    return Err(IommuFault::IoPageFault {
                        device_id,
                        domain_id,
                        address: iova,
                        is_write: false,
                    });
                }

                return Ok(gpa);
            }

            let next_level = pte.next_level();

            if next_level == 0 || next_level >= current_level {
                return Err(IommuFault::IoPageFault {
                    device_id,
                    domain_id,
                    address: iova,
                    is_write: write,
                });
            }

            table_addr = pte.phys_address();
            current_level = next_level;
        }
    }

    fn lookup_irte_inner(
        &self,
        state: &AmdIommuState,
        device_id: u16,
        dte: &Dte,
        msi_data: u32,
    ) -> Result<RemappedInterrupt, IommuFault> {
        let dw2 = dte.dw2;

        if !dw2.iv() {
            return Err(IommuFault::IllegalDevTableEntry {
                device_id,
                address: 0,
                is_interrupt: true,
                is_write: false,
            });
        }

        let irt_base = dw2.int_tab_address();
        let max_entries = dw2.int_tab_entries();

        // §2.2.5 / Figure 14: The IRTE index is always MSI data[10:0]
        // (11 bits). NumIntRemapMode controls which interrupt *types* are
        // remapped, not the index width.
        let irte_index = msi_data & 0x7FF;

        if irte_index >= max_entries {
            return Err(IommuFault::IoPageFault {
                device_id,
                domain_id: dte.dw1.domain_id(),
                address: 0,
                is_write: false,
            });
        }

        let ctrl = IommuCtrl::from_bits(state.iommu_ctrl);
        if ctrl.ga_en() {
            // 128-bit GA-format IRTE (16 bytes per entry).
            use spec::irte::{IRTE_GA_SIZE, IrteGa};
            let irte_gpa = irt_base + (irte_index as u64) * (IRTE_GA_SIZE as u64);
            let irte: IrteGa = self.guest_memory.read_plain(irte_gpa).map_err(|_| {
                IommuFault::PageTabHardwareError {
                    device_id,
                    address: irte_gpa,
                }
            })?;

            if !irte.lo.remap_en() {
                return Err(IommuFault::IoPageFault {
                    device_id,
                    domain_id: dte.dw1.domain_id(),
                    address: 0,
                    is_write: false,
                });
            }

            Ok(RemappedInterrupt {
                destination: irte.lo.destination() | ((irte.hi.destination_hi() as u32) << 24),
                vector: irte.hi.vector(),
                dm: irte.lo.dm(),
                int_type: irte.lo.int_type(),
            })
        } else {
            // Basic 32-bit IRTE (4 bytes per entry).
            let irte_gpa = irt_base + (irte_index as u64) * (IRTE_SIZE as u64);
            let irte: Irte = self.guest_memory.read_plain(irte_gpa).map_err(|_| {
                IommuFault::PageTabHardwareError {
                    device_id,
                    address: irte_gpa,
                }
            })?;

            if !irte.remap_en() {
                return Err(IommuFault::IoPageFault {
                    device_id,
                    domain_id: dte.dw1.domain_id(),
                    address: 0,
                    is_write: false,
                });
            }

            Ok(RemappedInterrupt {
                destination: irte.destination() as u32,
                vector: irte.vector(),
                dm: irte.dm(),
                int_type: irte.int_type(),
            })
        }
    }

    fn remap_msi_inner(
        &self,
        state: &AmdIommuState,
        device_id: u16,
        address: u64,
        data: u32,
    ) -> Result<(u64, u32), IommuFault> {
        let ctrl = IommuCtrl::from_bits(state.iommu_ctrl);
        if !ctrl.iommu_en() {
            return Ok((address, data));
        }

        let dte = self.lookup_dte_inner(state, device_id)?;

        // Per AMD IOMMU spec §2.2.2 Table 8: when V=0, interrupt
        // requests are passed upstream without remapping.
        if !dte.dw0.v() {
            return Ok((address, data));
        }

        tracing::trace!(
            device_id,
            address,
            data,
            int_ctl = dte.dw2.int_ctl(),
            iv = dte.dw2.iv(),
            ga_en = ctrl.ga_en(),
            "remap_msi: DTE lookup"
        );

        match dte.dw2.int_ctl_mode() {
            IntCtl::PASS_THROUGH => Ok((address, data)),
            IntCtl::ABORT => {
                // Strictly per spec, IntCtl=ABORT should target-abort all
                // interrupts. However, during device initialization the
                // Linux driver sets IV=1 (via init_device_table) with
                // IntCtl=00 before the IRT is configured. We pass through
                // interrupts when IV=0 to avoid dropping MSIs during this
                // window.
                if !dte.dw2.iv() {
                    return Ok((address, data));
                }
                Err(IommuFault::IllegalDevTableEntry {
                    device_id,
                    address,
                    is_interrupt: true,
                    is_write: false,
                })
            }
            IntCtl::REMAP => {
                let ri = self.lookup_irte_inner(state, device_id, &dte, data)?;

                let new_address: u64 =
                    MSI_ADDRESS_PREFIX | ((ri.destination as u64) << 12) | ((ri.dm as u64) << 2);
                let new_data: u32 = (ri.vector as u32) | ((ri.int_type as u32) << 8);

                tracing::trace!(
                    device_id,
                    address,
                    data,
                    new_address,
                    new_data,
                    dest = ri.destination,
                    vector = ri.vector,
                    dm = ri.dm,
                    int_type = ri.int_type,
                    "remap_msi: remapped"
                );

                Ok((new_address, new_data))
            }
            _ => Err(IommuFault::IllegalDevTableEntry {
                device_id,
                address,
                is_interrupt: true,
                is_write: false,
            }),
        }
    }

    fn write_event_inner(
        guest_memory: &GuestMemory,
        state: &mut AmdIommuState,
        event: EventEntry,
        msi_interrupt: Option<&Interrupt>,
    ) {
        let ctrl = IommuCtrl::from_bits(state.iommu_ctrl);
        if !ctrl.iommu_en() || !ctrl.evt_log_en() {
            return;
        }

        // §2.5: "When an event log overflow condition exists, the IOMMU
        // ceases recording events until software resets the event logging
        // function." Drop events while EventOverflow is set.
        let status = IommuStatus::from_bits(state.iommu_status);
        if status.evt_overflow() {
            return;
        }

        let buf_size_bytes = match evt_log_size_bytes(state) {
            Some(size) => size,
            None => {
                tracelimit::warn_ratelimited!(
                    "event log length below spec minimum (256 entries), dropping event"
                );
                return;
            }
        };
        let evt_base = EvtLogBase::from_bits(state.evt_log_base);
        let base_gpa = evt_base.base_addr() << 12;

        let tail = EvtLogTail::from_bits(state.evt_log_tail);
        // §3.4.15: "The IOMMU increments this register, rolling over at
        // the end of the buffer" — mask to buffer size.
        let tail_offset = ((tail.tail_ptr() as u64) << 4) % buf_size_bytes;

        let head = spec::registers::EvtLogHead::from_bits(state.evt_log_head);
        // §3.4.15: Behavior is undefined if software sets the head
        // pointer beyond the buffer length. Mask to keep the overflow
        // check and entry GPA within the configured ring.
        let head_offset = ((head.head_ptr() as u64) << 4) % buf_size_bytes;

        let next_tail = (tail_offset + 16) % buf_size_bytes;
        if next_tail == head_offset {
            let mut status = IommuStatus::from_bits(state.iommu_status);
            status.set_evt_overflow(true);
            state.iommu_status = status.into_bits();
            // §2.5.1: deliver MSI on overflow when EventIntEn is set.
            if ctrl.evt_int_en() {
                if let Some(interrupt) = msi_interrupt {
                    interrupt.deliver();
                }
            }
            return;
        }

        let entry_gpa = base_gpa + tail_offset;
        if let Err(e) = guest_memory.write_plain(entry_gpa, &event) {
            tracelimit::warn_ratelimited!(
                error = %e,
                gpa = entry_gpa,
                "failed to write event log entry"
            );
            return;
        }

        let new_tail = EvtLogTail::new().with_tail_ptr((next_tail >> 4) as u32);
        state.evt_log_tail = new_tail.into_bits();

        if ctrl.evt_int_en() {
            let mut status = IommuStatus::from_bits(state.iommu_status);
            status.set_evt_log_int(true);
            state.iommu_status = status.into_bits();
            if let Some(interrupt) = msi_interrupt {
                interrupt.deliver();
            }
        }
    }
}

// =============================================================================
// Per-Device DMA Translation Wrapper
// =============================================================================

/// An [`IommuTranslator`](iommu_common::IommuTranslator) for the AMD IOMMU.
///
/// One `AmdTranslator` is shared by all PCI devices behind the same IOMMU.
/// The requester ID (RID / BDF) is passed at each translation call and
/// used directly as the AMD IOMMU DeviceID.
pub struct AmdTranslator {
    /// Reference to the shared IOMMU state.
    shared: Arc<IommuSharedState>,
}

impl iommu_common::IommuTranslator for AmdTranslator {
    type Error = IommuFault;

    fn max_iova(&self) -> u64 {
        // The AMD IOMMU architecture supports up to 48-bit virtual
        // addresses (VA_SIZE), the architectural maximum for all
        // paging modes and translation configurations.
        1u64 << VA_SIZE
    }

    fn translate<R>(
        &self,
        rid: u16,
        iova: u64,
        write: bool,
        op: impl FnOnce(u64) -> R,
    ) -> Result<R, iommu_common::TranslationFault<IommuFault>> {
        let device_id = rid;

        // Hold the read lock across translate + op to prevent IOMMU config
        // from changing between getting the GPA and using it.
        let state = self.shared.state.read();
        let gpa = match self.shared.translate_locked(&state, device_id, iova, write) {
            Ok(gpa) => gpa,
            Err(fault) => {
                drop(state);
                self.shared.queue_event(fault.to_event_entry());
                return Err(iommu_common::TranslationFault { iova, error: fault });
            }
        };

        let result = op(gpa);
        drop(state);
        Ok(result)
    }
}

/// Return a ring buffer size in bytes from a log2-entry-count field, or
/// `None` if the value is below the spec minimum of 1000b (256 entries).
/// §3.4.1: values 0000b–0111b are reserved for command buffer, event log,
/// and PPR log length fields.
fn ring_size_bytes(log2_entries: u8) -> Option<u64> {
    if log2_entries < 8 {
        None
    } else {
        Some((1u64 << (log2_entries as u64)) * 16)
    }
}

/// Return the command buffer size in bytes, or `None` if invalid.
fn cmd_buf_size_bytes(state: &AmdIommuState) -> Option<u64> {
    ring_size_bytes(CmdBufBase::from_bits(state.cmd_buf_base).length())
}

/// Return the event log size in bytes, or `None` if invalid.
fn evt_log_size_bytes(state: &AmdIommuState) -> Option<u64> {
    ring_size_bytes(EvtLogBase::from_bits(state.evt_log_base).length())
}

// =============================================================================
// Per-Device MSI Remapping Wrapper
// =============================================================================

/// A `SignalMsi` implementation that remaps MSIs through the AMD IOMMU's
/// Interrupt Remapping Table before delivering them.
///
/// The requester ID (BDF) is taken from the `devid` parameter supplied by
/// the PCI MSI layer at each `signal_msi` call. MSIs without a requester
/// ID are dropped, matching the SMMU behavior.
pub struct IommuSignalMsi {
    /// Reference to the shared IOMMU state.
    shared: Arc<IommuSharedState>,
    /// The inner SignalMsi target (delivers to the partition APIC).
    inner: Arc<dyn SignalMsi>,
}

impl SignalMsi for IommuSignalMsi {
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
        // Use the supplied requester ID for the DTE/IRTE lookup. The PCI
        // MSI layer resolves the requester ID before calling SignalMsi,
        // so `devid` is the correct BDF for interrupt remapping. Drop
        // the MSI when no requester ID is supplied — without a BDF we
        // cannot perform the DTE/IRTE lookup required for remapping.
        let Some(device_id) = devid else {
            return;
        };
        let device_id = device_id as u16;
        match self.shared.remap_msi(device_id, address, data) {
            Ok((new_address, new_data)) => {
                self.inner.signal_msi(devid, new_address, new_data);
            }
            Err(fault) => {
                self.shared.queue_event(fault.to_event_entry());
                tracelimit::warn_ratelimited!(device_id, "MSI remapping fault, interrupt dropped");
            }
        }
    }
}

impl AmdIommuDevice {
    /// Create a new AMD IOMMU device with the given configuration.
    ///
    /// `msi_target` is used for the IOMMU's own MSI capability, which the
    /// guest driver uses for completion wait and event log interrupts.
    pub fn new(
        guest_memory: GuestMemory,
        config: AmdIommuConfig,
        msi_target: &pci_core::msi::MsiTarget,
    ) -> Self {
        let mmio_base = config.mmio_base;

        // Build the PCI capability block data.
        let cap_data = IommuCapabilityData::new(mmio_base);
        let capability =
            ReadOnlyCapability::new_with_capability_id("amd-iommu", AMD_IOMMU_CAP_ID, cap_data);

        // MSI capability: 1 vector, 64-bit address, no per-vector masking.
        let msi_capability =
            pci_core::capabilities::msi_cap::MsiCapability::new(0, true, false, msi_target);

        // Extract the interrupt handle before the capability is consumed into
        // the config space emulator. This lets the IOMMU deliver its own MSIs
        // for completion wait and event log interrupts.
        let msi_interrupt = msi_capability.interrupt();

        // Build PCI config space with AMD IOMMU IDs and capabilities.
        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: spec::registers::PCI_VENDOR_ID,
                device_id: spec::registers::PCI_DEVICE_ID,
                revision_id: 0x00,
                prog_if: ProgrammingInterface(spec::registers::PCI_CLASS_PROG_IF),
                sub_class: Subclass(spec::registers::PCI_CLASS_SUB),
                base_class: ClassCode(spec::registers::PCI_CLASS_BASE),
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(capability), Box::new(msi_capability)],
            vec![],            // No extended capabilities
            DeviceBars::new(), // No BARs — MMIO is via capability, not BAR
        );

        let static_regions = [(
            "amd-iommu-mmio",
            mmio_base..=mmio_base + MMIO_REGION_SIZE - 1,
        )];

        Self {
            cfg_space,
            mmio_base,
            static_regions,
            pci_bdf: config.pci_bdf,
            shared: Arc::new(IommuSharedState::new(guest_memory, msi_interrupt)),
        }
    }

    /// Returns the shared IOMMU state for creating per-device wrappers.
    pub fn shared_state(&self) -> &Arc<IommuSharedState> {
        &self.shared
    }

    /// Returns whether the IOMMU is currently enabled (IommuCtrl.IommuEn).
    pub fn is_enabled(&self) -> bool {
        self.shared.is_enabled()
    }

    /// Returns the current IOMMU control register value.
    pub fn iommu_ctrl(&self) -> IommuCtrl {
        let state = self.shared.state.read();
        IommuCtrl::from_bits(state.iommu_ctrl)
    }

    /// Read a 64-bit MMIO register.
    fn read_register(&self, offset: u64) -> u64 {
        let state = self.shared.state.read();
        match MmioRegister(offset as u16) {
            MmioRegister::DEV_TAB_BASE => state.dev_tab_base,
            MmioRegister::CMD_BUF_BASE => state.cmd_buf_base,
            MmioRegister::EVT_LOG_BASE => state.evt_log_base,
            MmioRegister::IOMMU_CTRL => state.iommu_ctrl,
            MmioRegister::EXCL_BASE => state.excl_base,
            MmioRegister::EXCL_LIMIT => state.excl_limit,
            MmioRegister::EXT_FEAT => ADVERTISED_EXT_FEAT,
            MmioRegister::EXT_FEAT2 => 0, // No EFR2 features supported.
            MmioRegister::CMD_BUF_HEAD => state.cmd_buf_head,
            MmioRegister::CMD_BUF_TAIL => state.cmd_buf_tail,
            MmioRegister::EVT_LOG_HEAD => state.evt_log_head,
            MmioRegister::EVT_LOG_TAIL => state.evt_log_tail,
            MmioRegister::IOMMU_STATUS => state.iommu_status,
            MmioRegister::GEN_XT_INT_CTRL => state.gen_xt_int_ctrl,
            MmioRegister::PPR_XT_INT_CTRL => state.ppr_xt_int_ctrl,
            _ => {
                tracelimit::warn_ratelimited!(offset, "MMIO read from unknown register");
                0
            }
        }
    }

    /// Write a 64-bit MMIO register. Returns true if the write was handled.
    fn write_register(&mut self, offset: u64, value: u64) {
        let mut state = self.shared.state.write();
        let ctrl = IommuCtrl::from_bits(state.iommu_ctrl);

        tracing::trace!(offset, value, "mmio_write");

        match MmioRegister(offset as u16) {
            MmioRegister::DEV_TAB_BASE => {
                if !ctrl.iommu_en() {
                    state.dev_tab_base = value;
                } else {
                    tracelimit::warn_ratelimited!(
                        "write to DevTabBase while IOMMU enabled, ignored"
                    );
                }
            }
            MmioRegister::CMD_BUF_BASE => {
                if !ctrl.iommu_en() {
                    state.cmd_buf_base = value;
                } else {
                    tracelimit::warn_ratelimited!(
                        "write to CmdBufBase while IOMMU enabled, ignored"
                    );
                }
            }
            MmioRegister::EVT_LOG_BASE => {
                if !ctrl.iommu_en() {
                    state.evt_log_base = value;
                } else {
                    tracelimit::warn_ratelimited!(
                        "write to EvtLogBase while IOMMU enabled, ignored"
                    );
                }
            }
            MmioRegister::IOMMU_CTRL => {
                let old_ctrl = IommuCtrl::from_bits(state.iommu_ctrl);
                state.iommu_ctrl = value;
                Self::update_status_from_ctrl(&mut state);
                // If CmdBufEn transitions from 0→1 while IOMMU is enabled
                // and there are pending commands, process them now.
                let new_ctrl = IommuCtrl::from_bits(value);
                if new_ctrl.iommu_en() && new_ctrl.cmd_buf_en() && !old_ctrl.cmd_buf_en() {
                    Self::process_commands(&self.shared, &mut state);
                }
            }
            MmioRegister::EXCL_BASE => {
                state.excl_base = value;
            }
            MmioRegister::EXCL_LIMIT => {
                state.excl_limit = value;
            }
            MmioRegister::EXT_FEAT | MmioRegister::EXT_FEAT2 => {
                // Read-only registers, ignore writes.
            }
            MmioRegister::CMD_BUF_HEAD => {
                if !ctrl.iommu_en() {
                    state.cmd_buf_head = value;
                }
            }
            MmioRegister::CMD_BUF_TAIL => {
                state.cmd_buf_tail = value;
                Self::process_commands(&self.shared, &mut state);
            }
            MmioRegister::EVT_LOG_HEAD => {
                state.evt_log_head = value;
            }
            MmioRegister::EVT_LOG_TAIL => {
                if !ctrl.iommu_en() {
                    state.evt_log_tail = value;
                }
            }
            MmioRegister::IOMMU_STATUS => {
                let status = IommuStatus::from_bits(state.iommu_status);
                let write_val = IommuStatus::from_bits(value);

                let new_status = IommuStatus::new()
                    .with_evt_overflow(status.evt_overflow() && !write_val.evt_overflow())
                    .with_evt_log_int(status.evt_log_int() && !write_val.evt_log_int())
                    .with_com_wait_int(status.com_wait_int() && !write_val.com_wait_int())
                    .with_evt_log_run(status.evt_log_run())
                    .with_cmd_buf_run(status.cmd_buf_run());

                state.iommu_status = new_status.into_bits();
            }
            MmioRegister::GEN_XT_INT_CTRL => {
                // IOMMU's own MSI destination in x2APIC format. Stored
                // verbatim; the emulator does not source MSIs from this
                // register because IOMMU-internal interrupts are not
                // delivered to the guest.
                state.gen_xt_int_ctrl = value;
            }
            MmioRegister::PPR_XT_INT_CTRL => {
                // PPR log MSI destination in x2APIC format. Stored verbatim;
                // the PPR log is not implemented.
                state.ppr_xt_int_ctrl = value;
            }
            _ => {
                tracelimit::warn_ratelimited!(offset, value, "MMIO write to unknown register");
            }
        }
    }

    /// Update IommuStatus RO bits based on the current IommuCtrl settings.
    ///
    /// `CmdBufRun` and `EvtLogRun` are only set when the respective buffer
    /// has a valid (spec-minimum) length. §3.4.1: values below 1000b (256
    /// entries) are reserved.
    fn update_status_from_ctrl(state: &mut AmdIommuState) {
        let ctrl = IommuCtrl::from_bits(state.iommu_ctrl);
        let mut status = IommuStatus::from_bits(state.iommu_status);

        status.set_cmd_buf_run(
            ctrl.iommu_en() && ctrl.cmd_buf_en() && cmd_buf_size_bytes(state).is_some(),
        );
        status.set_evt_log_run(
            ctrl.iommu_en() && ctrl.evt_log_en() && evt_log_size_bytes(state).is_some(),
        );

        state.iommu_status = status.into_bits();
    }

    // =========================================================================
    // Event Log (1D)
    // =========================================================================

    /// Write an event record to the event log in guest memory.
    #[cfg(test)]
    fn write_event(&mut self, event: EventEntry) {
        let mut state = self.shared.state.write();
        IommuSharedState::write_event_inner(
            &self.shared.guest_memory,
            &mut state,
            event,
            self.shared.msi_interrupt.as_ref(),
        );
    }

    // =========================================================================
    // Command Buffer Processing (1C)
    // =========================================================================

    /// Process all pending commands in the command buffer.
    fn process_commands(shared: &IommuSharedState, state: &mut AmdIommuState) {
        let ctrl = IommuCtrl::from_bits(state.iommu_ctrl);
        if !ctrl.iommu_en() || !ctrl.cmd_buf_en() {
            return;
        }

        let buf_size_bytes = match cmd_buf_size_bytes(state) {
            Some(size) => size,
            None => {
                tracelimit::warn_ratelimited!(
                    "command buffer length below spec minimum (256 entries), halting"
                );
                let mut status = IommuStatus::from_bits(state.iommu_status);
                status.set_cmd_buf_run(false);
                state.iommu_status = status.into_bits();
                return;
            }
        };
        let cmd_base = CmdBufBase::from_bits(state.cmd_buf_base);
        let base_gpa = cmd_base.base_addr() << 12;

        loop {
            let head = CmdBufHead::from_bits(state.cmd_buf_head);
            // §3.4.15: "The IOMMU increments this register, rolling over to
            // zero at the end of the buffer" — mask to buffer size.
            let head_offset = ((head.head_ptr() as u64) << 4) % buf_size_bytes;

            let tail = CmdBufTail::from_bits(state.cmd_buf_tail);
            // §3.4.15: Behavior is undefined if software sets the tail
            // pointer beyond the buffer length. Mask to prevent an
            // unreachable comparison that would spin the command loop.
            let tail_offset = ((tail.tail_ptr() as u64) << 4) % buf_size_bytes;

            if head_offset == tail_offset {
                break;
            }

            let entry_gpa = base_gpa + head_offset;
            let entry: CommandEntry = match shared.guest_memory.read_plain(entry_gpa) {
                Ok(e) => e,
                Err(e) => {
                    tracelimit::warn_ratelimited!(
                        error = %e,
                        gpa = entry_gpa,
                        "failed to read command buffer entry"
                    );
                    // §2.5.7: log COMMAND_HARDWARE_ERROR event.
                    let event = EventEntry::command_hardware_error(entry_gpa);
                    IommuSharedState::write_event_inner(
                        &shared.guest_memory,
                        state,
                        event,
                        shared.msi_interrupt.as_ref(),
                    );
                    let mut status = IommuStatus::from_bits(state.iommu_status);
                    status.set_cmd_buf_run(false);
                    state.iommu_status = status.into_bits();
                    return;
                }
            };

            tracing::trace!(opcode = entry.opcode().0, entry_gpa, "command");

            match entry.opcode() {
                CommandOpcode::COMPLETION_WAIT => {
                    Self::process_completion_wait(shared, state, &entry);
                }
                CommandOpcode::INVALIDATE_DEVTAB_ENTRY
                | CommandOpcode::INVALIDATE_IOMMU_PAGES
                | CommandOpcode::INVALIDATE_IOTLB_PAGES
                | CommandOpcode::INVALIDATE_INTERRUPT_TABLE
                | CommandOpcode::PREFETCH_IOMMU_PAGES
                | CommandOpcode::INVALIDATE_IOMMU_ALL => {
                    // No-op: no caches to invalidate in the emulator.
                }
                _ => {
                    tracelimit::warn_ratelimited!(
                        opcode = entry.opcode().0,
                        "unknown command opcode, halting command buffer"
                    );
                    let event = EventEntry::illegal_command_error(entry_gpa);
                    IommuSharedState::write_event_inner(
                        &shared.guest_memory,
                        state,
                        event,
                        shared.msi_interrupt.as_ref(),
                    );

                    let mut status = IommuStatus::from_bits(state.iommu_status);
                    status.set_cmd_buf_run(false);
                    state.iommu_status = status.into_bits();
                    return;
                }
            }

            let next_head = (head_offset + 16) % buf_size_bytes;
            let new_head = CmdBufHead::new().with_head_ptr((next_head >> 4) as u32);
            state.cmd_buf_head = new_head.into_bits();
        }
    }

    /// Process a COMPLETION_WAIT command (opcode 0x01).
    fn process_completion_wait(
        shared: &IommuSharedState,
        state: &mut AmdIommuState,
        entry: &CommandEntry,
    ) {
        let fields = CompletionWaitDw0Dw1::from(entry);

        if fields.s() {
            let store_addr = fields.store_address();
            let store_data = completion_wait_store_data(entry);
            if let Err(e) = shared
                .guest_memory
                .write_plain(store_addr, &store_data.to_le_bytes())
            {
                tracelimit::warn_ratelimited!(
                    error = %e,
                    addr = store_addr,
                    "failed to write COMPLETION_WAIT store data"
                );
            }
        }

        if fields.i() {
            // §2.4.1: "If the i bit is set, the IOMMU sets MMIO Offset
            // 2020h[ComWaitInt]." The status bit is set unconditionally;
            // ComWaitIntEn only gates MSI delivery.
            let mut status = IommuStatus::from_bits(state.iommu_status);
            status.set_com_wait_int(true);
            state.iommu_status = status.into_bits();

            let ctrl = IommuCtrl::from_bits(state.iommu_ctrl);
            if ctrl.com_wait_int_en() {
                shared.deliver_interrupt();
            }
        }
    }

    // =========================================================================
    // Delegation to shared state (1E, 1F)
    // =========================================================================

    /// Look up a Device Table Entry for the given DeviceID (BDF).
    pub fn lookup_dte(&self, device_id: u16) -> Result<Dte, IommuFault> {
        self.shared.lookup_dte(device_id)
    }

    /// Translate an IOVA to a GPA for the given device.
    pub fn translate(&self, device_id: u16, iova: u64, write: bool) -> Result<u64, IommuFault> {
        self.shared.translate(device_id, iova, write)
    }

    /// Look up an IRTE for the given DTE and MSI data.
    pub fn lookup_irte(
        &self,
        device_id: u16,
        dte: &Dte,
        msi_data: u32,
    ) -> Result<RemappedInterrupt, IommuFault> {
        self.shared.lookup_irte(device_id, dte, msi_data)
    }

    /// Remap an MSI address/data pair for the given device.
    pub fn remap_msi(
        &self,
        device_id: u16,
        address: u64,
        data: u32,
    ) -> Result<(u64, u32), IommuFault> {
        self.shared.remap_msi(device_id, address, data)
    }
}

/// x86 MSI address prefix: 0xFEE0_0000.
const MSI_ADDRESS_PREFIX: u64 = 0xFEE0_0000;

/// Result of an IRTE lookup — the remapped interrupt fields needed to
/// construct the new MSI address/data pair.
#[derive(Debug)]
pub struct RemappedInterrupt {
    /// Destination APIC ID.
    pub destination: u32,
    /// Interrupt vector.
    pub vector: u8,
    /// Destination mode — false = physical, true = logical.
    pub dm: bool,
    /// Interrupt type (delivery mode).
    pub int_type: u8,
}

// =============================================================================
// Translation Faults
// =============================================================================

/// An IOMMU translation fault.
///
/// These correspond to events that would be logged to the event log and
/// indicate a translation or lookup failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IommuFault {
    /// The DeviceID is out of range or the DTE has reserved field
    /// values (§2.5.2).
    #[error("illegal device table entry for device {device_id:#06x} at {address:#x}")]
    IllegalDevTableEntry {
        /// The device that caused the fault (BDF).
        device_id: u16,
        /// The faulting address.
        address: u64,
        /// Whether this was an interrupt request.
        is_interrupt: bool,
        /// Whether this was a write access.
        is_write: bool,
    },
    /// An I/O page fault — page not present, or permission violation (§2.5.3).
    #[error(
        "I/O page fault for device {device_id:#06x} domain {domain_id} at {address:#x} (write={is_write})"
    )]
    IoPageFault {
        /// The device that caused the fault (BDF).
        device_id: u16,
        /// Domain ID from the DTE.
        domain_id: u16,
        /// The faulting IOVA.
        address: u64,
        /// Whether this was a write access.
        is_write: bool,
    },
    /// Hardware error reading the device table (§2.5.4).
    #[error("device table hardware error for device {device_id:#06x} at {address:#x}")]
    DevTabHardwareError {
        /// The device that caused the fault (BDF).
        device_id: u16,
        /// The address that failed to read.
        address: u64,
    },
    /// Hardware error reading a page table (§2.5.5).
    #[error("page table hardware error for device {device_id:#06x} at {address:#x}")]
    PageTabHardwareError {
        /// The device that caused the fault (BDF).
        device_id: u16,
        /// The address that failed to read.
        address: u64,
    },
}

impl IommuFault {
    /// Convert this fault into an event log entry.
    pub fn to_event_entry(&self) -> EventEntry {
        match self {
            IommuFault::IllegalDevTableEntry {
                device_id,
                is_interrupt,
                is_write,
                address,
            } => {
                EventEntry::illegal_dev_table_entry(*device_id, *is_interrupt, *is_write, *address)
            }
            IommuFault::IoPageFault {
                device_id,
                domain_id,
                address,
                is_write,
            } => EventEntry::io_page_fault(*device_id, *domain_id, false, *is_write, *address),
            IommuFault::DevTabHardwareError { device_id, address } => {
                EventEntry::dev_tab_hardware_error(*device_id, *address)
            }
            IommuFault::PageTabHardwareError { device_id, address } => {
                EventEntry::page_tab_hardware_error(*device_id, *address)
            }
        }
    }
}

impl InspectMut for AmdIommuDevice {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .hex("mmio_base", self.mmio_base)
            .field(
                "pci_bdf",
                format!(
                    "{:02x}:{:02x}.{}",
                    self.pci_bdf.0, self.pci_bdf.1, self.pci_bdf.2
                ),
            )
            .field("enabled", self.is_enabled())
            .field("state", &*self.shared.state.read());
    }
}

impl vmcore::device_state::ChangeDeviceState for AmdIommuDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let mut state = self.shared.state.write();
        *state = AmdIommuState::default();
    }
}

impl vmcore::save_restore::SaveRestore for AmdIommuDevice {
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
// ChipsetDevice trait implementation
// =============================================================================

impl ChipsetDevice for AmdIommuDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

// =============================================================================
// PCI Config Space
// =============================================================================

impl PciConfigSpace for AmdIommuDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.cfg_space.read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.cfg_space.write_u32(offset, value)
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        Some(self.pci_bdf)
    }
}

// =============================================================================
// MMIO Register Access
// =============================================================================

impl MmioIntercept for AmdIommuDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        let offset = addr - self.mmio_base;

        // All IOMMU MMIO registers are 64-bit, but guests may access them
        // as 32-bit halves (lower then upper DWORD).
        match data.len() {
            8 => {
                // 64-bit aligned read of a full register.
                let reg_offset = offset & !7;
                let val = self.read_register(reg_offset);
                data.copy_from_slice(&val.to_le_bytes());
            }
            4 => {
                // 32-bit read of lower or upper half.
                let reg_offset = offset & !7;
                let val = self.read_register(reg_offset);
                let bytes = val.to_le_bytes();
                let half_offset = (offset & 4) as usize;
                data.copy_from_slice(&bytes[half_offset..half_offset + 4]);
            }
            _ => {
                tracelimit::warn_ratelimited!(addr, len = data.len(), "unsupported MMIO read size");
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
                // 64-bit aligned write of a full register.
                let reg_offset = offset & !7;
                let val = u64::from_le_bytes(data.try_into().unwrap());
                self.write_register(reg_offset, val);
            }
            4 => {
                // 32-bit write to lower or upper half.
                // Read-modify-write the full 64-bit register.
                let reg_offset = offset & !7;
                let mut val = self.read_register(reg_offset);
                let half_offset = (offset & 4) as usize;
                let mut bytes = val.to_le_bytes();
                bytes[half_offset..half_offset + 4].copy_from_slice(data);
                val = u64::from_le_bytes(bytes);
                self.write_register(reg_offset, val);
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    addr,
                    len = data.len(),
                    "unsupported MMIO write size"
                );
                return IoResult::Err(IoError::InvalidAccessSize);
            }
        }

        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        &self.static_regions
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use guestmem::GuestMemory;
    use spec::commands::CommandEntry;
    use spec::dte::IntCtl;
    use spec::events::EventCode;
    use spec::events::EventEntry;
    use spec::irte::Irte;
    use spec::registers::CmdBufBase;
    use spec::registers::CmdBufHead;
    use spec::registers::CmdBufTail;
    use spec::registers::DevTabBase;
    use spec::registers::EvtLogHead;
    use spec::registers::ExtFeat;

    /// Test-only MMIO base address.
    const TEST_MMIO_BASE: u64 = 0xFD00_0000;

    fn test_config() -> AmdIommuConfig {
        AmdIommuConfig {
            mmio_base: TEST_MMIO_BASE,
            pci_bdf: (0, 0, 0),
        }
    }

    fn create_test_device() -> AmdIommuDevice {
        let guest_memory = GuestMemory::empty();
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target())
    }

    /// Helper to read a 32-bit PCI config register.
    fn pci_read(dev: &mut AmdIommuDevice, offset: u16) -> u32 {
        let mut value = 0u32;
        let _ = dev.pci_cfg_read(offset, &mut value);
        value
    }

    /// Helper to read a 64-bit MMIO register.
    fn mmio_read64(dev: &mut AmdIommuDevice, offset: u64) -> u64 {
        let addr = dev.mmio_base + offset;
        let mut data = [0u8; 8];
        let _ = dev.mmio_read(addr, &mut data);
        u64::from_le_bytes(data)
    }

    /// Helper to write a 64-bit MMIO register.
    fn mmio_write64(dev: &mut AmdIommuDevice, offset: u64, value: u64) {
        let addr = dev.mmio_base + offset;
        let _ = dev.mmio_write(addr, &value.to_le_bytes());
    }

    /// Helper to read a 32-bit MMIO value.
    fn mmio_read32(dev: &mut AmdIommuDevice, offset: u64) -> u32 {
        let addr = dev.mmio_base + offset;
        let mut data = [0u8; 4];
        let _ = dev.mmio_read(addr, &mut data);
        u32::from_le_bytes(data)
    }

    /// Helper to write a 32-bit MMIO value.
    fn mmio_write32(dev: &mut AmdIommuDevice, offset: u64, value: u32) {
        let addr = dev.mmio_base + offset;
        let _ = dev.mmio_write(addr, &value.to_le_bytes());
    }

    // =========================================================================
    // PCI Config Space Tests (1B.2)
    // =========================================================================

    #[test]
    fn test_pci_vendor_device_id() {
        let mut dev = create_test_device();
        let id = pci_read(&mut dev, 0x00);
        assert_eq!(id & 0xFFFF, 0x1022, "vendor ID should be AMD");
        assert_eq!(id >> 16, 0x1451, "device ID should be 0x1451");
    }

    #[test]
    fn test_pci_class_code() {
        let mut dev = create_test_device();
        let class_rev = pci_read(&mut dev, 0x08);
        assert_eq!(
            (class_rev >> 24) & 0xFF,
            0x08,
            "base class: System Peripheral"
        );
        assert_eq!((class_rev >> 16) & 0xFF, 0x06, "sub class: IOMMU");
        assert_eq!((class_rev >> 8) & 0xFF, 0x00, "prog_if: 0");
    }

    #[test]
    fn test_pci_capability_pointer() {
        let mut dev = create_test_device();
        // Offset 0x34 = capabilities pointer, should point to 0x40.
        let cap_ptr = pci_read(&mut dev, 0x34);
        assert_eq!(cap_ptr & 0xFF, 0x40, "capability pointer should be 0x40");
    }

    #[test]
    fn test_pci_capability_header() {
        let mut dev = create_test_device();
        // Read capability at offset 0x40 (first DWORD of capability).
        let cap_header_raw = pci_read(&mut dev, 0x40);
        let cap_id = cap_header_raw & 0xFF;
        let cap_next = (cap_header_raw >> 8) & 0xFF;

        assert_eq!(cap_id, 0x0F, "CapID should be 0x0F (AMD IOMMU)");
        assert_ne!(cap_next, 0x00, "should have a next capability (MSI)");

        let header = CapHeader::from_bits(cap_header_raw);
        assert_eq!(header.cap_type(), 0b011, "CapType should be 011b");
        assert_eq!(header.cap_rev(), 0b00001, "CapRev should be 1");
        assert!(header.efr_sup(), "EFRSup should be set");
        assert!(!header.iotlb_sup(), "IotlbSup should be clear");
        assert!(!header.ht_tunnel(), "HtTunnel should be clear");
        assert!(!header.np_cache(), "NpCache should be clear");
        assert!(!header.cap_ext(), "CapExt should be clear");
    }

    #[test]
    fn test_pci_capability_mmio_base() {
        let mut dev = create_test_device();
        // Offset 0x44 = BaseAddrLow, 0x48 = BaseAddrHigh.
        let base_low_raw = pci_read(&mut dev, 0x44);
        let base_high_raw = pci_read(&mut dev, 0x48);

        let base_low = BaseAddrLow::from_bits(base_low_raw);
        let base_high = BaseAddrHigh::from_bits(base_high_raw);

        assert!(base_low.enable(), "Enable should be set");

        let mmio_addr =
            ((base_low.base_addr() as u64) << 14) | ((base_high.base_addr() as u64) << 32);
        assert_eq!(mmio_addr, TEST_MMIO_BASE, "MMIO base should match");
    }

    #[test]
    fn test_pci_capability_device_range() {
        let mut dev = create_test_device();
        // Offset 0x4C = Range register.
        let range_raw = pci_read(&mut dev, 0x4C);
        let range = Range::from_bits(range_raw);

        assert!(range.rng_valid(), "RngValid should be set");
        assert_eq!(range.bus_number(), 0, "bus number should be 0");
        assert_eq!(range.first_device(), 0x00, "first device should be 0x00");
        assert_eq!(range.last_device(), 0xFF, "last device should be 0xFF");
    }

    #[test]
    fn test_pci_capability_misc_info() {
        let mut dev = create_test_device();
        // Offset 0x50 = MiscInfo0.
        let misc_raw = pci_read(&mut dev, 0x50);
        let misc = MiscInfo0::from_bits(misc_raw);

        assert_eq!(misc.pa_size(), 48, "PA size should be 48 bits");
        assert_eq!(misc.va_size(), 48, "VA size should be 48 bits");
        assert_eq!(misc.msi_num(), 0, "MsiNum should be 0");
    }

    #[test]
    fn test_suggested_bdf() {
        let mut dev = create_test_device();
        assert_eq!(
            dev.suggested_bdf(),
            Some((0, 0, 0)),
            "should suggest default BDF"
        );
    }

    // =========================================================================
    // MMIO Register Tests (1B.3, 1B.4)
    // =========================================================================

    #[test]
    fn test_extfeat_readback() {
        let mut dev = create_test_device();
        let ext_feat = mmio_read64(&mut dev, MmioRegister::EXT_FEAT.0 as u64);

        assert_eq!(
            ext_feat, ADVERTISED_EXT_FEAT,
            "ExtFeat should match emulator constant"
        );

        let feat = ExtFeat::from_bits(ext_feat);
        assert!(feat.ia_sup(), "IASup should be set");
        assert!(feat.ga_sup(), "GASup should be set");
        assert!(!feat.pref_sup(), "PrefSup should be clear");
        assert!(!feat.ppr_sup(), "PPRSup should be clear");
        assert!(!feat.gt_sup(), "GTSup should be clear");
        assert_eq!(feat.hats(), 0, "HATS should be 00 (4-level)");
    }

    #[test]
    fn test_extfeat_readonly() {
        let mut dev = create_test_device();
        // Write to ExtFeat should have no effect.
        mmio_write64(&mut dev, MmioRegister::EXT_FEAT.0 as u64, 0xDEAD_BEEF);
        let ext_feat = mmio_read64(&mut dev, MmioRegister::EXT_FEAT.0 as u64);
        assert_eq!(
            ext_feat, ADVERTISED_EXT_FEAT,
            "ExtFeat should be unchanged after write"
        );
    }

    #[test]
    fn test_ctrl_enable_disable() {
        let mut dev = create_test_device_with_memory();

        // Initially disabled.
        assert!(!dev.is_enabled());
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(!status.cmd_buf_run());
        assert!(!status.evt_log_run());

        // Configure valid buffer bases before enabling.
        let cmd_base = CmdBufBase::new()
            .with_base_addr(0x0000 >> 12)
            .with_length(8);
        mmio_write64(
            &mut dev,
            MmioRegister::CMD_BUF_BASE.0 as u64,
            cmd_base.into_bits(),
        );
        let evt_base = EvtLogBase::new()
            .with_base_addr(0x1000 >> 12)
            .with_length(8);
        mmio_write64(
            &mut dev,
            MmioRegister::EVT_LOG_BASE.0 as u64,
            evt_base.into_bits(),
        );

        // Enable IOMMU with command buffer and event log.
        let ctrl = IommuCtrl::new()
            .with_iommu_en(true)
            .with_cmd_buf_en(true)
            .with_evt_log_en(true);
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_CTRL.0 as u64,
            ctrl.into_bits(),
        );

        assert!(dev.is_enabled());
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(status.cmd_buf_run(), "CmdBufRun should be set");
        assert!(status.evt_log_run(), "EvtLogRun should be set");

        // Disable IOMMU.
        mmio_write64(&mut dev, MmioRegister::IOMMU_CTRL.0 as u64, 0);
        assert!(!dev.is_enabled());
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(!status.cmd_buf_run(), "CmdBufRun should be clear");
        assert!(!status.evt_log_run(), "EvtLogRun should be clear");
    }

    #[test]
    fn test_devtab_base_readback() {
        let mut dev = create_test_device();
        let base = DevTabBase::new()
            .with_size(0x1FF) // Max table size
            .with_base_addr(0x100_0000 >> 12);
        let val = base.into_bits();

        mmio_write64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64, val);
        let readback = mmio_read64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64);
        assert_eq!(readback, val, "DevTabBase should round-trip");
    }

    #[test]
    fn test_cmdbuf_base_readback() {
        let mut dev = create_test_device();
        let base = CmdBufBase::new()
            .with_length(8) // 256 entries
            .with_base_addr(0x200_0000 >> 12);
        let val = base.into_bits();

        mmio_write64(&mut dev, MmioRegister::CMD_BUF_BASE.0 as u64, val);
        let readback = mmio_read64(&mut dev, MmioRegister::CMD_BUF_BASE.0 as u64);
        assert_eq!(readback, val, "CmdBufBase should round-trip");
    }

    #[test]
    fn test_devtab_base_locked_when_enabled() {
        let mut dev = create_test_device();

        // Write DevTabBase while disabled — should work.
        let val1 = 0x1234_5000;
        mmio_write64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64, val1);
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64),
            val1
        );

        // Enable IOMMU.
        let ctrl = IommuCtrl::new().with_iommu_en(true);
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_CTRL.0 as u64,
            ctrl.into_bits(),
        );

        // Write DevTabBase while enabled — should be ignored.
        let val2 = 0xAAAA_0000;
        mmio_write64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64, val2);
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64),
            val1,
            "DevTabBase should not change while IOMMU is enabled"
        );
    }

    #[test]
    fn test_status_rw1c() {
        let mut dev = create_test_device();

        // Manually set some status bits (simulating events).
        let status = IommuStatus::new()
            .with_evt_overflow(true)
            .with_evt_log_int(true)
            .with_com_wait_int(true);
        dev.shared.state.write().iommu_status = status.into_bits();

        // Read back — all bits should be set.
        let readback =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(readback.evt_overflow());
        assert!(readback.evt_log_int());
        assert!(readback.com_wait_int());

        // Clear only EvtLogInt by writing 1 to it.
        let clear = IommuStatus::new().with_evt_log_int(true);
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_STATUS.0 as u64,
            clear.into_bits(),
        );

        let readback =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(readback.evt_overflow(), "evt_overflow should remain set");
        assert!(!readback.evt_log_int(), "evt_log_int should be cleared");
        assert!(readback.com_wait_int(), "com_wait_int should remain set");
    }

    #[test]
    fn test_32bit_mmio_access() {
        let mut dev = create_test_device();

        // Write a 64-bit value to DevTabBase.
        let val: u64 = 0xDEAD_BEEF_1234_51FF;
        mmio_write64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64, val);

        // Read back as two 32-bit halves.
        let low = mmio_read32(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64);
        let high = mmio_read32(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64 + 4);
        let combined = (high as u64) << 32 | low as u64;
        assert_eq!(
            combined, val,
            "32-bit halves should reconstruct the 64-bit value"
        );
    }

    #[test]
    fn test_32bit_mmio_write() {
        let mut dev = create_test_device();

        // Write lower 32 bits, then upper 32 bits.
        mmio_write32(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64, 0x1234_51FF);
        mmio_write32(
            &mut dev,
            MmioRegister::DEV_TAB_BASE.0 as u64 + 4,
            0xDEAD_BEEF,
        );

        let val = mmio_read64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64);
        assert_eq!(val, 0xDEAD_BEEF_1234_51FF);
    }

    #[test]
    fn test_static_mmio_region() {
        let mut dev = create_test_device();
        let regions = dev.get_static_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].0, "amd-iommu-mmio");
        assert_eq!(*regions[0].1.start(), TEST_MMIO_BASE);
        assert_eq!(*regions[0].1.end(), TEST_MMIO_BASE + MMIO_REGION_SIZE - 1);
    }

    #[test]
    fn test_initial_state() {
        let mut dev = create_test_device();

        // All registers should be zero initially.
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64),
            0
        );
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::CMD_BUF_BASE.0 as u64),
            0
        );
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::EVT_LOG_BASE.0 as u64),
            0
        );
        assert_eq!(mmio_read64(&mut dev, MmioRegister::IOMMU_CTRL.0 as u64), 0);
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::CMD_BUF_HEAD.0 as u64),
            0
        );
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::CMD_BUF_TAIL.0 as u64),
            0
        );
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::EVT_LOG_HEAD.0 as u64),
            0
        );
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64),
            0
        );
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64),
            0
        );

        // ExtFeat is non-zero (read-only).
        assert_ne!(mmio_read64(&mut dev, MmioRegister::EXT_FEAT.0 as u64), 0);
    }

    #[test]
    fn test_evt_log_head_write() {
        let mut dev = create_test_device();

        // Software writes EvtLogHead to acknowledge consumed events.
        let head = EvtLogHead::new().with_head_ptr(5);
        mmio_write64(
            &mut dev,
            MmioRegister::EVT_LOG_HEAD.0 as u64,
            head.into_bits(),
        );
        let readback = mmio_read64(&mut dev, MmioRegister::EVT_LOG_HEAD.0 as u64);
        assert_eq!(readback, head.into_bits());
    }

    // =========================================================================
    // Event Log Tests (1D)
    // =========================================================================

    /// Create a device with backing guest memory for command buffer / event log
    /// testing. The memory layout:
    ///   0x0000..0x0FFF = command buffer (4KB = 256 entries at log2=8)
    ///   0x1000..0x1FFF = event log (4KB = 256 entries at log2=8)
    ///   0x2000..0x2FFF = scratch space (for COMPLETION_WAIT store data)
    fn create_test_device_with_memory() -> AmdIommuDevice {
        let guest_memory = GuestMemory::allocate(0x10000);
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target())
    }

    /// Configure and enable the IOMMU with command buffer and event log.
    fn setup_iommu_enabled(dev: &mut AmdIommuDevice) {
        // Command buffer at GPA 0x0000, 256 entries (log2=8).
        let cmd_base = CmdBufBase::new()
            .with_base_addr(0x0000 >> 12)
            .with_length(8);
        mmio_write64(
            dev,
            MmioRegister::CMD_BUF_BASE.0 as u64,
            cmd_base.into_bits(),
        );

        // Event log at GPA 0x1000, 256 entries (log2=8).
        let evt_base = EvtLogBase::new()
            .with_base_addr(0x1000 >> 12)
            .with_length(8);
        mmio_write64(
            dev,
            MmioRegister::EVT_LOG_BASE.0 as u64,
            evt_base.into_bits(),
        );

        // Enable IOMMU with command buffer, event log, and event interrupt.
        let ctrl = IommuCtrl::new()
            .with_iommu_en(true)
            .with_cmd_buf_en(true)
            .with_evt_log_en(true)
            .with_evt_int_en(true)
            .with_com_wait_int_en(true);
        mmio_write64(dev, MmioRegister::IOMMU_CTRL.0 as u64, ctrl.into_bits());
    }

    #[test]
    fn test_evtlog_write_and_read() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Write an IO_PAGE_FAULT event.
        let event = EventEntry::io_page_fault(
            0x0100, // device_id
            0x0042, // domain_id
            false,  // is_interrupt
            true,   // is_write
            0xDEAD_0000,
        );
        dev.write_event(event);

        // Verify tail advanced by one entry (16 bytes).
        let tail =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));
        assert_eq!(tail.tail_ptr(), 1, "tail should advance by one entry");

        // Read the event back from guest memory.
        let readback: EventEntry = dev
            .shared
            .guest_memory
            .read_plain(0x1000) // event log base
            .expect("should read event");
        assert_eq!(readback.event_code(), EventCode::IO_PAGE_FAULT);
        assert_eq!(readback.device_id(), 0x0100);
    }

    #[test]
    fn test_evtlog_interrupt_signal() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        let event = EventEntry::io_page_fault(0x0100, 0x0042, false, false, 0x1000);
        dev.write_event(event);

        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(status.evt_log_int(), "EventLogInt should be set");
    }

    #[test]
    fn test_evtlog_full() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Event log has 256 entries (log2=8). Fill it by writing 255 events
        // (one slot is always empty to distinguish full from empty).
        for i in 0..255 {
            let event = EventEntry::io_page_fault(i as u16, 0x0001, false, false, 0x0);
            dev.write_event(event);
        }

        // Verify tail is at 255 entries * 16 bytes / 16 = 255.
        let tail =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));
        assert_eq!(tail.tail_ptr() as u64, 255);

        // Clear EventLogInt so we can check the overflow path clearly.
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_STATUS.0 as u64,
            IommuStatus::new().with_evt_log_int(true).into_bits(),
        );

        // One more event should trigger overflow.
        let event = EventEntry::io_page_fault(0xFFFF, 0x0001, false, false, 0x0);
        dev.write_event(event);

        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(status.evt_overflow(), "EventOverflow should be set");

        // Tail should NOT have advanced.
        let tail =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));
        assert_eq!(tail.tail_ptr() as u64, 255);
    }

    #[test]
    fn test_evtlog_head_frees_space() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Fill the event log (255 entries).
        for i in 0..255 {
            let event = EventEntry::io_page_fault(i as u16, 0x0001, false, false, 0x0);
            dev.write_event(event);
        }

        // Advance head to consume one event.
        let head = EvtLogHead::new().with_head_ptr(1);
        mmio_write64(
            &mut dev,
            MmioRegister::EVT_LOG_HEAD.0 as u64,
            head.into_bits(),
        );

        // Now there's space for one more event.
        let event = EventEntry::io_page_fault(0xBEEF, 0x0001, false, false, 0x0);
        dev.write_event(event);

        // Tail should have advanced (wrapped to 0).
        let tail =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));
        assert_eq!(tail.tail_ptr(), 0, "tail should wrap to 0");

        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(!status.evt_overflow(), "no overflow after head frees space");
    }

    // =========================================================================
    // Command Buffer Tests (1C)
    // =========================================================================

    /// Write a command entry to the command buffer at the given entry index.
    fn write_cmd(dev: &AmdIommuDevice, index: u64, entry: &CommandEntry) {
        let gpa = index * 16; // command buffer at GPA 0
        dev.shared
            .guest_memory
            .write_plain(gpa, entry)
            .expect("should write command");
    }

    /// Build a COMPLETION_WAIT command with the store flag set.
    fn completion_wait_store(store_addr: u64, store_data: u64) -> CommandEntry {
        // Build dw0/dw1 manually: opcode 0x01 in dw1[31:28], store flag in dw0[0],
        // store address split across dw0[31:3] and dw1[19:0].
        let dw0 = 0x01u32 // s=1
            | ((store_addr >> 3) as u32 & 0x1FFF_FFFF) << 3; // StoreAddr[31:3]
        let dw1 = (CommandOpcode::COMPLETION_WAIT.0 as u32) << 28
            | ((store_addr >> 32) as u32 & 0x000F_FFFF); // StoreAddr[51:32]
        CommandEntry {
            dw0,
            dw1,
            dw2: store_data as u32,
            dw3: (store_data >> 32) as u32,
        }
    }

    /// Build a COMPLETION_WAIT command with the interrupt flag set.
    fn completion_wait_interrupt() -> CommandEntry {
        CommandEntry {
            dw0: 0x02, // i=1 (bit 1)
            dw1: (CommandOpcode::COMPLETION_WAIT.0 as u32) << 28,
            dw2: 0,
            dw3: 0,
        }
    }

    /// Build an INVALIDATE_DEVTAB_ENTRY command for a given device ID.
    fn invalidate_devtab_entry(device_id: u16) -> CommandEntry {
        CommandEntry {
            dw0: device_id as u32,
            dw1: (CommandOpcode::INVALIDATE_DEVTAB_ENTRY.0 as u32) << 28,
            dw2: 0,
            dw3: 0,
        }
    }

    /// Poke CmdBufTail to trigger command processing up to the given entry index.
    fn poke_tail(dev: &mut AmdIommuDevice, entry_index: u64) {
        let tail = CmdBufTail::new().with_tail_ptr(entry_index as u32);
        mmio_write64(dev, MmioRegister::CMD_BUF_TAIL.0 as u64, tail.into_bits());
    }

    #[test]
    fn test_cmdbuf_basic_consumption() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Write INVALIDATE_DEVTAB_ENTRY + COMPLETION_WAIT(S=1) to command buffer.
        let store_addr: u64 = 0x2000;
        let store_data: u64 = 0xCAFE_BABE_1234_5678;
        write_cmd(&dev, 0, &invalidate_devtab_entry(0x0100));
        write_cmd(&dev, 1, &completion_wait_store(store_addr, store_data));

        // Poke tail to entry 2 (two commands).
        poke_tail(&mut dev, 2);

        // Head should have advanced to 2.
        let head =
            CmdBufHead::from_bits(mmio_read64(&mut dev, MmioRegister::CMD_BUF_HEAD.0 as u64));
        assert_eq!(head.head_ptr(), 2, "head should advance to 2");

        // Verify store data was written to guest memory.
        let readback: u64 = dev
            .shared
            .guest_memory
            .read_plain(store_addr)
            .expect("should read store data");
        assert_eq!(readback, store_data, "store data should match");
    }

    #[test]
    fn test_cmdbuf_completion_wait_interrupt() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        write_cmd(&dev, 0, &completion_wait_interrupt());
        poke_tail(&mut dev, 1);

        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(
            status.com_wait_int(),
            "ComWaitInt should be set after COMPLETION_WAIT with I=1"
        );
    }

    #[test]
    fn test_cmdbuf_completion_wait_delivers_msi() {
        // Create device with a connected MSI controller to verify actual
        // MSI delivery (not just status bit).
        let guest_memory = GuestMemory::allocate(0x10000);
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let msi_controller = pci_core::test_helpers::TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        // Enable MSI on the IOMMU's PCI config space.
        // The MSI capability is the second capability. Find its offset.
        let iommu_cap_header = pci_read(&mut dev, 0x40);
        let msi_cap_offset = ((iommu_cap_header >> 8) & 0xFF) as u16;
        assert_ne!(msi_cap_offset, 0, "MSI capability should exist");

        // Configure MSI: address = 0xFEE00000, data = 0x41.
        // Write address low (offset + 4).
        let _ = dev.pci_cfg_write(msi_cap_offset + 4, 0xFEE0_0000);
        // Write address high (offset + 8) — 64-bit capable.
        let _ = dev.pci_cfg_write(msi_cap_offset + 8, 0);
        // Write data (offset + 12 for 64-bit).
        let _ = dev.pci_cfg_write(msi_cap_offset + 12, 0x41);
        // Enable MSI (write control register at offset + 0, set enable bit).
        let control = pci_read(&mut dev, msi_cap_offset);
        let _ = dev.pci_cfg_write(msi_cap_offset, control | (1 << 16));

        setup_iommu_enabled(&mut dev);

        // No MSI should have been delivered yet.
        assert!(
            msi_controller.get_next_interrupt().is_none(),
            "no MSI should be pending before COMPLETION_WAIT"
        );

        // Issue COMPLETION_WAIT with I=1 (interrupt flag).
        write_cmd(&dev, 0, &completion_wait_interrupt());
        poke_tail(&mut dev, 1);

        // Verify ComWaitInt is set in status.
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(status.com_wait_int(), "ComWaitInt should be set");

        // Verify the MSI was actually delivered to the controller.
        let msi = msi_controller
            .get_next_interrupt()
            .expect("MSI should have been delivered");
        assert_eq!(msi.0, 0xFEE0_0000, "MSI address should match");
        assert_eq!(msi.1, 0x41, "MSI data should match");
    }

    #[test]
    fn test_cmdbuf_wrap() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Command buffer has 256 entries (log2=8).
        // Write a command at position 255 (last entry) and at position 0 (wrap).
        let store_addr: u64 = 0x2000;
        let store_data: u64 = 0xDEAD_BEEF;

        // Set head to 255 (last entry).
        dev.shared.state.write().cmd_buf_head = CmdBufHead::new().with_head_ptr(255).into_bits();

        // Write command at entry 255 and entry 0 (wraps).
        write_cmd(&dev, 255, &invalidate_devtab_entry(0x01));
        write_cmd(&dev, 0, &completion_wait_store(store_addr, store_data));

        // Set tail to 1 (which is past the wrap point from 255).
        poke_tail(&mut dev, 1);

        // Head should have wrapped to 1.
        let head =
            CmdBufHead::from_bits(mmio_read64(&mut dev, MmioRegister::CMD_BUF_HEAD.0 as u64));
        assert_eq!(head.head_ptr(), 1, "head should wrap to 1");

        // Verify store data was written.
        let readback: u64 = dev
            .shared
            .guest_memory
            .read_plain(store_addr)
            .expect("should read store data");
        assert_eq!(readback, store_data, "store data should match after wrap");
    }

    #[test]
    fn test_cmdbuf_unknown_opcode() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Write a command with an invalid opcode (0x0F).
        let bad_cmd = CommandEntry {
            dw0: 0,
            dw1: 0xF000_0000, // opcode = 0x0F
            dw2: 0,
            dw3: 0,
        };
        write_cmd(&dev, 0, &bad_cmd);
        poke_tail(&mut dev, 1);

        // CmdBufRun should be cleared (command buffer halted).
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(
            !status.cmd_buf_run(),
            "CmdBufRun should be clear after illegal command"
        );

        // Head should NOT have advanced (command was not consumed).
        let head =
            CmdBufHead::from_bits(mmio_read64(&mut dev, MmioRegister::CMD_BUF_HEAD.0 as u64));
        assert_eq!(
            head.head_ptr(),
            0,
            "head should not advance on illegal command"
        );

        // Event log should have an ILLEGAL_COMMAND_ERROR entry.
        let tail =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));
        assert_eq!(tail.tail_ptr(), 1, "event log should have one entry");

        let event: EventEntry = dev
            .shared
            .guest_memory
            .read_plain(0x1000) // event log base
            .expect("should read event");
        assert_eq!(event.event_code(), EventCode::ILLEGAL_COMMAND_ERROR);
    }

    #[test]
    fn test_cmdbuf_not_processed_when_disabled() {
        let mut dev = create_test_device_with_memory();
        // Do NOT enable the IOMMU.

        // Configure command buffer base (while disabled, this is allowed).
        let cmd_base = CmdBufBase::new()
            .with_base_addr(0x0000 >> 12)
            .with_length(8);
        mmio_write64(
            &mut dev,
            MmioRegister::CMD_BUF_BASE.0 as u64,
            cmd_base.into_bits(),
        );

        let store_addr: u64 = 0x2000;
        let store_data: u64 = 0xBEEF;
        write_cmd(&dev, 0, &completion_wait_store(store_addr, store_data));
        poke_tail(&mut dev, 1);

        // Head should not advance (IOMMU disabled).
        let head =
            CmdBufHead::from_bits(mmio_read64(&mut dev, MmioRegister::CMD_BUF_HEAD.0 as u64));
        assert_eq!(
            head.head_ptr(),
            0,
            "head should not advance when IOMMU disabled"
        );

        // Store data should NOT have been written.
        let readback: u64 = dev
            .shared
            .guest_memory
            .read_plain(store_addr)
            .expect("should read");
        assert_eq!(
            readback, 0,
            "store data should not be written when disabled"
        );
    }

    #[test]
    fn test_cmdbuf_multiple_commands() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Write 5 commands: 4 invalidates + 1 completion wait.
        for i in 0..4 {
            write_cmd(&dev, i, &invalidate_devtab_entry(i as u16));
        }
        let store_addr: u64 = 0x2000;
        let store_data: u64 = 0x42;
        write_cmd(&dev, 4, &completion_wait_store(store_addr, store_data));

        poke_tail(&mut dev, 5);

        // Head should advance to 5.
        let head =
            CmdBufHead::from_bits(mmio_read64(&mut dev, MmioRegister::CMD_BUF_HEAD.0 as u64));
        assert_eq!(
            head.head_ptr(),
            5,
            "head should advance past all 5 commands"
        );

        // Verify store data.
        let readback: u64 = dev
            .shared
            .guest_memory
            .read_plain(store_addr)
            .expect("should read");
        assert_eq!(readback, store_data);
    }

    #[test]
    fn test_cmdbuf_invalidate_all() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // INVALIDATE_IOMMU_ALL is a no-op but should be consumed successfully.
        let cmd = CommandEntry {
            dw0: 0,
            dw1: (CommandOpcode::INVALIDATE_IOMMU_ALL.0 as u32) << 28,
            dw2: 0,
            dw3: 0,
        };
        write_cmd(&dev, 0, &cmd);
        poke_tail(&mut dev, 1);

        let head =
            CmdBufHead::from_bits(mmio_read64(&mut dev, MmioRegister::CMD_BUF_HEAD.0 as u64));
        assert_eq!(
            head.head_ptr(),
            1,
            "INVALIDATE_IOMMU_ALL should be consumed"
        );
    }

    #[test]
    fn test_cmdbuf_completion_wait_store_and_interrupt() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Build a command with both S and I flags set.
        let store_addr: u64 = 0x2000;
        let store_data: u64 = 0xABCD_EF01;
        let dw0 = 0x03u32 // s=1 (bit 0), i=1 (bit 1)
            | ((store_addr >> 3) as u32 & 0x1FFF_FFFF) << 3;
        let dw1 = (CommandOpcode::COMPLETION_WAIT.0 as u32) << 28
            | ((store_addr >> 32) as u32 & 0x000F_FFFF);
        let cmd = CommandEntry {
            dw0,
            dw1,
            dw2: store_data as u32,
            dw3: (store_data >> 32) as u32,
        };
        write_cmd(&dev, 0, &cmd);
        poke_tail(&mut dev, 1);

        // Verify store data.
        let readback: u64 = dev
            .shared
            .guest_memory
            .read_plain(store_addr)
            .expect("should read");
        assert_eq!(readback, store_data);

        // Verify ComWaitInt is set.
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(status.com_wait_int(), "ComWaitInt should be set");
    }

    // =========================================================================
    // DTE Lookup and DMA Translation Tests (1E)
    // =========================================================================

    /// Create a test device with enough guest memory for device table +
    /// page tables (1MB).
    fn create_test_device_for_translation() -> AmdIommuDevice {
        let guest_memory = GuestMemory::allocate(0x10_0000); // 1MB
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target())
    }

    /// Set up the IOMMU with a device table at a given GPA.
    /// Returns the device table base GPA.
    fn setup_iommu_with_devtab(dev: &mut AmdIommuDevice, devtab_gpa: u64, num_entries: u16) {
        // Device table size field: (size+1) * 4096 / 32 = num_entries
        // → size = num_entries * 32 / 4096 - 1
        let size = (num_entries as u64 * DTE_SIZE as u64 / 4096).saturating_sub(1) as u16;
        let dtb = DevTabBase::new()
            .with_base_addr(devtab_gpa >> 12)
            .with_size(size);
        mmio_write64(dev, MmioRegister::DEV_TAB_BASE.0 as u64, dtb.into_bits());

        // Command buffer at 0x8_0000, event log at 0x9_0000.
        let cmd_base = CmdBufBase::new()
            .with_base_addr(0x8_0000 >> 12)
            .with_length(8);
        mmio_write64(
            dev,
            MmioRegister::CMD_BUF_BASE.0 as u64,
            cmd_base.into_bits(),
        );
        let evt_base = EvtLogBase::new()
            .with_base_addr(0x9_0000 >> 12)
            .with_length(8);
        mmio_write64(
            dev,
            MmioRegister::EVT_LOG_BASE.0 as u64,
            evt_base.into_bits(),
        );

        let ctrl = IommuCtrl::new()
            .with_iommu_en(true)
            .with_cmd_buf_en(true)
            .with_evt_log_en(true);
        mmio_write64(dev, MmioRegister::IOMMU_CTRL.0 as u64, ctrl.into_bits());
    }

    /// Write a DTE into guest memory at the device table.
    fn write_dte(dev: &AmdIommuDevice, devtab_gpa: u64, device_id: u16, dte: &Dte) {
        let dte_gpa = devtab_gpa + (device_id as u64) * (DTE_SIZE as u64);
        dev.shared
            .guest_memory
            .write_plain(dte_gpa, dte)
            .expect("write DTE");
    }

    /// Write a PTE into guest memory.
    fn write_pte(dev: &AmdIommuDevice, table_gpa: u64, index: usize, pte: &IommuPte) {
        let pte_gpa = table_gpa + (index as u64) * 8;
        dev.shared
            .guest_memory
            .write_plain(pte_gpa, pte)
            .expect("write PTE");
    }

    /// Build a simple DTE with translation enabled.
    fn make_dte_with_translation(
        pt_root_gpa: u64,
        levels: u8,
        ir: bool,
        iw: bool,
        domain_id: u16,
    ) -> Dte {
        use spec::dte::*;
        Dte {
            dw0: DteDw0::new()
                .with_v(true)
                .with_tv(true)
                .with_mode(levels)
                .with_host_pt_root_ptr(pt_root_gpa >> 12)
                .with_ir(ir)
                .with_iw(iw),
            dw1: DteDw1::new().with_domain_id(domain_id),
            dw2: DteDw2::new(),
            dw3: 0,
        }
    }

    /// Build a DTE with V=1, TV=0 (DMA aborted per spec Table 8).
    fn make_dte_v1_tv0() -> Dte {
        use spec::dte::*;
        Dte {
            dw0: DteDw0::new().with_v(true).with_tv(false),
            dw1: DteDw1::new(),
            dw2: DteDw2::new(),
            dw3: 0,
        }
    }

    #[test]
    fn test_translate_tv0_aborts() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let dte = make_dte_v1_tv0();
        write_dte(&dev, devtab_gpa, 0x01, &dte);

        // Per spec Table 8: V=1, TV=0 → table walk terminated (target abort).
        let result = dev.translate(0x01, 0x1234_5000, false);
        assert!(result.is_err());
        match result.unwrap_err() {
            IommuFault::IoPageFault { device_id, .. } => {
                assert_eq!(device_id, 0x01);
            }
            other => panic!("expected IoPageFault, got {:?}", other),
        }
    }

    #[test]
    fn test_translate_passthrough_mode0() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // DTE with V=1, TV=1, Mode=0 → pass-through.
        let dte = make_dte_with_translation(0, 0, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x02, &dte);

        let gpa = dev.translate(0x02, 0xABCD_0000, false).unwrap();
        assert_eq!(gpa, 0xABCD_0000);
    }

    #[test]
    fn test_translate_iommu_disabled() {
        let dev = create_test_device_for_translation();
        // Don't enable the IOMMU.

        // Should pass through (identity mapping) when IOMMU is off.
        let gpa = dev.translate(0x01, 0xDEAD_BEEF, false).unwrap();
        assert_eq!(gpa, 0xDEAD_BEEF);
    }

    #[test]
    fn test_translate_dte_v0_passthrough() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // DTE at device_id=0x03 is all zeros (V=0).
        // Per spec §2.2.2 Table 8: V=0 → passthrough.
        let gpa = dev.translate(0x03, 0x1000, false).unwrap();
        assert_eq!(gpa, 0x1000);
    }

    #[test]
    fn test_translate_device_id_out_of_range() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        // Only 128 entries.
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 128);

        let result = dev.translate(200, 0x1000, false);
        assert!(result.is_err());
        match result.unwrap_err() {
            IommuFault::IllegalDevTableEntry { device_id, .. } => {
                assert_eq!(device_id, 200);
            }
            other => panic!("expected IllegalDevTableEntry, got {:?}", other),
        }
    }

    #[test]
    fn test_translate_4level() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // Set up a 4-level page table mapping IOVA 0x0 → GPA 0xA_0000.
        //
        // Memory layout:
        //   L4 table at 0x2_0000
        //   L3 table at 0x3_0000
        //   L2 table at 0x4_0000
        //   L1 table at 0x5_0000
        //   Target page at GPA 0xA_0000
        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xA_0000u64;

        // L4[0] → L3 table (NextLevel=3, pointing to L3)
        let pde_l4 = IommuPte::new()
            .with_pr(true)
            .with_next_level(3)
            .with_address(l3_gpa >> 12)
            .with_ir(true)
            .with_iw(true);
        write_pte(&dev, l4_gpa, 0, &pde_l4);

        // L3[0] → L2 table (NextLevel=2, pointing to L2)
        let pde_l3 = IommuPte::new()
            .with_pr(true)
            .with_next_level(2)
            .with_address(l2_gpa >> 12)
            .with_ir(true)
            .with_iw(true);
        write_pte(&dev, l3_gpa, 0, &pde_l3);

        // L2[0] → L1 table (NextLevel=1, pointing to L1)
        let pde_l2 = IommuPte::new()
            .with_pr(true)
            .with_next_level(1)
            .with_address(l1_gpa >> 12)
            .with_ir(true)
            .with_iw(true);
        write_pte(&dev, l2_gpa, 0, &pde_l2);

        // L1[0] → target page (leaf, NextLevel=0)
        let pte = IommuPte::new()
            .with_pr(true)
            .with_next_level(0)
            .with_address(target_gpa >> 12)
            .with_ir(true)
            .with_iw(true);
        write_pte(&dev, l1_gpa, 0, &pte);

        // DTE: 4-level page table, root at L4.
        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Translate IOVA 0x0 → GPA 0xA_0000.
        let gpa = dev.translate(0x10, 0x0, false).unwrap();
        assert_eq!(gpa, target_gpa);
    }

    #[test]
    fn test_translate_4level_with_offset() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xA_0000u64;

        write_pte(
            &dev,
            l4_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l2_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // IOVA 0x123 → GPA 0xA_0123 (page offset preserved).
        let gpa = dev.translate(0x10, 0x123, false).unwrap();
        assert_eq!(gpa, target_gpa | 0x123);
    }

    #[test]
    fn test_translate_3level() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // 3-level page table: L3→L2→L1→page
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xA_0000u64;

        write_pte(
            &dev,
            l3_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l2_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let dte = make_dte_with_translation(l3_gpa, 3, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let gpa = dev.translate(0x10, 0x0, false).unwrap();
        assert_eq!(gpa, target_gpa);
    }

    #[test]
    fn test_translate_2mb_large_page() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // 4-level page table with a 2MB large page at L2.
        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        // The large page maps IOVA 0x0 to GPA 0x20_0000 (2MB aligned).
        let large_page_gpa = 0x20_0000u64;

        write_pte(
            &dev,
            l4_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        // L2[0] is a leaf (NextLevel=0 at level 2 = 2MB large page).
        write_pte(
            &dev,
            l2_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0) // leaf at level 2 = 2MB page
                .with_address(large_page_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // IOVA 0x0 → GPA 0x20_0000.
        let gpa = dev.translate(0x10, 0x0, false).unwrap();
        assert_eq!(gpa, large_page_gpa);

        // IOVA 0x1_2345 → GPA 0x21_2345 (offset within 2MB page preserved).
        let gpa = dev.translate(0x10, 0x1_2345, false).unwrap();
        assert_eq!(gpa, large_page_gpa | 0x1_2345);
    }

    /// Mirror Linux's `PAGE_SIZE_PTE` from `amd_iommu_types.h`:
    /// `((address) | (pgsize - 1)) & ~(pgsize >> 1) & PM_ADDR_MASK`.
    fn linux_page_size_pte_addr(paddr: u64, pgsize: u64) -> u64 {
        const PM_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
        ((paddr | (pgsize - 1)) & !(pgsize >> 1)) & PM_ADDR_MASK
    }

    /// Linux's `PAGE_SIZE_PTE_COUNT`:
    /// `1 << ((__ffs(pgsize) - 12) % 9)`.
    fn linux_pte_count(pgsize: u64) -> usize {
        let log2 = pgsize.trailing_zeros() as usize;
        1 << ((log2 - 12) % 9)
    }

    /// End-to-end test for AMD IOMMU "mode 7" replicated large-page PTEs as
    /// used by the Linux driver for non-default page sizes (e.g. 16 KiB).
    ///
    /// Linux's `iommu_v1_map_pages` writes a 16 KiB mapping as four identical
    /// PTEs at the level-1 page table, each with `NextLevel = 7`, with the
    /// page size encoded in the trailing 1-bits of the address field.
    #[test]
    fn test_translate_mode7_16k_large_page() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // 4-level page table. 16 KiB mapping at L1 (mode-7 encoded).
        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let pgsize = 0x4000u64; // 16 KiB
        let target_gpa = 0x10_0000u64; // 16 KiB-aligned
        let iova_base = 0x40_0000u64; // 16 KiB-aligned

        // Build the chain L4 → L3 → L2 → L1.
        write_pte(
            &dev,
            l4_gpa,
            IommuPte::va_index(iova_base, 4),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            IommuPte::va_index(iova_base, 3),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l2_gpa,
            IommuPte::va_index(iova_base, 2),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        // Linux writes the same level-7 PTE into `count` consecutive L1 slots.
        let count = linux_pte_count(pgsize);
        assert_eq!(count, 4);
        let addr_field = linux_page_size_pte_addr(target_gpa, pgsize);
        let leaf = IommuPte::new()
            .with_pr(true)
            .with_next_level(7) // mode-7 large-page marker
            .with_address(addr_field >> 12)
            .with_ir(true)
            .with_iw(true)
            .with_fc(true);
        let base_index = IommuPte::va_index(iova_base, 1);
        for i in 0..count {
            write_pte(&dev, l1_gpa, base_index + i, &leaf);
        }

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Translate every 4 KiB offset within the 16 KiB region and verify
        // the GPA is what Linux intended (preserving the IOVA offset
        // within the 16 KiB large page). Without the mode-7 fix, GPAs
        // would alias to the wrong 4 KiB sub-page.
        for off in [0u64, 0x123, 0x1000, 0x1234, 0x2000, 0x2abc, 0x3000, 0x3fff] {
            let iova = iova_base + off;
            let gpa = dev.translate(0x10, iova, false).expect("translate ok");
            assert_eq!(
                gpa,
                target_gpa + off,
                "iova {:#x} should map to {:#x}, got {:#x}",
                iova,
                target_gpa + off,
                gpa
            );
        }
    }

    /// Same as above but for a 64 KiB page (16 replicated PTEs at L1) —
    /// the typical size for NVMe IO submission queues.
    #[test]
    fn test_translate_mode7_64k_large_page() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let pgsize = 0x10000u64; // 64 KiB
        let target_gpa = 0x80_0000u64;
        let iova_base = 0xC0_0000u64;

        write_pte(
            &dev,
            l4_gpa,
            IommuPte::va_index(iova_base, 4),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            IommuPte::va_index(iova_base, 3),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l2_gpa,
            IommuPte::va_index(iova_base, 2),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let count = linux_pte_count(pgsize);
        assert_eq!(count, 16);
        let addr_field = linux_page_size_pte_addr(target_gpa, pgsize);
        let leaf = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .with_ir(true)
            .with_iw(true);
        let base_index = IommuPte::va_index(iova_base, 1);
        for i in 0..count {
            write_pte(&dev, l1_gpa, base_index + i, &leaf);
        }

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Endpoints, middles, and crossing-the-4 KiB-boundary offsets.
        for off in [0u64, 0xFFF, 0x1000, 0x4000, 0x7000, 0xABCD, 0xFFFF] {
            let iova = iova_base + off;
            let gpa = dev.translate(0x10, iova, false).expect("translate ok");
            assert_eq!(gpa, target_gpa + off);
        }
    }

    /// Mode-7 large page at L2 (4 MiB) — Linux writes 2 replicated entries
    /// in the level-2 table with the size encoded in the address field.
    #[test]
    fn test_translate_mode7_4m_at_l2() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let pgsize = 0x40_0000u64; // 4 MiB
        let target_gpa = 0x100_0000u64;
        let iova_base = 0x200_0000u64;

        write_pte(
            &dev,
            l4_gpa,
            IommuPte::va_index(iova_base, 4),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            IommuPte::va_index(iova_base, 3),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let count = linux_pte_count(pgsize);
        assert_eq!(count, 2);
        let addr_field = linux_page_size_pte_addr(target_gpa, pgsize);
        let leaf = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .with_ir(true)
            .with_iw(true);
        let base_index = IommuPte::va_index(iova_base, 2);
        for i in 0..count {
            write_pte(&dev, l2_gpa, base_index + i, &leaf);
        }

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Test a range of offsets across the 4 MiB region.
        for off in [0u64, 0x1FFFFF, 0x20_0000, 0x3F_FFFF] {
            let iova = iova_base + off;
            let gpa = dev.translate(0x10, iova, false).expect("translate ok");
            assert_eq!(
                gpa,
                target_gpa + off,
                "iova {:#x} expected {:#x} got {:#x}",
                iova,
                target_gpa + off,
                gpa
            );
        }
    }

    /// Read-only mode-7 page: writes must fault, reads must succeed.
    #[test]
    fn test_translate_mode7_read_only_denies_write() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let pgsize = 0x4000u64; // 16 KiB
        let target_gpa = 0x10_0000u64;
        let iova_base = 0x40_0000u64;

        write_pte(
            &dev,
            l4_gpa,
            IommuPte::va_index(iova_base, 4),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            IommuPte::va_index(iova_base, 3),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l2_gpa,
            IommuPte::va_index(iova_base, 2),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let count = linux_pte_count(pgsize);
        let addr_field = linux_page_size_pte_addr(target_gpa, pgsize);
        let leaf = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .with_ir(true) // read allowed
            .with_iw(false); // write denied
        let base_index = IommuPte::va_index(iova_base, 1);
        for i in 0..count {
            write_pte(&dev, l1_gpa, base_index + i, &leaf);
        }

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Read succeeds at any offset.
        for off in [0u64, 0x2000, 0x3FFF] {
            let gpa = dev
                .translate(0x10, iova_base + off, false)
                .expect("read ok");
            assert_eq!(gpa, target_gpa + off);
        }
        // Write faults at any offset.
        for off in [0u64, 0x2000, 0x3FFF] {
            let err = dev
                .translate(0x10, iova_base + off, true)
                .expect_err("write must fault");
            assert!(matches!(
                err,
                IommuFault::IoPageFault { is_write: true, .. }
            ));
        }
    }

    /// Write-only mode-7 page: reads must fault.
    #[test]
    fn test_translate_mode7_write_only_denies_read() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let pgsize = 0x4000u64;
        let target_gpa = 0x10_0000u64;
        let iova_base = 0x40_0000u64;

        write_pte(
            &dev,
            l4_gpa,
            IommuPte::va_index(iova_base, 4),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            IommuPte::va_index(iova_base, 3),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l2_gpa,
            IommuPte::va_index(iova_base, 2),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let count = linux_pte_count(pgsize);
        let addr_field = linux_page_size_pte_addr(target_gpa, pgsize);
        let leaf = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .with_ir(false) // read denied
            .with_iw(true);
        let base_index = IommuPte::va_index(iova_base, 1);
        for i in 0..count {
            write_pte(&dev, l1_gpa, base_index + i, &leaf);
        }

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let gpa = dev.translate(0x10, iova_base + 0x1234, true).unwrap();
        assert_eq!(gpa, target_gpa + 0x1234);
        let err = dev
            .translate(0x10, iova_base + 0x1234, false)
            .expect_err("read must fault");
        assert!(matches!(
            err,
            IommuFault::IoPageFault {
                is_write: false,
                ..
            }
        ));
    }

    /// PDE without write permission AND-accumulates with a fully-permissive
    /// mode-7 PTE: writes must fault despite the leaf allowing them.
    #[test]
    fn test_translate_mode7_pde_permission_restricts() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let pgsize = 0x10000u64; // 64 KiB
        let target_gpa = 0x20_0000u64;
        let iova_base = 0x80_0000u64;

        write_pte(
            &dev,
            l4_gpa,
            IommuPte::va_index(iova_base, 4),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        // L3 PDE: writes denied — must propagate.
        write_pte(
            &dev,
            l3_gpa,
            IommuPte::va_index(iova_base, 3),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(false),
        );
        write_pte(
            &dev,
            l2_gpa,
            IommuPte::va_index(iova_base, 2),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let count = linux_pte_count(pgsize);
        let addr_field = linux_page_size_pte_addr(target_gpa, pgsize);
        let leaf = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .with_ir(true)
            .with_iw(true); // leaf allows write, but PDE doesn't
        let base_index = IommuPte::va_index(iova_base, 1);
        for i in 0..count {
            write_pte(&dev, l1_gpa, base_index + i, &leaf);
        }

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Read still works.
        let gpa = dev.translate(0x10, iova_base + 0x4321, false).unwrap();
        assert_eq!(gpa, target_gpa + 0x4321);
        // Write faults because of L3 PDE.
        let err = dev
            .translate(0x10, iova_base + 0x4321, true)
            .expect_err("write must fault");
        assert!(matches!(
            err,
            IommuFault::IoPageFault { is_write: true, .. }
        ));
    }

    /// Two adjacent 16 KiB mode-7 mappings must each return their own GPA;
    /// the page-size mask must not leak across region boundaries.
    #[test]
    fn test_translate_mode7_adjacent_mappings_independent() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let pgsize = 0x4000u64; // 16 KiB
        let target_a = 0x10_0000u64;
        let target_b = 0x20_0000u64;
        let iova_a = 0x40_0000u64;
        let iova_b = iova_a + pgsize; // immediately adjacent

        // L4 → L3 → L2 → L1.
        write_pte(
            &dev,
            l4_gpa,
            IommuPte::va_index(iova_a, 4),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            IommuPte::va_index(iova_a, 3),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l2_gpa,
            IommuPte::va_index(iova_a, 2),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let count = linux_pte_count(pgsize);
        // Region A: 4 replicated PTEs.
        let leaf_a = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(linux_page_size_pte_addr(target_a, pgsize) >> 12)
            .with_ir(true)
            .with_iw(true);
        let base_a = IommuPte::va_index(iova_a, 1);
        for i in 0..count {
            write_pte(&dev, l1_gpa, base_a + i, &leaf_a);
        }
        // Region B: 4 replicated PTEs, immediately after region A.
        let leaf_b = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(linux_page_size_pte_addr(target_b, pgsize) >> 12)
            .with_ir(true)
            .with_iw(true);
        let base_b = IommuPte::va_index(iova_b, 1);
        for i in 0..count {
            write_pte(&dev, l1_gpa, base_b + i, &leaf_b);
        }

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Last byte of region A.
        let gpa = dev
            .translate(0x10, iova_a + pgsize - 1, false)
            .expect("translate ok");
        assert_eq!(gpa, target_a + pgsize - 1);
        // First byte of region B — must go to target_b, not target_a + pgsize.
        let gpa = dev.translate(0x10, iova_b, false).expect("translate ok");
        assert_eq!(gpa, target_b);
        // Middle of region B.
        let gpa = dev
            .translate(0x10, iova_b + 0x2345, false)
            .expect("translate ok");
        assert_eq!(gpa, target_b + 0x2345);
    }

    /// A malformed mode-7 PTE with no zero bit in the address field
    /// (all-ones encoding) must produce a fault, not a panic or garbage GPA.
    #[test]
    fn test_translate_mode7_malformed_address_field_faults() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l1_gpa = 0x5_0000u64;
        // All-ones address field: ffz finds no zero bit ≤ 51 → fault.
        let leaf = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address((1u64 << 40) - 1) // all 40 bits set
            .with_ir(true)
            .with_iw(true);
        write_pte(&dev, l1_gpa, 0, &leaf);

        let dte = make_dte_with_translation(l1_gpa, 1, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let err = dev
            .translate(0x10, 0x0, false)
            .expect_err("malformed mode-7 PTE must fault");
        assert!(matches!(err, IommuFault::IoPageFault { .. }));
    }

    /// Mode-7 at L3: smallest non-default size is 2 GiB (count = 2). Verify
    /// the page walk handles huge mode-7 pages above the L2 default.
    #[test]
    fn test_translate_mode7_2g_at_l3() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let pgsize = 0x8000_0000u64; // 2 GiB
        let target_gpa = 0x4_0000_0000u64; // 16 GiB, 2 GiB-aligned
        let iova_base = 0x8_0000_0000u64; // 32 GiB, 2 GiB-aligned

        write_pte(
            &dev,
            l4_gpa,
            IommuPte::va_index(iova_base, 4),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let count = linux_pte_count(pgsize);
        assert_eq!(count, 2);
        let addr_field = linux_page_size_pte_addr(target_gpa, pgsize);
        let leaf = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .with_ir(true)
            .with_iw(true);
        let base_index = IommuPte::va_index(iova_base, 3);
        for i in 0..count {
            write_pte(&dev, l3_gpa, base_index + i, &leaf);
        }

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Test across the 2 GiB region, including past the 1 GiB L3-default
        // boundary so a mistaken use of L3's natural mask would alias.
        for off in [0u64, 0x3FFF_FFFF, 0x4000_0000, 0x7FFF_FFFF] {
            let iova = iova_base + off;
            let gpa = dev.translate(0x10, iova, false).expect("translate ok");
            assert_eq!(
                gpa,
                target_gpa + off,
                "iova {:#x} expected {:#x} got {:#x}",
                iova,
                target_gpa + off,
                gpa
            );
        }
    }

    /// Target GPA with non-trivial high bits in the same byte-range as the
    /// page-size encoding. Confirms the encode-decode round trip recovers the
    /// real base even when the address field has bits set above the size
    /// marker.
    #[test]
    fn test_translate_mode7_target_with_nontrivial_bits() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let pgsize = 0x4000u64; // 16 KiB
        // target_gpa has bit 14 set (above the size marker at bit 13). It
        // is still 16 KiB-aligned (0x10_4000 / 0x4000 = 0x41).
        let target_gpa = 0x10_4000u64;
        let iova_base = 0x40_0000u64;

        write_pte(
            &dev,
            l4_gpa,
            IommuPte::va_index(iova_base, 4),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            IommuPte::va_index(iova_base, 3),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l2_gpa,
            IommuPte::va_index(iova_base, 2),
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let count = linux_pte_count(pgsize);
        let addr_field = linux_page_size_pte_addr(target_gpa, pgsize);
        // The Linux encoding sets bit 12 (the size marker is bit 13, cleared).
        // Sanity-check the encoded field still recovers the target.
        assert_eq!(addr_field & !(pgsize - 1), target_gpa);
        let leaf = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .with_ir(true)
            .with_iw(true);
        let base_index = IommuPte::va_index(iova_base, 1);
        for i in 0..count {
            write_pte(&dev, l1_gpa, base_index + i, &leaf);
        }

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        for off in [0u64, 0xFFF, 0x1000, 0x2ABC, 0x3FFF] {
            let iova = iova_base + off;
            let gpa = dev.translate(0x10, iova, false).expect("translate ok");
            assert_eq!(gpa, target_gpa + off);
        }
    }

    #[test]
    fn test_translate_page_not_present() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // 1-level page table with no entries (all zeros = not present).
        let l1_gpa = 0x5_0000u64;
        let dte = make_dte_with_translation(l1_gpa, 1, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let result = dev.translate(0x10, 0x1000, false);
        assert!(result.is_err());
        match result.unwrap_err() {
            IommuFault::IoPageFault {
                device_id,
                domain_id,
                address,
                ..
            } => {
                assert_eq!(device_id, 0x10);
                assert_eq!(domain_id, 1);
                assert_eq!(address, 0x1000);
            }
            other => panic!("expected IoPageFault, got {:?}", other),
        }
    }

    #[test]
    fn test_translate_write_permission_denied() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // 1-level page table, read-only (IR=1, IW=0).
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xA_0000u64;

        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_gpa >> 12)
                .with_ir(true)
                .with_iw(false), // No write permission
        );

        // DTE allows IR and IW, but PTE only allows IR.
        let dte = make_dte_with_translation(l1_gpa, 1, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Read should succeed.
        let gpa = dev.translate(0x10, 0x0, false).unwrap();
        assert_eq!(gpa, target_gpa);

        // Write should fail.
        let result = dev.translate(0x10, 0x0, true);
        assert!(result.is_err());
        match result.unwrap_err() {
            IommuFault::IoPageFault { is_write, .. } => {
                assert!(is_write);
            }
            other => panic!("expected IoPageFault, got {:?}", other),
        }
    }

    #[test]
    fn test_translate_read_permission_denied() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // 1-level page table, write-only (IR=0, IW=1).
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xA_0000u64;

        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_gpa >> 12)
                .with_ir(false) // No read permission
                .with_iw(true),
        );

        let dte = make_dte_with_translation(l1_gpa, 1, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Read should fail.
        let result = dev.translate(0x10, 0x0, false);
        assert!(result.is_err());
        match result.unwrap_err() {
            IommuFault::IoPageFault { is_write, .. } => {
                assert!(!is_write);
            }
            other => panic!("expected IoPageFault, got {:?}", other),
        }
    }

    #[test]
    fn test_translate_dte_permission_restricts() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // PTE has full permissions, but DTE restricts write.
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xA_0000u64;

        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        // DTE: IR=1, IW=0 (read-only at DTE level).
        let dte = make_dte_with_translation(l1_gpa, 1, true, false, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Read should succeed.
        let gpa = dev.translate(0x10, 0x0, false).unwrap();
        assert_eq!(gpa, target_gpa);

        // Write should fail (DTE restricts IW).
        let result = dev.translate(0x10, 0x0, true);
        assert!(result.is_err());
    }

    #[test]
    fn test_translate_reserved_mode7() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // DTE with Mode=7 (reserved) should fault.
        let dte = make_dte_with_translation(0, 7, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let result = dev.translate(0x10, 0x0, false);
        assert!(result.is_err());
        match result.unwrap_err() {
            IommuFault::IllegalDevTableEntry { device_id, .. } => {
                assert_eq!(device_id, 0x10);
            }
            other => panic!("expected IllegalDevTableEntry, got {:?}", other),
        }
    }

    #[test]
    fn test_translate_iova_offset_preserved() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // 1-level page table mapping IOVA page 0 → GPA 0xB_0000.
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xB_0000u64;

        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let dte = make_dte_with_translation(l1_gpa, 1, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Various offsets within the 4KB page should be preserved.
        for offset in [0u64, 1, 0x100, 0x7FF, 0xFFF] {
            let gpa = dev.translate(0x10, offset, false).unwrap();
            assert_eq!(
                gpa,
                target_gpa | offset,
                "offset 0x{:x} not preserved",
                offset
            );
        }
    }

    #[test]
    fn test_translate_fault_to_event() {
        // Test that IommuFault::to_event_entry produces correct event entries.
        let fault = IommuFault::IoPageFault {
            device_id: 0x0100,
            domain_id: 0x0042,
            address: 0xDEAD_0000,
            is_write: true,
        };
        let event = fault.to_event_entry();
        assert_eq!(event.event_code(), EventCode::IO_PAGE_FAULT);
        assert_eq!(event.device_id(), 0x0100);

        let fault = IommuFault::IllegalDevTableEntry {
            device_id: 0x0200,
            address: 0x1000,
            is_interrupt: false,
            is_write: true,
        };
        let event = fault.to_event_entry();
        assert_eq!(event.event_code(), EventCode::ILLEGAL_DEV_TABLE_ENTRY);
        assert_eq!(event.device_id(), 0x0200);

        let fault = IommuFault::DevTabHardwareError {
            device_id: 0x0300,
            address: 0x2000,
        };
        let event = fault.to_event_entry();
        assert_eq!(event.event_code(), EventCode::DEV_TAB_HARDWARE_ERROR);
        assert_eq!(event.device_id(), 0x0300);

        let fault = IommuFault::PageTabHardwareError {
            device_id: 0x0400,
            address: 0x3000,
        };
        let event = fault.to_event_entry();
        assert_eq!(event.event_code(), EventCode::PAGE_TAB_HARDWARE_ERROR);
        assert_eq!(event.device_id(), 0x0400);
    }

    // =========================================================================
    // Interrupt Remapping Tests (1F)
    // =========================================================================

    /// Build a DTE with interrupt remapping configured.
    fn make_dte_with_interrupt_remap(irt_base_gpa: u64, int_tab_len: u8, int_ctl: u8) -> Dte {
        use spec::dte::*;
        Dte {
            dw0: DteDw0::new().with_v(true).with_tv(false),
            dw1: DteDw1::new().with_domain_id(1),
            dw2: DteDw2::new()
                .with_iv(true)
                .with_int_tab_len(int_tab_len)
                .with_int_tab_root_ptr(irt_base_gpa >> 6)
                .with_int_ctl(int_ctl),
            dw3: 0,
        }
    }

    /// Write an IRTE into guest memory.
    fn write_irte(dev: &AmdIommuDevice, irt_base_gpa: u64, index: u32, irte: &Irte) {
        let irte_gpa = irt_base_gpa + (index as u64) * (IRTE_SIZE as u64);
        dev.shared
            .guest_memory
            .write_plain(irte_gpa, irte)
            .expect("write IRTE");
    }

    #[test]
    fn test_irte_lookup_valid() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // IRT at 0xA_0000, 128-byte aligned (required by spec).
        let irt_gpa = 0xA_0000u64;
        let irte = Irte::new()
            .with_remap_en(true)
            .with_int_type(0) // Fixed
            .with_dm(false) // Physical destination
            .with_destination(3)
            .with_vector(0x40);
        write_irte(&dev, irt_gpa, 0, &irte);

        // DTE with IntCtl=Remap, IntTabLen=4 (16 entries), IV=1.
        let dte = make_dte_with_interrupt_remap(irt_gpa, 4, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // MSI data index = 0 → look up IRTE at index 0.
        let result = dev.lookup_irte(0x10, &dte, 0);
        let irte_out = result.unwrap();
        assert_eq!(irte_out.vector, 0x40);
        assert_eq!(irte_out.destination, 3);
        assert!(!irte_out.dm);
    }

    #[test]
    fn test_irte_lookup_invalid_remap_disabled() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let irt_gpa = 0xA_0000u64;
        // IRTE with RemapEn=0.
        let irte = Irte::new().with_remap_en(false).with_vector(0x40);
        write_irte(&dev, irt_gpa, 0, &irte);

        let dte = make_dte_with_interrupt_remap(irt_gpa, 4, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let result = dev.lookup_irte(0x10, &dte, 0);
        assert!(result.is_err());
        match result.unwrap_err() {
            IommuFault::IoPageFault { device_id, .. } => {
                assert_eq!(device_id, 0x10);
            }
            other => panic!("expected IoPageFault, got {:?}", other),
        }
    }

    #[test]
    fn test_irte_lookup_index_out_of_range() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let irt_gpa = 0xA_0000u64;
        // IntTabLen=2 → 4 entries (2^2). Index 5 is out of range.
        let dte = make_dte_with_interrupt_remap(irt_gpa, 2, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let result = dev.lookup_irte(0x10, &dte, 5);
        assert!(result.is_err());
    }

    #[test]
    fn test_irte_lookup_iv_not_set() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // DTE with IV=0 (interrupt map not valid).
        use spec::dte::*;
        let dte = Dte {
            dw0: DteDw0::new().with_v(true).with_tv(false),
            dw1: DteDw1::new(),
            dw2: DteDw2::new().with_iv(false).with_int_ctl(IntCtl::REMAP.0),
            dw3: 0,
        };
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let result = dev.lookup_irte(0x10, &dte, 0);
        assert!(result.is_err());
        match result.unwrap_err() {
            IommuFault::IllegalDevTableEntry {
                device_id,
                is_interrupt,
                ..
            } => {
                assert_eq!(device_id, 0x10);
                assert!(is_interrupt);
            }
            other => panic!("expected IllegalDevTableEntry, got {:?}", other),
        }
    }

    #[test]
    fn test_remap_msi_basic() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let irt_gpa = 0xA_0000u64;
        // Set up IRTE at index 0x30: remap to vector 0x40, destination 3, fixed.
        let irte = Irte::new()
            .with_remap_en(true)
            .with_int_type(0) // Fixed
            .with_dm(false) // Physical
            .with_destination(3)
            .with_vector(0x40);
        write_irte(&dev, irt_gpa, 0x30, &irte);

        let dte = make_dte_with_interrupt_remap(irt_gpa, 8, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Original MSI: vector 0x30, destination 0, fixed.
        let orig_addr = 0xFEE0_0000u64;
        let orig_data = 0x30u32; // data[10:0] = 0x30 = IRTE index

        let (new_addr, new_data) = dev.remap_msi(0x10, orig_addr, orig_data).unwrap();

        // Remapped: vector 0x40, destination 3, physical.
        assert_eq!(new_addr, 0xFEE0_0000 | (3u64 << 12)); // dest=3
        assert_eq!(new_data & 0xFF, 0x40); // vector
        assert_eq!((new_data >> 8) & 0x7, 0); // delivery mode = Fixed
    }

    #[test]
    fn test_remap_msi_logical_destination() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let irt_gpa = 0xA_0000u64;
        let irte = Irte::new()
            .with_remap_en(true)
            .with_int_type(0) // Fixed
            .with_dm(true) // Logical destination
            .with_destination(0xFF)
            .with_vector(0x50);
        write_irte(&dev, irt_gpa, 0, &irte);

        let dte = make_dte_with_interrupt_remap(irt_gpa, 4, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let (new_addr, new_data) = dev.remap_msi(0x10, 0xFEE0_0000, 0).unwrap();

        // Logical destination mode set in bit 2.
        assert_eq!(new_addr & (1 << 2), 1 << 2);
        // Destination 0xFF in bits [19:12].
        assert_eq!((new_addr >> 12) & 0xFF, 0xFF);
        assert_eq!(new_data & 0xFF, 0x50);
    }

    #[test]
    fn test_remap_msi_passthrough() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // IntCtl = PASS_THROUGH (01b) → return original address/data.
        let dte = make_dte_with_interrupt_remap(0, 0, IntCtl::PASS_THROUGH.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let orig_addr = 0xFEE0_5000u64;
        let orig_data = 0x30u32;

        let (new_addr, new_data) = dev.remap_msi(0x10, orig_addr, orig_data).unwrap();
        assert_eq!(new_addr, orig_addr);
        assert_eq!(new_data, orig_data);
    }

    #[test]
    fn test_remap_msi_abort() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // IntCtl = ABORT (00b) → fault.
        let dte = make_dte_with_interrupt_remap(0, 0, IntCtl::ABORT.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let result = dev.remap_msi(0x10, 0xFEE0_0000, 0x30);
        assert!(result.is_err());
        match result.unwrap_err() {
            IommuFault::IllegalDevTableEntry {
                device_id,
                is_interrupt,
                ..
            } => {
                assert_eq!(device_id, 0x10);
                assert!(is_interrupt);
            }
            other => panic!("expected IllegalDevTableEntry, got {:?}", other),
        }
    }

    #[test]
    fn test_remap_msi_reserved_int_ctl() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // IntCtl = 11b (reserved) → fault.
        let dte = make_dte_with_interrupt_remap(0, 0, 0b11);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let result = dev.remap_msi(0x10, 0xFEE0_0000, 0x30);
        assert!(result.is_err());
    }

    #[test]
    fn test_remap_msi_ga_mode_128bit_irte() {
        // This test exercises the GA (Guest APIC) IRTE path that Linux uses
        // when GASup=1 in the EFR. The driver sets GA_EN in the control
        // register and uses 128-bit GA-format IRTEs (irte_128_ops).
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;

        // Set up IOMMU with GA_EN=1.
        let size = (256u64 * DTE_SIZE as u64 / 4096).saturating_sub(1) as u16;
        let dtb = DevTabBase::new()
            .with_base_addr(devtab_gpa >> 12)
            .with_size(size);
        mmio_write64(
            &mut dev,
            MmioRegister::DEV_TAB_BASE.0 as u64,
            dtb.into_bits(),
        );

        let cmd_base = CmdBufBase::new()
            .with_base_addr(0x8_0000 >> 12)
            .with_length(8);
        mmio_write64(
            &mut dev,
            MmioRegister::CMD_BUF_BASE.0 as u64,
            cmd_base.into_bits(),
        );

        let evt_base = EvtLogBase::new()
            .with_base_addr(0x9_0000 >> 12)
            .with_length(8);
        mmio_write64(
            &mut dev,
            MmioRegister::EVT_LOG_BASE.0 as u64,
            evt_base.into_bits(),
        );

        let ctrl = IommuCtrl::new()
            .with_iommu_en(true)
            .with_cmd_buf_en(true)
            .with_evt_log_en(true)
            .with_ga_en(true); // Enable GA mode → 128-bit IRTEs
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_CTRL.0 as u64,
            ctrl.into_bits(),
        );

        // Verify GA_EN is set.
        let readback =
            IommuCtrl::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_CTRL.0 as u64));
        assert!(readback.ga_en(), "GA_EN should be set");

        // IRT with 128-bit GA entries at 0xA_0000 (128-byte aligned).
        let irt_gpa = 0xA_0000u64;
        let irte_index = 5u32;

        // Build a 128-bit GA IRTE: vector=0x80, destination=7, physical mode.
        use spec::irte::{IRTE_GA_SIZE, IrteGa, IrteGaHi, IrteGaLo};
        let ga_irte = IrteGa {
            lo: IrteGaLo::new()
                .with_remap_en(true)
                .with_int_type(0) // Fixed
                .with_dm(false) // Physical
                .with_destination(7), // Low 24 bits of destination
            hi: IrteGaHi::new().with_vector(0x80).with_destination_hi(0), // High 8 bits of destination
        };

        // Write the 128-bit IRTE at the correct offset.
        let irte_gpa = irt_gpa + (irte_index as u64) * (IRTE_GA_SIZE as u64);
        dev.shared
            .guest_memory
            .write_plain(irte_gpa, &ga_irte)
            .unwrap();

        // DTE with interrupt remapping, pointing to our IRT.
        let dte = make_dte_with_interrupt_remap(irt_gpa, 8, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Remap MSI with data=irte_index → should read 128-bit GA IRTE.
        let (new_addr, new_data) = dev.remap_msi(0x10, 0xFEE0_0000, irte_index).unwrap();

        // Verify remapped values.
        assert_eq!(new_data & 0xFF, 0x80, "vector should be 0x80");
        assert_eq!((new_data >> 8) & 0x7, 0, "delivery mode should be Fixed");
        assert_eq!((new_addr >> 12) & 0xFF, 7, "destination should be 7");
        assert_eq!(new_addr & (1 << 2), 0, "DM should be physical (0)");
    }

    /// Verify that the 128-bit GA-format IRTE walker correctly assembles a
    /// full 32-bit x2APIC destination from `destination_lo` (24 bits in the
    /// low qword) and `destination_hi` (8 bits in the high qword), per spec
    /// §2.2.5.2.
    #[test]
    fn test_remap_msi_ga_mode_x2apic_destination() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // Enable GA mode so the walker uses the 128-bit IRTE format.
        let ctrl = IommuCtrl::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_CTRL.0 as u64))
            .with_ga_en(true);
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_CTRL.0 as u64,
            ctrl.into_bits(),
        );

        // Build a GA IRTE with a 32-bit X2APIC destination 0x0A12_3456:
        // destination_lo = 0x12_3456 (24 bits), destination_hi = 0x0A (8 bits).
        let irt_gpa = 0xA_0000u64;
        let irte_index = 3u32;
        use spec::irte::{IRTE_GA_SIZE, IrteGa, IrteGaHi, IrteGaLo};
        let ga_irte = IrteGa {
            lo: IrteGaLo::new()
                .with_remap_en(true)
                .with_int_type(0)
                .with_dm(false)
                .with_destination(0x12_3456),
            hi: IrteGaHi::new().with_vector(0x42).with_destination_hi(0x0A),
        };
        let irte_gpa = irt_gpa + (irte_index as u64) * (IRTE_GA_SIZE as u64);
        dev.shared
            .guest_memory
            .write_plain(irte_gpa, &ga_irte)
            .unwrap();

        let dte = make_dte_with_interrupt_remap(irt_gpa, 4, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Use the IRTE walker directly so we observe the full 32-bit
        // destination, independent of how the legacy MSI-address encoder
        // packs the result.
        let ri = dev.lookup_irte(0x10, &dte, irte_index).unwrap();
        assert_eq!(
            ri.destination, 0x0A12_3456,
            "32-bit X2APIC destination should combine destination_lo and destination_hi"
        );
        assert_eq!(ri.vector, 0x42);
        assert!(!ri.dm, "physical destination mode");
    }

    /// Verify that `ADVERTISED_EXT_FEAT` advertises `XTSup` (bit 2), `IASup`
    /// (bit 6), and `GASup` (bit 7) when read via MMIO.
    #[test]
    fn test_ext_feat_advertises_xtsup_iasup_gasup() {
        let mut dev = create_test_device();
        let efr = mmio_read64(&mut dev, MmioRegister::EXT_FEAT.0 as u64);
        let ef = ExtFeat::from_bits(efr);
        assert!(ef.xt_sup(), "XTSup (bit 2) must be advertised");
        assert!(ef.ia_sup(), "IASup (bit 6) must be advertised");
        assert!(ef.ga_sup(), "GASup (bit 7) must be advertised");
    }

    /// Verify that the `XTEn` and `IntCapXTEn` bits round-trip through the
    /// IOMMU control register. When `EFR[XTSup]=1`, both bits are valid
    /// software-settable bits in `IOMMU_CTRL` (§3.4.8).
    #[test]
    fn test_iommu_ctrl_xten_intcapxten_roundtrip() {
        let mut dev = create_test_device();
        let ctrl = IommuCtrl::new()
            .with_iommu_en(true)
            .with_xt_en(true)
            .with_int_cap_xt_en(true);
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_CTRL.0 as u64,
            ctrl.into_bits(),
        );

        let readback =
            IommuCtrl::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_CTRL.0 as u64));
        assert!(readback.iommu_en());
        assert!(readback.xt_en(), "XTEn must round-trip");
        assert!(readback.int_cap_xt_en(), "IntCapXTEn must round-trip");
    }

    /// Verify that the General and PPR XT Interrupt Control registers
    /// round-trip arbitrary 64-bit values via MMIO. When `EFR[XTSup]=1`,
    /// these registers (§3.4.13) hold the IOMMU's own MSI destination in
    /// x2APIC format and must not fall through to the "unknown register"
    /// warn-and-discard path.
    #[test]
    fn test_xt_interrupt_control_registers_roundtrip() {
        use spec::registers::XtIntCtrl;

        let mut dev = create_test_device();

        // Compose an XT control value with non-zero high destination bits so
        // both `xt_int_dest_low` (24 bits) and `xt_int_dest_high` (8 bits) are
        // exercised through the round-trip.
        let value = XtIntCtrl::new()
            .with_xt_int_dest_mode(false)
            .with_xt_int_dest_low(0x12_3456)
            .with_xt_int_vector(0x80)
            .with_xt_int_dm(false)
            .with_xt_int_dest_high(0x0A)
            .into_bits();

        mmio_write64(&mut dev, MmioRegister::GEN_XT_INT_CTRL.0 as u64, value);
        mmio_write64(
            &mut dev,
            MmioRegister::PPR_XT_INT_CTRL.0 as u64,
            value.wrapping_add(1),
        );

        let gen_readback = mmio_read64(&mut dev, MmioRegister::GEN_XT_INT_CTRL.0 as u64);
        let ppr_readback = mmio_read64(&mut dev, MmioRegister::PPR_XT_INT_CTRL.0 as u64);
        assert_eq!(gen_readback, value);
        assert_eq!(ppr_readback, value.wrapping_add(1));

        // Sanity-check that the bitfield decodes the round-tripped value.
        let decoded = XtIntCtrl::from_bits(gen_readback);
        assert_eq!(decoded.xt_int_dest_low(), 0x12_3456);
        assert_eq!(decoded.xt_int_dest_high(), 0x0A);
        assert_eq!(decoded.xt_int_vector(), 0x80);
    }

    #[test]
    fn test_remap_msi_iommu_disabled() {
        let guest_memory = GuestMemory::allocate(0x10_0000);
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        // IOMMU not enabled — MSI should pass through unchanged.
        let (new_addr, new_data) = dev.remap_msi(0x10, 0xFEE0_0000, 0x30).unwrap();
        assert_eq!(new_addr, 0xFEE0_0000);
        assert_eq!(new_data, 0x30);
    }

    #[test]
    fn test_remap_msi_index_out_of_range() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let irt_gpa = 0xA_0000u64;
        // IntTabLen=1 → 2 entries (2^1). MSI data index = 5 is out of range.
        let dte = make_dte_with_interrupt_remap(irt_gpa, 1, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let result = dev.remap_msi(0x10, 0xFEE0_0000, 5);
        assert!(result.is_err());
    }

    #[test]
    fn test_remap_msi_arbitrated_delivery() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let irt_gpa = 0xA_0000u64;
        let irte = Irte::new()
            .with_remap_en(true)
            .with_int_type(1) // Arbitrated (lowest priority)
            .with_dm(false)
            .with_destination(7)
            .with_vector(0x80);
        write_irte(&dev, irt_gpa, 0, &irte);

        let dte = make_dte_with_interrupt_remap(irt_gpa, 4, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        let (new_addr, new_data) = dev.remap_msi(0x10, 0xFEE0_0000, 0).unwrap();
        assert_eq!((new_addr >> 12) & 0xFF, 7); // destination
        assert_eq!(new_data & 0xFF, 0x80); // vector
        assert_eq!((new_data >> 8) & 0x7, 1); // delivery mode = Arbitrated
    }

    #[test]
    fn test_irte_lookup_multiple_indices() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        let irt_gpa = 0xA_0000u64;
        // Set up multiple IRTEs.
        for i in 0..4u32 {
            let irte = Irte::new()
                .with_remap_en(true)
                .with_vector((0x40 + i) as u8)
                .with_destination(i as u8);
            write_irte(&dev, irt_gpa, i, &irte);
        }

        let dte = make_dte_with_interrupt_remap(irt_gpa, 4, IntCtl::REMAP.0);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Verify each index returns the correct IRTE.
        for i in 0..4u32 {
            let irte = dev.lookup_irte(0x10, &dte, i).unwrap();
            assert_eq!(irte.vector, (0x40 + i) as u8);
            assert_eq!(irte.destination, i);
        }
    }

    // =========================================================================
    // Shared State and Per-Device Wrapper Tests (1G)
    // =========================================================================

    /// A mock `SignalMsi` implementation that records the last MSI signaled.
    struct MockSignalMsi {
        last_msi: parking_lot::Mutex<Option<(Option<u32>, u64, u32)>>,
    }

    impl MockSignalMsi {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                last_msi: parking_lot::Mutex::new(None),
            })
        }

        fn last(&self) -> Option<(Option<u32>, u64, u32)> {
            *self.last_msi.lock()
        }
    }

    impl SignalMsi for MockSignalMsi {
        fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
            *self.last_msi.lock() = Some((devid, address, data));
        }
    }

    const WRAPPER_BUS: u8 = 1;
    const WRAPPER_DEVICE_ID: u16 = (WRAPPER_BUS as u16) << 8;

    fn bus_range_for_device_id(device_id: u16) -> pci_core::bus_range::AssignedBusRange {
        assert_eq!(device_id & 0xFF, 0, "test DeviceID must be dev 0 fn 0");
        let bus = (device_id >> 8) as u8;
        let bus_range = pci_core::bus_range::AssignedBusRange::new();
        bus_range.set_bus_range(bus, bus);
        bus_range
    }

    fn wrapper_bus_range() -> pci_core::bus_range::AssignedBusRange {
        bus_range_for_device_id(WRAPPER_DEVICE_ID)
    }

    /// Set up a device with IOMMU enabled, device table, page tables, and IRT
    /// for testing per-device wrappers. Returns the device and shared state.
    fn setup_iommu_for_wrappers() -> AmdIommuDevice {
        let guest_memory = GuestMemory::allocate(0x10_0000); // 1MB
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 512);

        // Set up a 1-level page table mapping IOVA 0 → GPA 0xA_0000.
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xA_0000u64;

        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        // DTE for the default BDF on the assigned wrapper test bus:
        // 1-level page table, IRT with remapping.
        let irt_gpa = 0xC_0000u64;
        let dte = {
            use spec::dte::*;
            Dte {
                dw0: DteDw0::new()
                    .with_v(true)
                    .with_tv(true)
                    .with_mode(1)
                    .with_host_pt_root_ptr(l1_gpa >> 12)
                    .with_ir(true)
                    .with_iw(true),
                dw1: DteDw1::new().with_domain_id(1),
                dw2: DteDw2::new()
                    .with_iv(true)
                    .with_int_tab_len(4) // 16 entries
                    .with_int_tab_root_ptr(irt_gpa >> 6)
                    .with_int_ctl(IntCtl::REMAP.0),
                dw3: 0,
            }
        };
        write_dte(&dev, devtab_gpa, WRAPPER_DEVICE_ID, &dte);

        // Set up IRTE at index 0: remap to vector 0x40, destination 3.
        let irte = Irte::new()
            .with_remap_en(true)
            .with_int_type(0)
            .with_dm(false)
            .with_destination(3)
            .with_vector(0x40);
        write_irte(&dev, irt_gpa, 0, &irte);

        dev
    }

    /// Test helper: create per-device wrappers for DMA translation and MSI
    /// remapping.
    fn device_context(
        shared: &Arc<IommuSharedState>,
        bus_range: pci_core::bus_range::AssignedBusRange,
        inner_gm: &GuestMemory,
        inner_msi: Arc<dyn SignalMsi>,
    ) -> (GuestMemory, Arc<IommuSignalMsi>) {
        let translator = shared.translator();
        let gm = iommu_common::TranslatingMemory::new_guest_memory(
            "amd-iommu-translating",
            translator,
            bus_range,
            inner_gm.clone(),
        );
        let msi = shared.wrap_signal_msi(inner_msi);
        (gm, msi)
    }

    #[test]
    fn test_translating_memory_basic_read() {
        let dev = setup_iommu_for_wrappers();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();

        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Write data at GPA 0xA_0000 (the target page).
        dev.shared
            .guest_memory
            .write_at(0xA_0000, &[0xDE, 0xAD, 0xBE, 0xEF])
            .unwrap();

        // Read via IOVA 0x0 through the translating memory.
        let mut buf = [0u8; 4];
        iommu_gm.read_at(0x0, &mut buf).unwrap();
        assert_eq!(buf, [0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_translating_memory_basic_write() {
        let dev = setup_iommu_for_wrappers();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();

        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Write via IOVA 0x100 through the translating memory.
        iommu_gm.write_at(0x100, &[0xCA, 0xFE]).unwrap();

        // Read raw from GPA 0xA_0100.
        let mut buf = [0u8; 2];
        dev.shared.guest_memory.read_at(0xA_0100, &mut buf).unwrap();
        assert_eq!(buf, [0xCA, 0xFE]);
    }

    #[test]
    fn test_translating_memory_passthrough() {
        let guest_memory = GuestMemory::allocate(0x10_0000);
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 512);

        // DTE with V=1, TV=1, Mode=0 (pass-through).
        let dte = make_dte_with_translation(0, 0, true, true, 1);
        write_dte(&dev, devtab_gpa, WRAPPER_DEVICE_ID, &dte);

        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Write data at GPA 0x2000.
        dev.shared.guest_memory.write_at(0x2000, &[0x42]).unwrap();

        // Read via IOVA 0x2000 — should pass through.
        let mut buf = [0u8; 1];
        iommu_gm.read_at(0x2000, &mut buf).unwrap();
        assert_eq!(buf, [0x42]);
    }

    #[test]
    fn test_translating_memory_fault() {
        let dev = setup_iommu_for_wrappers();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();

        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Read from unmapped IOVA (page index 1 is not mapped in the L1 table).
        let mut buf = [0u8; 4];
        let result = iommu_gm.read_at(0x1000, &mut buf);
        assert!(result.is_err(), "unmapped IOVA should fault");
    }

    #[test]
    fn test_translating_memory_disabled_bypass() {
        let guest_memory = GuestMemory::allocate(0x10_0000);
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());
        // IOMMU not enabled — all accesses should pass through.

        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        dev.shared.guest_memory.write_at(0x5000, &[0xAA]).unwrap();

        let mut buf = [0u8; 1];
        iommu_gm.read_at(0x5000, &mut buf).unwrap();
        assert_eq!(buf, [0xAA]);
    }

    #[test]
    fn test_signal_msi_remapped() {
        let dev = setup_iommu_for_wrappers();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();

        let (_gm, iommu_msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi.clone(),
        );

        // Signal MSI with data=0 (IRTE index 0), requester ID = wrapper device.
        // Should remap to vector 0x40, dest 3.
        iommu_msi.signal_msi(Some(WRAPPER_DEVICE_ID as u32), 0xFEE0_0000, 0);

        let (devid, addr, data) = mock_msi.last().expect("MSI should have been delivered");
        assert_eq!(devid, Some(WRAPPER_DEVICE_ID as u32));
        assert_eq!(addr, 0xFEE0_0000 | (3u64 << 12)); // destination 3
        assert_eq!(data & 0xFF, 0x40); // vector 0x40
    }

    #[test]
    fn test_signal_msi_iommu_disabled() {
        let guest_memory = GuestMemory::allocate(0x10_0000);
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (_gm, iommu_msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi.clone(),
        );

        // IOMMU disabled — MSI should pass through unchanged.
        iommu_msi.signal_msi(Some(5), 0xFEE0_5000, 0x30);

        let (devid, addr, data) = mock_msi.last().expect("MSI should pass through");
        assert_eq!(devid, Some(5));
        assert_eq!(addr, 0xFEE0_5000);
        assert_eq!(data, 0x30);
    }

    #[test]
    fn test_signal_msi_fault_drops_interrupt() {
        let dev = setup_iommu_for_wrappers();
        let devtab_gpa = 0x1_0000;

        // Set up a DTE with IntCtl=ABORT for device 0x20.
        let dte = make_dte_with_interrupt_remap(0, 0, IntCtl::ABORT.0);
        write_dte(&dev, devtab_gpa, 0x20, &dte);

        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (_gm, iommu_msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi.clone(),
        );

        // Signal MSI with requester ID — should be dropped (IntCtl=ABORT).
        iommu_msi.signal_msi(Some(0x20), 0xFEE0_0000, 0x30);

        assert!(
            mock_msi.last().is_none(),
            "MSI should be dropped on abort fault"
        );
    }

    #[test]
    fn test_device_context() {
        let dev = setup_iommu_for_wrappers();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();

        let (gm, msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Verify the returned GuestMemory works.
        dev.shared.guest_memory.write_at(0xA_0000, &[0x55]).unwrap();
        let mut buf = [0u8; 1];
        gm.read_at(0x0, &mut buf).unwrap();
        assert_eq!(buf, [0x55]);

        // Verify the MSI wrapper works.
        msi.signal_msi(None, 0xFEE0_0000, 0);
        // (IRTE 0 remaps to vector 0x40, dest 3 — verified in other tests)
    }

    // =========================================================================
    // End-to-End Test (1J.1)
    // =========================================================================

    /// End-to-end test exercising the full IOMMU stack: device discovery via
    /// PCI capability, MMIO register programming (mimicking a guest IOMMU
    /// driver init sequence), device table + page table + IRT setup in guest
    /// memory, command buffer processing, DMA translation through
    /// `IommuTranslatingMemory`, and MSI remapping through `IommuSignalMsi`.
    ///
    /// This test follows the exact sequence a guest IOMMU driver would use:
    ///
    /// 1. Read PCI capability, find MMIO base.
    /// 2. Read ExtFeat register, verify features.
    /// 3. Write DevTabBase (point to device table in guest memory).
    /// 4. Write CmdBufBase (point to command buffer).
    /// 5. Write EvtLogBase (point to event log).
    /// 6. Write IommuCtrl: enable CmdBufEn, EvtLogEn, IommuEn.
    /// 7. Configure DTE for a device with page table and IRT.
    /// 8. Build a 4-level page table mapping IOVA 0x0 → GPA 0x10_0000.
    /// 9. Set up IRTE: RemapEn=1, Vector=0x40, Destination=0, IntType=FIXED.
    /// 10. Issue INVALIDATE_DEVTAB_ENTRY + COMPLETION_WAIT via command buffer.
    /// 11. Write data to GPA via raw guest memory.
    /// 12. Read via IommuTranslatingMemory at IOVA, verify data.
    /// 13. Write via IommuTranslatingMemory at IOVA, read from GPA, verify.
    /// 14. Read from unmapped IOVA, verify error + event log entry.
    /// 15. Fire MSI via IommuSignalMsi, verify remapped vector/destination.
    #[test]
    fn test_end_to_end_full_stack() {
        // =====================================================================
        // Memory layout (2MB guest memory):
        //   0x00_0000 – 0x00_7FFF  Device Table (1024 entries × 32 bytes = 32KB)
        //   0x00_8000 – 0x00_8FFF  Command Buffer (256 entries × 16 = 4KB)
        //   0x00_9000 – 0x00_9FFF  Event Log (256 entries × 16 = 4KB)
        //   0x00_A000 – 0x00_AFFF  L4 Page Table
        //   0x00_B000 – 0x00_BFFF  L3 Page Table
        //   0x00_C000 – 0x00_CFFF  L2 Page Table
        //   0x00_D000 – 0x00_DFFF  L1 Page Table
        //   0x00_E000 – 0x00_E3FF  Interrupt Remapping Table (256 IRTEs × 4)
        //   0x00_F000 – 0x00_F007  COMPLETION_WAIT store target
        //   0x10_0000 – 0x10_0FFF  DMA target page (GPA for IOVA 0x0)
        // =====================================================================

        let guest_memory = GuestMemory::allocate(0x20_0000); // 2MB
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        let devtab_gpa: u64 = 0x00_0000;
        let cmdbuf_gpa: u64 = 0x00_8000;
        let evtlog_gpa: u64 = 0x00_9000;
        let l4_gpa: u64 = 0x00_A000;
        let l3_gpa: u64 = 0x00_B000;
        let l2_gpa: u64 = 0x00_C000;
        let l1_gpa: u64 = 0x00_D000;
        let irt_gpa: u64 = 0x00_E000;
        let store_gpa: u64 = 0x00_F000;
        let dma_target_gpa: u64 = 0x10_0000;
        let device_id: u16 = WRAPPER_DEVICE_ID;

        // -----------------------------------------------------------------
        // Step 1: Read PCI capability, find MMIO base.
        // -----------------------------------------------------------------
        let cap_header_raw = pci_read(&mut dev, PCI_CAP_OFFSET);
        let cap_id = cap_header_raw & 0xFF;
        assert_eq!(cap_id, 0x0F, "CapID should be AMD IOMMU");

        let base_low_raw = pci_read(&mut dev, PCI_CAP_OFFSET + 4);
        let base_high_raw = pci_read(&mut dev, PCI_CAP_OFFSET + 8);
        let base_low = BaseAddrLow::from_bits(base_low_raw);
        let base_high = BaseAddrHigh::from_bits(base_high_raw);
        assert!(base_low.enable(), "IOMMU MMIO should be enabled");
        let mmio_base_from_cap =
            ((base_low.base_addr() as u64) << 14) | ((base_high.base_addr() as u64) << 32);
        assert_eq!(mmio_base_from_cap, TEST_MMIO_BASE);

        // Read device range from capability.
        let range_raw = pci_read(&mut dev, PCI_CAP_OFFSET + 12);
        let range = Range::from_bits(range_raw);
        assert!(range.rng_valid(), "range should be valid");
        assert_eq!(range.first_device(), 0x00);
        assert_eq!(range.last_device(), 0xFF);

        // -----------------------------------------------------------------
        // Step 2: Read ExtFeat register, verify features.
        // -----------------------------------------------------------------
        let ext_feat = mmio_read64(&mut dev, MmioRegister::EXT_FEAT.0 as u64);
        let ef = ExtFeat::from_bits(ext_feat);
        assert!(ef.ia_sup(), "INVALIDATE_IOMMU_ALL should be supported");
        assert_eq!(ef.hats(), 0b00, "HATS should be 4-level");

        // -----------------------------------------------------------------
        // Step 3: Write DevTabBase.
        // -----------------------------------------------------------------
        // 1024 entries: size = (1024 * 32 / 4096) - 1 = 7
        let dtb = DevTabBase::new()
            .with_base_addr(devtab_gpa >> 12)
            .with_size(7);
        mmio_write64(
            &mut dev,
            MmioRegister::DEV_TAB_BASE.0 as u64,
            dtb.into_bits(),
        );
        assert_eq!(
            mmio_read64(&mut dev, MmioRegister::DEV_TAB_BASE.0 as u64),
            dtb.into_bits()
        );

        // -----------------------------------------------------------------
        // Step 4: Write CmdBufBase.
        // -----------------------------------------------------------------
        let cmd_base = CmdBufBase::new()
            .with_base_addr(cmdbuf_gpa >> 12)
            .with_length(8); // 256 entries
        mmio_write64(
            &mut dev,
            MmioRegister::CMD_BUF_BASE.0 as u64,
            cmd_base.into_bits(),
        );

        // -----------------------------------------------------------------
        // Step 5: Write EvtLogBase.
        // -----------------------------------------------------------------
        let evt_base = EvtLogBase::new()
            .with_base_addr(evtlog_gpa >> 12)
            .with_length(8); // 256 entries
        mmio_write64(
            &mut dev,
            MmioRegister::EVT_LOG_BASE.0 as u64,
            evt_base.into_bits(),
        );

        // -----------------------------------------------------------------
        // Step 6: Enable IOMMU.
        // -----------------------------------------------------------------
        let ctrl = IommuCtrl::new()
            .with_iommu_en(true)
            .with_cmd_buf_en(true)
            .with_evt_log_en(true)
            .with_evt_int_en(true)
            .with_com_wait_int_en(true);
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_CTRL.0 as u64,
            ctrl.into_bits(),
        );

        // Verify status reflects running state.
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(status.cmd_buf_run(), "CmdBufRun should be set");
        assert!(status.evt_log_run(), "EvtLogRun should be set");

        // -----------------------------------------------------------------
        // Step 7: Configure DTE for the test device.
        // -----------------------------------------------------------------
        // 4-level page table, interrupt remapping enabled.
        use spec::dte::*;
        let dte = Dte {
            dw0: DteDw0::new()
                .with_v(true)
                .with_tv(true)
                .with_mode(4) // 4-level page table
                .with_host_pt_root_ptr(l4_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
            dw1: DteDw1::new().with_domain_id(1),
            dw2: DteDw2::new()
                .with_iv(true)
                .with_int_tab_len(8) // 2^8 = 256 IRT entries
                .with_int_tab_root_ptr(irt_gpa >> 6)
                .with_int_ctl(IntCtl::REMAP.0),
            dw3: 0,
        };
        write_dte(&dev, devtab_gpa, device_id, &dte);

        // -----------------------------------------------------------------
        // Step 8: Build 4-level page table: IOVA 0x0 → GPA 0x10_0000.
        // -----------------------------------------------------------------
        // L4[0] → L3
        write_pte(
            &dev,
            l4_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        // L3[0] → L2
        write_pte(
            &dev,
            l3_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        // L2[0] → L1
        write_pte(
            &dev,
            l2_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        // L1[0] → target page at GPA 0x10_0000
        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0) // leaf
                .with_address(dma_target_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        // -----------------------------------------------------------------
        // Step 9: Set up IRTE at index 0x30.
        // -----------------------------------------------------------------
        let irte = Irte::new()
            .with_remap_en(true)
            .with_int_type(0) // Fixed
            .with_dm(false) // Physical destination
            .with_destination(0) // CPU 0
            .with_vector(0x40); // Remapped vector
        write_irte(&dev, irt_gpa, 0x30, &irte);

        // -----------------------------------------------------------------
        // Step 10: Issue INVALIDATE_DEVTAB_ENTRY + COMPLETION_WAIT.
        // -----------------------------------------------------------------
        // Write commands to the command buffer.
        let cmd0 = invalidate_devtab_entry(device_id);
        // Build COMPLETION_WAIT with both S (store) and I (interrupt) flags.
        let cw_store_data: u64 = 0xDEAD_BEEF_CAFE_1234;
        let cmd1 = {
            let dw0 = 0x03u32 // s=1 (bit 0), i=1 (bit 1)
                | ((store_gpa >> 3) as u32 & 0x1FFF_FFFF) << 3;
            let dw1 = (CommandOpcode::COMPLETION_WAIT.0 as u32) << 28
                | ((store_gpa >> 32) as u32 & 0x000F_FFFF);
            CommandEntry {
                dw0,
                dw1,
                dw2: cw_store_data as u32,
                dw3: (cw_store_data >> 32) as u32,
            }
        };
        dev.shared
            .guest_memory
            .write_plain(cmdbuf_gpa, &cmd0)
            .unwrap();
        dev.shared
            .guest_memory
            .write_plain(cmdbuf_gpa + 16, &cmd1)
            .unwrap();

        // Poke CmdBufTail to process both commands.
        let tail = CmdBufTail::new().with_tail_ptr(2);
        mmio_write64(
            &mut dev,
            MmioRegister::CMD_BUF_TAIL.0 as u64,
            tail.into_bits(),
        );

        // Verify CmdBufHead advanced to 2.
        let head =
            CmdBufHead::from_bits(mmio_read64(&mut dev, MmioRegister::CMD_BUF_HEAD.0 as u64));
        assert_eq!(head.head_ptr(), 2, "CmdBufHead should advance to 2");

        // Verify COMPLETION_WAIT store data written.
        let store_readback: u64 = dev
            .shared
            .guest_memory
            .read_plain(store_gpa)
            .expect("read store data");
        assert_eq!(store_readback, cw_store_data);

        // Verify ComWaitInt is set.
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(status.com_wait_int(), "ComWaitInt should be set");

        // Clear ComWaitInt (RW1C).
        let clear_status = IommuStatus::new().with_com_wait_int(true);
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_STATUS.0 as u64,
            clear_status.into_bits(),
        );
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(
            !status.com_wait_int(),
            "ComWaitInt should be cleared after RW1C"
        );

        // -----------------------------------------------------------------
        // Create per-device wrappers.
        // -----------------------------------------------------------------
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, iommu_msi) = device_context(
            &shared,
            bus_range_for_device_id(device_id),
            &dev.shared.guest_memory,
            mock_msi.clone(),
        );

        // -----------------------------------------------------------------
        // Step 11: Write test data to GPA via raw guest memory.
        // -----------------------------------------------------------------
        let test_data = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
        dev.shared
            .guest_memory
            .write_at(dma_target_gpa, &test_data)
            .unwrap();

        // -----------------------------------------------------------------
        // Step 12: Read via IommuTranslatingMemory at IOVA 0x0.
        // -----------------------------------------------------------------
        let mut buf = [0u8; 8];
        iommu_gm
            .read_at(0x0, &mut buf)
            .expect("DMA read should succeed through IOMMU translation");
        assert_eq!(buf, test_data, "DMA read data should match");

        // -----------------------------------------------------------------
        // Step 13: Write via IommuTranslatingMemory, read from GPA.
        // -----------------------------------------------------------------
        let write_data = [0x01, 0x02, 0x03, 0x04];
        iommu_gm
            .write_at(0x100, &write_data)
            .expect("DMA write should succeed");

        let mut verify_buf = [0u8; 4];
        dev.shared
            .guest_memory
            .read_at(dma_target_gpa + 0x100, &mut verify_buf)
            .unwrap();
        assert_eq!(
            verify_buf, write_data,
            "DMA write data should appear at GPA"
        );

        // -----------------------------------------------------------------
        // Step 14: Read from unmapped IOVA, verify error + event log entry.
        // -----------------------------------------------------------------
        // IOVA 0x1000 is page index 1, which has no L1 PTE (PR=0).
        let mut fail_buf = [0u8; 4];
        let result = iommu_gm.read_at(0x1000, &mut fail_buf);
        assert!(
            result.is_err(),
            "read from unmapped IOVA should return error"
        );

        // Verify event log has an IO_PAGE_FAULT entry.
        let evt_tail =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));
        assert!(
            evt_tail.tail_ptr() > 0,
            "event log should have at least one entry"
        );
        let event: EventEntry = dev
            .shared
            .guest_memory
            .read_plain(evtlog_gpa)
            .expect("read event log entry");
        assert_eq!(
            event.event_code(),
            EventCode::IO_PAGE_FAULT,
            "event should be IO_PAGE_FAULT"
        );
        assert_eq!(event.device_id(), device_id, "fault device_id should match");

        // -----------------------------------------------------------------
        // Step 15: Fire MSI via IommuSignalMsi, verify remapped output.
        // -----------------------------------------------------------------
        // Original MSI data = 0x30 → IRTE index 0x30, which we set up above
        // to remap to vector 0x40, destination 0, physical.
        iommu_msi.signal_msi(Some(device_id as u32), 0xFEE0_0000, 0x30);

        let (devid, new_addr, new_data) =
            mock_msi.last().expect("remapped MSI should be delivered");
        assert_eq!(devid, Some(device_id as u32));
        // Destination 0 → address = 0xFEE0_0000 | (0 << 12) = 0xFEE0_0000
        assert_eq!(
            new_addr, 0xFEE0_0000,
            "remapped MSI address should have destination 0"
        );
        // Vector 0x40, delivery mode Fixed (0)
        assert_eq!(new_data & 0xFF, 0x40, "remapped vector should be 0x40");
        assert_eq!(
            (new_data >> 8) & 0x7,
            0,
            "delivery mode should be Fixed (0)"
        );
    }

    // =========================================================================
    // Multi-page and cross-page DMA translation tests
    // =========================================================================

    /// Set up an IOMMU with two consecutive IOVA pages mapped to
    /// non-contiguous GPAs, for testing cross-page and large DMA accesses.
    ///
    /// Memory layout (2MB guest memory):
    ///   0x01_0000  Device table (256 entries)
    ///   0x05_0000  L1 page table (512 entries, only [0] and [1] populated)
    ///   0x08_0000  Command buffer
    ///   0x09_0000  Event log
    ///   0x0A_0000  Target page 0: IOVA 0x0000–0x0FFF → GPA 0x0A_0000
    ///   0x0C_0000  Target page 1: IOVA 0x1000–0x1FFF → GPA 0x0C_0000
    ///              (deliberately non-contiguous GPAs)
    fn setup_iommu_two_pages() -> AmdIommuDevice {
        let guest_memory = GuestMemory::allocate(0x20_0000); // 2MB
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 512);

        let l1_gpa = 0x5_0000u64;
        let target_page0 = 0xA_0000u64;
        let target_page1 = 0xC_0000u64;

        // L1[0] → GPA 0xA_0000
        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_page0 >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        // L1[1] → GPA 0xC_0000
        write_pte(
            &dev,
            l1_gpa,
            1,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_page1 >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let dte = make_dte_with_translation(l1_gpa, 1, true, true, 1);
        write_dte(&dev, devtab_gpa, WRAPPER_DEVICE_ID, &dte);

        dev
    }

    fn setup_iommu_multi_region_target() -> AmdIommuDevice {
        let guest_memory = GuestMemory::new_multi_region(
            "iommu-multi-region-test",
            0x10_0000,
            vec![
                Some(guestmem::AlignedHeapMemory::new(0x20_000)),
                Some(guestmem::AlignedHeapMemory::new(0x20_000)),
            ],
        )
        .unwrap();
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        let devtab_gpa = 0x1000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 512);

        let l1_gpa = 0x5000u64;
        let target_page0 = 0x10_0000u64;
        let target_page1 = 0x10_1000u64;

        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_page0 >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l1_gpa,
            1,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_page1 >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let dte = make_dte_with_translation(l1_gpa, 1, true, true, 1);
        write_dte(&dev, devtab_gpa, WRAPPER_DEVICE_ID, &dte);

        dev
    }

    #[test]
    fn test_translating_memory_write_plain_u32() {
        let dev = setup_iommu_for_wrappers();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // write_plain<u32> at IOVA offset 0x100.
        iommu_gm.write_plain::<u32>(0x100, &0xDEAD_BEEFu32).unwrap();

        let val: u32 = dev.shared.guest_memory.read_plain(0xA_0100).unwrap();
        assert_eq!(val, 0xDEAD_BEEF);
    }

    #[test]
    fn test_translating_memory_read_plain_u32() {
        let dev = setup_iommu_for_wrappers();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        dev.shared
            .guest_memory
            .write_plain::<u32>(0xA_0200, &0xCAFE_BABEu32)
            .unwrap();

        let val: u32 = iommu_gm.read_plain(0x200).unwrap();
        assert_eq!(val, 0xCAFE_BABE);
    }

    #[test]
    fn test_translating_memory_fill() {
        let dev = setup_iommu_for_wrappers();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Fill 64 bytes at IOVA 0x300 with 0xAB.
        iommu_gm.fill_at(0x300, 0xAB, 64).unwrap();

        let mut buf = [0u8; 64];
        dev.shared.guest_memory.read_at(0xA_0300, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn test_translating_memory_large_intra_page() {
        let dev = setup_iommu_for_wrappers();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Write a full 4KB page via IOMMU-translated memory.
        let data: Vec<u8> = (0..4096u32).map(|i| (i & 0xFF) as u8).collect();
        iommu_gm.write_at(0x0, &data).unwrap();

        let mut readback = vec![0u8; 4096];
        dev.shared
            .guest_memory
            .read_at(0xA_0000, &mut readback)
            .unwrap();
        assert_eq!(readback, data);

        // Read it back through the IOMMU too.
        let mut via_iommu = vec![0u8; 4096];
        iommu_gm.read_at(0x0, &mut via_iommu).unwrap();
        assert_eq!(via_iommu, data);
    }

    #[test]
    fn test_translating_memory_cross_page_read() {
        let dev = setup_iommu_two_pages();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Write known patterns at the end of page 0 and start of page 1
        // in the raw guest memory (non-contiguous GPAs).
        dev.shared
            .guest_memory
            .write_at(0xA_0FF0, &[0x11; 16])
            .unwrap();
        dev.shared
            .guest_memory
            .write_at(0xC_0000, &[0x22; 16])
            .unwrap();

        // Read 32 bytes spanning IOVA 0x0FF0–0x100F (crosses page boundary).
        let mut buf = [0u8; 32];
        iommu_gm.read_at(0x0FF0, &mut buf).unwrap();

        assert_eq!(&buf[..16], &[0x11; 16], "first half from page 0");
        assert_eq!(&buf[16..], &[0x22; 16], "second half from page 1");
    }

    #[test]
    fn test_translating_memory_cross_page_write() {
        let dev = setup_iommu_two_pages();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Write 32 bytes spanning IOVA 0x0FF0–0x100F.
        let data: Vec<u8> = (0..32u8).collect();
        iommu_gm.write_at(0x0FF0, &data).unwrap();

        // Verify first 16 bytes landed at GPA 0xA_0FF0.
        let mut buf0 = [0u8; 16];
        dev.shared
            .guest_memory
            .read_at(0xA_0FF0, &mut buf0)
            .unwrap();
        assert_eq!(&buf0, &data[..16]);

        // Verify last 16 bytes landed at GPA 0xC_0000.
        let mut buf1 = [0u8; 16];
        dev.shared
            .guest_memory
            .read_at(0xC_0000, &mut buf1)
            .unwrap();
        assert_eq!(&buf1, &data[16..]);
    }

    #[test]
    fn test_translating_memory_cross_page_fill() {
        let dev = setup_iommu_two_pages();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Fill 256 bytes spanning the page boundary at IOVA 0x0F80–0x107F.
        iommu_gm.fill_at(0x0F80, 0xCC, 256).unwrap();

        // First 128 bytes at GPA 0xA_0F80.
        let mut buf0 = [0u8; 128];
        dev.shared
            .guest_memory
            .read_at(0xA_0F80, &mut buf0)
            .unwrap();
        assert!(buf0.iter().all(|&b| b == 0xCC));

        // Next 128 bytes at GPA 0xC_0000.
        let mut buf1 = [0u8; 128];
        dev.shared
            .guest_memory
            .read_at(0xC_0000, &mut buf1)
            .unwrap();
        assert!(buf1.iter().all(|&b| b == 0xCC));
    }

    #[test]
    fn test_translating_memory_subrange_cross_page_write() {
        let dev = setup_iommu_two_pages();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        let subrange = iommu_gm.subrange(0x0FF0, 0x20, true).unwrap();
        let data: Vec<u8> = (0..32u8).map(|n| n.wrapping_add(0x40)).collect();
        subrange.write_at(0, &data).unwrap();

        let mut buf0 = [0u8; 16];
        dev.shared
            .guest_memory
            .read_at(0xA_0FF0, &mut buf0)
            .unwrap();
        assert_eq!(&buf0, &data[..16]);

        let mut buf1 = [0u8; 16];
        dev.shared
            .guest_memory
            .read_at(0xC_0000, &mut buf1)
            .unwrap();
        assert_eq!(&buf1, &data[16..]);
    }

    #[test]
    fn test_translating_memory_paged_range_write() {
        let dev = setup_iommu_two_pages();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        let iova_gpns = [0, 1];
        let range = guestmem::ranges::PagedRange::new(0x0FF0, 0x20, &iova_gpns).unwrap();
        let data: Vec<u8> = (0..32u8).map(|n| n.wrapping_add(0x80)).collect();
        iommu_gm.write_range(&range, &data).unwrap();

        let mut buf0 = [0u8; 16];
        dev.shared
            .guest_memory
            .read_at(0xA_0FF0, &mut buf0)
            .unwrap();
        assert_eq!(&buf0, &data[..16]);

        let mut buf1 = [0u8; 16];
        dev.shared
            .guest_memory
            .read_at(0xC_0000, &mut buf1)
            .unwrap();
        assert_eq!(&buf1, &data[16..]);
    }

    #[test]
    fn test_translating_memory_multi_region_target() {
        let dev = setup_iommu_multi_region_target();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        let data: Vec<u8> = (0..32u8).map(|n| n.wrapping_add(0x20)).collect();
        iommu_gm.write_at(0x0FF0, &data).unwrap();

        let mut buf0 = [0u8; 16];
        dev.shared
            .guest_memory
            .read_at(0x10_0FF0, &mut buf0)
            .unwrap();
        assert_eq!(&buf0, &data[..16]);

        let mut buf1 = [0u8; 16];
        dev.shared
            .guest_memory
            .read_at(0x10_1000, &mut buf1)
            .unwrap();
        assert_eq!(&buf1, &data[16..]);
    }

    #[test]
    fn test_translating_memory_cross_page_single_byte() {
        let dev = setup_iommu_two_pages();
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Write single bytes at the last byte of page 0 and first byte of page 1.
        iommu_gm.write_at(0x0FFF, &[0xAA]).unwrap();
        iommu_gm.write_at(0x1000, &[0xBB]).unwrap();

        let mut b0 = [0u8; 1];
        dev.shared.guest_memory.read_at(0xA_0FFF, &mut b0).unwrap();
        assert_eq!(b0, [0xAA]);

        let mut b1 = [0u8; 1];
        dev.shared.guest_memory.read_at(0xC_0000, &mut b1).unwrap();
        assert_eq!(b1, [0xBB]);
    }

    #[test]
    fn test_translating_memory_cross_page_fault_second_page() {
        // Map only page 0, leave page 1 unmapped. A cross-page access
        // should fault when it reaches the unmapped second page.
        let dev = setup_iommu_for_wrappers(); // only IOVA page 0 mapped
        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Read 32 bytes starting at IOVA 0x0FF0 — last 16 bytes cross into
        // unmapped page 1.
        let mut buf = [0u8; 32];
        let result = iommu_gm.read_at(0x0FF0, &mut buf);
        assert!(
            result.is_err(),
            "cross-page read into unmapped page should fault"
        );
    }

    /// Test 3-level page table translation with high IOVAs (like Linux's
    /// reverse IOVA allocator uses: 0xFFFFF000, 0xFFFFE000, etc.).
    ///
    /// This mimics the real page table layout a Linux 6.1 guest creates
    /// for an AMD IOMMU with mode=3 (3-level, 39-bit VA space).
    #[test]
    fn test_translating_memory_3level_high_iova() {
        let guest_memory = GuestMemory::allocate(0x20_0000); // 2MB
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 512);

        // Build 3-level page tables mapping:
        //   IOVA 0xFFFFF000 → GPA 0x15_0000
        //   IOVA 0xFFFFE000 → GPA 0x15_1000
        //
        // With mode=3 (39-bit VA space):
        //   L3 index = (iova >> 30) & 0x1FF
        //   L2 index = (iova >> 21) & 0x1FF
        //   L1 index = (iova >> 12) & 0x1FF
        //
        // For IOVA 0xFFFFF000:
        //   L3 index = (0xFFFFF000 >> 30) & 0x1FF = 3
        //   L2 index = (0xFFFFF000 >> 21) & 0x1FF = 511
        //   L1 index = (0xFFFFF000 >> 12) & 0x1FF = 511
        //
        // For IOVA 0xFFFFE000:
        //   L3 index = 3, L2 index = 511, L1 index = 510

        let l3_gpa = 0x5_0000u64;
        let l2_gpa = 0x6_0000u64;
        let l1_gpa = 0x7_0000u64;
        let target0_gpa = 0x15_0000u64;
        let target1_gpa = 0x15_1000u64;

        // L3[3] → L2
        write_pte(
            &dev,
            l3_gpa,
            3,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        // L2[511] → L1
        write_pte(
            &dev,
            l2_gpa,
            511,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        // L1[511] → target0 (IOVA 0xFFFFF000)
        write_pte(
            &dev,
            l1_gpa,
            511,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target0_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        // L1[510] → target1 (IOVA 0xFFFFE000)
        write_pte(
            &dev,
            l1_gpa,
            510,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        // DTE for the assigned wrapper test bus: 3-level page table.
        let dte = make_dte_with_translation(l3_gpa, 3, true, true, 1);
        write_dte(&dev, devtab_gpa, WRAPPER_DEVICE_ID, &dte);

        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Write test pattern at GPA 0x15_0000 (target for IOVA 0xFFFFF000).
        dev.shared
            .guest_memory
            .write_at(target0_gpa, &[0xDE, 0xAD, 0xBE, 0xEF])
            .unwrap();

        // Read via IOVA 0xFFFFF000 through IOMMU translation.
        let mut buf = [0u8; 4];
        iommu_gm.read_at(0xFFFFF000, &mut buf).unwrap();
        assert_eq!(buf, [0xDE, 0xAD, 0xBE, 0xEF]);

        // Write via IOVA 0xFFFFF100.
        iommu_gm.write_at(0xFFFFF100, &[0xCA, 0xFE]).unwrap();
        let mut buf2 = [0u8; 2];
        dev.shared
            .guest_memory
            .read_at(target0_gpa + 0x100, &mut buf2)
            .unwrap();
        assert_eq!(buf2, [0xCA, 0xFE]);

        // Write via IOVA 0xFFFFE000 (second mapped page).
        iommu_gm
            .write_at(0xFFFFE000, &[0x42, 0x43, 0x44, 0x45])
            .unwrap();
        let mut buf3 = [0u8; 4];
        dev.shared
            .guest_memory
            .read_at(target1_gpa, &mut buf3)
            .unwrap();
        assert_eq!(buf3, [0x42, 0x43, 0x44, 0x45]);

        // write_plain<u32> via IOVA (the NVMe shadow doorbell path).
        iommu_gm
            .write_plain::<u32>(0xFFFFF200, &0xDEAD_BEEFu32)
            .unwrap();
        let val: u32 = dev
            .shared
            .guest_memory
            .read_plain(target0_gpa + 0x200)
            .unwrap();
        assert_eq!(val, 0xDEAD_BEEF);

        // Verify subrange works (the virtio path).
        let sub = iommu_gm.subrange(0xFFFFF000, 0x1000, false).unwrap();
        sub.write_at(0x300, &[0xAA, 0xBB]).unwrap();
        let mut buf4 = [0u8; 2];
        dev.shared
            .guest_memory
            .read_at(target0_gpa + 0x300, &mut buf4)
            .unwrap();
        assert_eq!(buf4, [0xAA, 0xBB]);

        // Cross-page read spanning IOVA 0xFFFFEFFF–0xFFFFF000.
        dev.shared
            .guest_memory
            .write_at(target1_gpa + 0xFFC, &[0x11, 0x22, 0x33, 0x44])
            .unwrap();
        dev.shared
            .guest_memory
            .write_at(target0_gpa, &[0x55, 0x66, 0x77, 0x88])
            .unwrap();
        let mut buf5 = [0u8; 8];
        iommu_gm.read_at(0xFFFFEFFC, &mut buf5).unwrap();
        assert_eq!(buf5, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
    }

    // =========================================================================
    // Regression tests for pointer/overflow hardening
    // =========================================================================

    /// §3.4.15: If software sets the command buffer tail pointer to an offset
    /// beyond the buffer length, IOMMU behavior is undefined. The emulator
    /// masks to the buffer size to prevent an infinite spin.
    #[test]
    fn test_cmdbuf_out_of_range_tail_no_spin() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Command buffer is 256 entries (log2=8), so valid tail range is 0..255.
        // 300 % 256 = 44, so after masking we need valid commands at entries 0..43.
        for i in 0..44 {
            write_cmd(&dev, i, &invalidate_devtab_entry(0x01));
        }

        // Set tail pointer well beyond the buffer size (entry index 300).
        // Without masking, head would never equal tail and spin forever.
        let out_of_range_tail = CmdBufTail::new().with_tail_ptr(300);
        mmio_write64(
            &mut dev,
            MmioRegister::CMD_BUF_TAIL.0 as u64,
            out_of_range_tail.into_bits(),
        );

        // The test completing without hanging proves the fix works.
        // Head should have advanced to the masked tail position: 300 % 256 = 44.
        let head =
            CmdBufHead::from_bits(mmio_read64(&mut dev, MmioRegister::CMD_BUF_HEAD.0 as u64));
        assert_eq!(
            head.head_ptr(),
            300 % 256,
            "head should stop at tail masked to buffer size"
        );
    }

    /// §3.4.15: If software sets the event log head pointer to an offset
    /// beyond the buffer length, IOMMU behavior is undefined. The emulator
    /// masks to the buffer size so the overflow check stays within the ring.
    #[test]
    fn test_evtlog_out_of_range_head_masked() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Set event log head to an out-of-range value (entry index 300).
        let out_of_range_head = EvtLogHead::new().with_head_ptr(300);
        mmio_write64(
            &mut dev,
            MmioRegister::EVT_LOG_HEAD.0 as u64,
            out_of_range_head.into_bits(),
        );

        // Write an event — should not write outside the ring or panic.
        let event = EventEntry::io_page_fault(0xBEEF, 0x0001, false, false, 0x0);
        dev.write_event(event);

        // Tail should have advanced (event was written successfully).
        let tail =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));
        assert!(tail.tail_ptr() > 0, "event should have been logged");
    }

    /// A 2-byte DMA read at IOVA u64::MAX must hit the `checked_add`
    /// overflow guard after the first 1-byte chunk succeeds.
    ///
    /// Setup: 1-level page table with L1[511] mapped to a backed GPA.
    /// IOVA u64::MAX has L1 index 511 and page offset 0xFFF, so the
    /// first chunk is 1 byte and translates successfully. Advancing
    /// the IOVA for the second chunk requires u64::MAX + 1, which
    /// overflows and returns an error.
    #[test]
    fn test_translating_memory_iova_overflow() {
        let guest_memory = GuestMemory::allocate(0x10_0000);
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 512);

        // 1-level page table: L1[511] → GPA 0xA_0000 (backed memory).
        // IOVA u64::MAX has L1 index 511, page offset 0xFFF.
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xA_0000u64;
        write_pte(
            &dev,
            l1_gpa,
            511,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let dte = make_dte_with_translation(l1_gpa, 1, true, true, 1);
        write_dte(&dev, devtab_gpa, WRAPPER_DEVICE_ID, &dte);

        let shared = dev.shared_state().clone();
        let mock_msi = MockSignalMsi::new();
        let (iommu_gm, _msi) = device_context(
            &shared,
            wrapper_bus_range(),
            &dev.shared.guest_memory,
            mock_msi,
        );

        // Write a known byte at the target GPA + 0xFFF so the first
        // chunk's read succeeds with real data.
        dev.shared
            .guest_memory
            .write_at(target_gpa + 0xFFF, &[0x42])
            .unwrap();

        // 2-byte read at IOVA u64::MAX:
        //   chunk 1: 1 byte at page offset 0xFFF → translates to
        //            target_gpa + 0xFFF → succeeds
        //   advance: u64::MAX + 1 → checked_add overflow → error
        let mut buf = [0u8; 2];
        let result = iommu_gm.read_at(u64::MAX, &mut buf);
        assert!(result.is_err(), "should fail on IOVA overflow, not wrap");
    }

    // =========================================================================
    // Spec Deviation Regression Tests
    // =========================================================================

    /// §2.4.1: COMPLETION_WAIT with i=1 must set ComWaitInt in IOMMU_STATUS
    /// regardless of whether ComWaitIntEn is set. ComWaitIntEn only gates
    /// MSI delivery, not the status bit.
    ///
    /// Deviation #4: the emulator only sets ComWaitInt when ComWaitIntEn=1.
    #[test]
    fn test_completion_wait_sets_comwaitint_without_comwaitinten() {
        let mut dev = create_test_device_with_memory();

        // Enable IOMMU with command buffer and event log, but
        // ComWaitIntEn = false (interrupt delivery disabled).
        let cmd_base = CmdBufBase::new()
            .with_base_addr(0x0000 >> 12)
            .with_length(8);
        mmio_write64(
            &mut dev,
            MmioRegister::CMD_BUF_BASE.0 as u64,
            cmd_base.into_bits(),
        );
        let evt_base = EvtLogBase::new()
            .with_base_addr(0x1000 >> 12)
            .with_length(8);
        mmio_write64(
            &mut dev,
            MmioRegister::EVT_LOG_BASE.0 as u64,
            evt_base.into_bits(),
        );
        let ctrl = IommuCtrl::new()
            .with_iommu_en(true)
            .with_cmd_buf_en(true)
            .with_evt_log_en(true)
            .with_com_wait_int_en(false); // <-- interrupt delivery disabled
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_CTRL.0 as u64,
            ctrl.into_bits(),
        );

        // Issue COMPLETION_WAIT with i=1.
        write_cmd(&dev, 0, &completion_wait_interrupt());
        poke_tail(&mut dev, 1);

        // Per spec §2.4.1: "If the i bit is set, the IOMMU sets
        // MMIO Offset 2020h[ComWaitInt]."
        // This must happen regardless of ComWaitIntEn.
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(
            status.com_wait_int(),
            "ComWaitInt must be set when COMPLETION_WAIT has i=1, \
             even when ComWaitIntEn is disabled (spec §2.4.1)"
        );
    }

    /// §2.5.1: When EventOverflow is set and EventIntEn=1, the IOMMU must
    /// deliver an MSI so the guest discovers the overflow promptly.
    ///
    /// Deviation #5: the emulator sets EventOverflow but does not deliver
    /// the MSI.
    #[test]
    fn test_evtlog_overflow_delivers_msi() {
        let guest_memory = GuestMemory::allocate(0x10000);
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let msi_controller = pci_core::test_helpers::TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        // Enable MSI on PCI config space.
        let iommu_cap_header = pci_read(&mut dev, 0x40);
        let msi_cap_offset = ((iommu_cap_header >> 8) & 0xFF) as u16;
        let _ = dev.pci_cfg_write(msi_cap_offset + 4, 0xFEE0_0000);
        let _ = dev.pci_cfg_write(msi_cap_offset + 8, 0);
        let _ = dev.pci_cfg_write(msi_cap_offset + 12, 0x41);
        let control = pci_read(&mut dev, msi_cap_offset);
        let _ = dev.pci_cfg_write(msi_cap_offset, control | (1 << 16));

        setup_iommu_enabled(&mut dev);

        // Drain any MSIs from setup.
        while msi_controller.get_next_interrupt().is_some() {}

        // Fill the event log (255 entries to leave ring "full").
        for i in 0..255 {
            let event = EventEntry::io_page_fault(i as u16, 0x0001, false, false, 0x0);
            dev.write_event(event);
        }

        // Drain MSIs from the 255 normal events.
        while msi_controller.get_next_interrupt().is_some() {}

        // Clear EventLogInt so we observe only the overflow path.
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_STATUS.0 as u64,
            IommuStatus::new().with_evt_log_int(true).into_bits(),
        );

        // This event should trigger overflow.
        let event = EventEntry::io_page_fault(0xFFFF, 0x0001, false, false, 0x0);
        dev.write_event(event);

        // Verify overflow is set.
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(status.evt_overflow(), "EventOverflow should be set");

        // Per spec §2.5.1: overflow with EventIntEn=1 must deliver MSI.
        let msi = msi_controller.get_next_interrupt();
        assert!(
            msi.is_some(),
            "MSI must be delivered on event log overflow when EventIntEn=1 (spec §2.5.1)"
        );
    }

    /// §2.5: "When an event log overflow condition exists, the IOMMU
    /// ceases recording events until software resets the event logging
    /// function." Events must be dropped while EventOverflow is set.
    ///
    /// Deviation #6: the emulator continues writing events as long as
    /// the ring buffer has physical space (e.g., after head advances).
    #[test]
    fn test_evtlog_drops_events_while_overflow_set() {
        let mut dev = create_test_device_with_memory();
        setup_iommu_enabled(&mut dev);

        // Fill the event log to trigger overflow.
        for i in 0..255 {
            let event = EventEntry::io_page_fault(i as u16, 0x0001, false, false, 0x0);
            dev.write_event(event);
        }
        // Trigger overflow.
        dev.write_event(EventEntry::io_page_fault(0xFFFF, 0x0001, false, false, 0x0));

        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(status.evt_overflow(), "precondition: overflow is set");

        // Advance head to free physical space, but do NOT clear EventOverflow.
        let head = EvtLogHead::new().with_head_ptr(10);
        mmio_write64(
            &mut dev,
            MmioRegister::EVT_LOG_HEAD.0 as u64,
            head.into_bits(),
        );

        // Record the tail before attempting another event.
        let tail_before =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));

        // Write another event while EventOverflow is still set.
        dev.write_event(EventEntry::io_page_fault(0xBEEF, 0x0001, false, false, 0x0));

        // Per spec: events must be dropped while overflow is set.
        let tail_after =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));
        assert_eq!(
            tail_before.tail_ptr(),
            tail_after.tail_ptr(),
            "event log tail must not advance while EventOverflow is set — \
             events must be dropped until software clears overflow (spec §2.5)"
        );
    }

    /// §2.5.7: A hardware error reading from the command buffer must
    /// generate a COMMAND_HARDWARE_ERROR event (code 0x06) and halt the
    /// command buffer.
    ///
    /// Deviation #7: the emulator halts the command buffer but does not
    /// log the event.
    #[test]
    fn test_cmdbuf_read_failure_logs_hardware_error_event() {
        // Allocate only 0x10000 bytes of guest memory.
        // Place the event log within valid memory and the command buffer
        // beyond it so that reading a command entry fails.
        let guest_memory = GuestMemory::allocate(0x10000);
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let mut dev = AmdIommuDevice::new(guest_memory, test_config(), msi_conn.target());

        // Event log at valid GPA 0x1000.
        let evt_base = EvtLogBase::new()
            .with_base_addr(0x1000 >> 12)
            .with_length(8);
        mmio_write64(
            &mut dev,
            MmioRegister::EVT_LOG_BASE.0 as u64,
            evt_base.into_bits(),
        );

        // Command buffer at GPA 0x2_0000 — beyond the 0x10000 allocation,
        // so reading a command entry will fail.
        let cmd_base = CmdBufBase::new()
            .with_base_addr(0x2_0000 >> 12)
            .with_length(8);
        mmio_write64(
            &mut dev,
            MmioRegister::CMD_BUF_BASE.0 as u64,
            cmd_base.into_bits(),
        );

        let ctrl = IommuCtrl::new()
            .with_iommu_en(true)
            .with_cmd_buf_en(true)
            .with_evt_log_en(true)
            .with_evt_int_en(true);
        mmio_write64(
            &mut dev,
            MmioRegister::IOMMU_CTRL.0 as u64,
            ctrl.into_bits(),
        );

        // Poke tail to trigger a read from the unmapped command buffer.
        poke_tail(&mut dev, 1);

        // CmdBufRun should be cleared (halted).
        let status =
            IommuStatus::from_bits(mmio_read64(&mut dev, MmioRegister::IOMMU_STATUS.0 as u64));
        assert!(
            !status.cmd_buf_run(),
            "CmdBufRun should be clear after command read failure"
        );

        // Per spec §2.5.7: a COMMAND_HARDWARE_ERROR event must be logged.
        let tail =
            EvtLogTail::from_bits(mmio_read64(&mut dev, MmioRegister::EVT_LOG_TAIL.0 as u64));
        assert_eq!(
            tail.tail_ptr(),
            1,
            "event log should contain one COMMAND_HARDWARE_ERROR entry (spec §2.5.7)"
        );

        let event: EventEntry = dev
            .shared
            .guest_memory
            .read_plain(0x1000)
            .expect("should read event");
        assert_eq!(
            event.event_code(),
            EventCode::COMMAND_HARDWARE_ERROR,
            "event must be COMMAND_HARDWARE_ERROR (code 0x06) per spec §2.5.7"
        );
    }

    /// §2.2.3: "Virtual addresses are [Mode×9+12]-bit." If any upper bits
    /// of the IOVA are non-zero, the IOMMU must generate an IO_PAGE_FAULT.
    ///
    /// Deviation #9: the emulator does not validate the IOVA width. With
    /// Mode=4 (48-bit VA), an IOVA with bits above 47 set is silently
    /// accepted and translated (the upper bits are ignored by va_index).
    #[test]
    fn test_translate_rejects_iova_exceeding_va_width() {
        let mut dev = create_test_device_for_translation();
        let devtab_gpa = 0x1_0000;
        setup_iommu_with_devtab(&mut dev, devtab_gpa, 256);

        // Set up a 4-level page table mapping IOVA 0x0 → GPA 0xA_0000.
        let l4_gpa = 0x2_0000u64;
        let l3_gpa = 0x3_0000u64;
        let l2_gpa = 0x4_0000u64;
        let l1_gpa = 0x5_0000u64;
        let target_gpa = 0xA_0000u64;

        write_pte(
            &dev,
            l4_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(3)
                .with_address(l3_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l3_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(2)
                .with_address(l2_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l2_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(1)
                .with_address(l1_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );
        write_pte(
            &dev,
            l1_gpa,
            0,
            &IommuPte::new()
                .with_pr(true)
                .with_next_level(0)
                .with_address(target_gpa >> 12)
                .with_ir(true)
                .with_iw(true),
        );

        let dte = make_dte_with_translation(l4_gpa, 4, true, true, 1);
        write_dte(&dev, devtab_gpa, 0x10, &dte);

        // Sanity: IOVA 0x0 should translate successfully.
        assert!(dev.translate(0x10, 0x0, false).is_ok());

        // Mode=4 → VA width = 4*9+12 = 48 bits. Bits 63:48 must be zero.
        // An IOVA with bit 48 set exceeds the configured VA width and must
        // produce an IO_PAGE_FAULT, not silently alias to IOVA 0x0.
        let bad_iova = 1u64 << 48; // bit 48 set, all lower bits zero
        let result = dev.translate(0x10, bad_iova, false);
        assert!(
            result.is_err(),
            "IOVA {:#x} has bits above the 48-bit VA width (Mode=4) and must \
             be rejected with IO_PAGE_FAULT (spec §2.2.3)",
            bad_iova
        );
        match result.unwrap_err() {
            IommuFault::IoPageFault { device_id, .. } => {
                assert_eq!(device_id, 0x10);
            }
            other => panic!("expected IoPageFault, got {:?}", other),
        }
    }
}
