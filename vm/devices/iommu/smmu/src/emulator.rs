// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 device emulator — register file and MMIO dispatch.

use crate::shared::SmmuSharedState;
use crate::shared::TranslationPolicy;
use crate::spec::commands::CmdCfgiCd;
use crate::spec::commands::CmdCfgiSte;
use crate::spec::commands::CmdCfgiSteRange;
use crate::spec::commands::CmdEntry;
use crate::spec::commands::CmdOpcode;
use crate::spec::commands::CmdSync;
use crate::spec::commands::SyncCs;
use crate::spec::registers;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use std::ops::RangeInclusive;
use std::sync::Arc;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

/// Output address size (OAS) resolution policy for the SMMU.
///
/// The OAS the SMMU advertises in IDR5 is resolved in two stages. The initial
/// advertised value is carried by this policy up front. For accelerated SMMUs
/// the final value also depends on the physical SMMU backing the device, which
/// is only known once a VFIO device is bound to iommufd — see
/// [`SmmuSharedState::bind_accel_viommu`].
///
/// Both variants carry a concrete OAS supplied by the caller. This crate
/// deliberately defines no default, so that the choice for `auto` is made once
/// by the wiring layer and every SMMU backend advertises the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmmuOasPolicy {
    /// Advertise `provisional` initially. For non-accel SMMUs this is the final
    /// advertised OAS. For accel SMMUs it is a provisional value that is
    /// replaced by the host SMMU's OAS when a device attaches.
    Auto {
        /// Initial advertised OAS in bits, used until (for accel) the host
        /// SMMU's OAS is adopted at device attach.
        provisional: u8,
    },
    /// Use a fixed OAS in bits. For accel, this is an upper bound that must
    /// not exceed the host SMMU's OAS (attach fails otherwise).
    Fixed(u8),
}

/// Capabilities of the physical SMMUv3 backing an accelerated vSMMU.
///
/// Decoded from a host SMMUv3's `IDR0..IDR5` register values (as returned by
/// `IOMMU_GET_HW_INFO`) via [`HostSmmuCaps::from_idr`], and handed to
/// [`SmmuSharedState::bind_accel_viommu`], which finalizes the host-derived
/// vSMMU parameters and validates host/guest compatibility the first time a
/// VFIO device binds. Keeping this a plain value type (with the IDR decoding
/// owned by this crate) avoids a dependency from the `smmu` crate on the
/// iommufd bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostSmmuCaps {
    /// Host SMMUv3 output address size (IDR5.OAS field). Resolved to a bit
    /// width (and validated) in [`SmmuSharedState::bind_accel_viommu`], since
    /// the encoding may be one the architecture reserves.
    pub(crate) oas: crate::spec::cd::Ips,
    /// Host translation table format support (IDR0.TTF).
    pub(crate) ttf: registers::Idr0Ttf,
    /// Host translation table endianness support (IDR0.TTENDIAN).
    pub(crate) ttendian: registers::Idr0TtEndian,
    /// Host 4KB translation granule support (IDR5.GRAN4K).
    pub(crate) gran4k: bool,
}

impl HostSmmuCaps {
    /// Decodes the host SMMUv3's capabilities from its `IDR0..IDR5` register
    /// values, as returned by `IOMMU_GET_HW_INFO` (`idr[n]` is `IDRn`).
    ///
    /// The caller (the VFIO layer) supplies the raw register array; this
    /// crate owns the register layout, so decoding lives here rather than in
    /// the iommufd bindings. This decode is total — the only field whose value
    /// may be unrecognized is OAS, whose `Ips` encoding is interpreted in
    /// [`SmmuSharedState::bind_accel_viommu`].
    pub fn from_idr(idr: [u32; 6]) -> Self {
        let idr0 = registers::Idr0::from(idr[0]);
        let idr5 = registers::Idr5::from(idr[5]);

        Self {
            oas: crate::spec::cd::Ips(idr5.oas()),
            ttf: registers::Idr0Ttf::from(idr0.ttf()),
            ttendian: registers::Idr0TtEndian(idr0.ttendian()),
            gran4k: idr5.gran4k(),
        }
    }
}

/// SMMUv3 device configuration.
#[derive(Debug, Clone)]
pub struct SmmuConfig {
    /// Number of StreamID bits (max 32, typically 16).
    pub sidsize: u8,
    /// How the advertised OAS is resolved. Also carries the initial advertised
    /// OAS bits: the fixed value for [`SmmuOasPolicy::Fixed`], or the
    /// provisional value for [`SmmuOasPolicy::Auto`].
    pub oas_policy: SmmuOasPolicy,
    /// Enable HW-accelerated nested S1 translation (iommufd).
    ///
    /// When true, VFIO cdev devices behind this SMMU use hardware-
    /// accelerated translation. The SMMU still emulates registers
    /// and dispatches CMDQ commands to iommufd backends.
    pub accel: bool,
}

/// Per-queue MSI configuration registers.
#[derive(Debug, Default, Inspect)]
struct MsiConfig {
    /// MSI address (64-bit, from IRQ_CFG0).
    addr: u64,
    /// MSI data payload (32-bit, from IRQ_CFG1).
    data: u32,
    /// MSI attributes (32-bit, from IRQ_CFG2).
    attr: u32,
}

/// SMMUv3 device emulator.
///
/// Implements MMIO register access for the SMMUv3 register file. The device
/// responds to reads/writes across a 128KB region (page 0 + page 1).
#[derive(InspectMut)]
pub struct SmmuDevice {
    // Static configuration
    #[inspect(skip)]
    mmio_region: (&'static str, RangeInclusive<u64>),
    #[inspect(skip)]
    mmio_base: u64,

    // Guest memory for reading queues and page tables.
    #[inspect(skip)]
    guest_memory: GuestMemory,

    // Shared state for per-device translation wrappers.
    #[inspect(skip)]
    shared_state: Arc<SmmuSharedState>,

    // Identification registers (read-only, set at construction).
    idr0: registers::Idr0,
    idr1: registers::Idr1,
    #[inspect(hex)]
    idr2: u32,
    #[inspect(hex)]
    idr3: u32,
    #[inspect(hex)]
    idr4: u32,
    idr5: registers::Idr5,
    #[inspect(hex)]
    iidr: u32,
    #[inspect(hex)]
    aidr: u32,

    // Control registers.
    cr0: registers::Cr0,
    cr1: registers::Cr1,
    cr2: registers::Cr2,
    gbpa: registers::Gbpa,

    // Interrupt control.
    irq_ctrl: registers::IrqCtrl,
    irq_ctrlack: registers::IrqCtrl,

    // Stream table base.
    #[inspect(hex)]
    strtab_base: u64,
    strtab_base_cfg: registers::StrtabBaseCfg,

    // Command queue.
    #[inspect(hex)]
    cmdq_base: u64,
    cmdq_prod: u32,
    cmdq_cons: registers::CmdqCons,

    // Reusable scratch buffer for accumulating consecutive forwardable
    // invalidation commands during CMDQ processing. Flushed to the host as a
    // single batched `IOMMU_HWPT_INVALIDATE` at each synchronization or
    // configuration boundary. Cleared (not reallocated) on each flush so
    // batching adds no per-command allocation.
    #[inspect(skip)]
    invalidation_batch: Vec<[u64; 2]>,

    // Event queue base register (raw value for MMIO read/write).
    // EVTQ producer/consumer state lives in SmmuSharedState.
    #[inspect(hex)]
    evtq_base: u64,

    // MSI configuration (stored for guest register access, not used for
    // interrupt delivery since IDR0.MSI=0).
    gerror_msi: MsiConfig,
    evtq_msi: MsiConfig,
    cmdq_msi: MsiConfig,
}

impl SmmuDevice {
    fn sanitize_cr0(value: u32) -> registers::Cr0 {
        let requested = registers::Cr0::from(value);
        registers::Cr0::new()
            .with_smmuen(requested.smmuen())
            .with_eventqen(requested.eventqen())
            .with_cmdqen(requested.cmdqen())
    }

    fn sanitize_gbpa(value: u32) -> registers::Gbpa {
        let requested = registers::Gbpa::from(value);
        registers::Gbpa::new().with_abort(requested.abort())
    }

    /// Creates a new SMMUv3 device.
    ///
    /// `mmio_base` is the physical address for the 128KB MMIO region.
    /// `guest_memory` is used for reading command/event queues and page tables.
    /// `evtq_irq` and `gerror_irq` are wired SPI interrupt lines for event
    /// queue and global error signaling.
    pub fn new(
        mmio_base: u64,
        guest_memory: GuestMemory,
        config: &SmmuConfig,
        evtq_irq: Option<LineInterrupt>,
        gerror_irq: Option<LineInterrupt>,
    ) -> Self {
        let idr0 = registers::Idr0::new()
            .with_s1p(true)
            .with_s2p(false)
            // AArch64 page tables only.
            .with_ttf(registers::Idr0Ttf::new().with_aarch64(true).into())
            .with_cohacc(true)
            .with_asid16(true)
            .with_msi(false)
            .with_ttendian(registers::Idr0TtEndian::LE.0) // Little-endian only
            .with_stall_model(0b01) // Stall not supported
            .with_term_model(true) // Terminate faults (no stall)
            // TODO: support 2-level stream tables (ST_LEVEL=0b01) so guests
            // with large SIDSIZE need not allocate one contiguous table. When
            // advertised, the host's stream table format is independent and
            // need not be validated (the guest table is never seen by the host
            // in the nested path).
            .with_st_level(0b00); // Linear stream table only

        let idr1 = registers::Idr1::new()
            .with_sidsize(config.sidsize)
            // TODO: support substreams (SSID/PASID). When SSIDSIZE > 0, the
            // accel path must validate the host SMMU's SSIDSIZE >= the
            // advertised value in resolve_host_caps.
            .with_ssidsize(0)
            .with_cmdqs(8) // 256 entries max
            .with_eventqs(8) // 256 entries max
            // ATTR_TYPES_OVR / ATTR_PERMS_OVR are left 0: this SMMU does not
            // support overriding incoming memory attributes/permissions. Per
            // the SMMUv3 spec that makes STE.{MTCFG, MemAttr, SHCFG, ALLOCCFG,
            // NSCFG, PRIVCFG, INSTCFG} RES 0 ("use incoming"), which the
            // accel path already strips from the nested STE and neither the
            // emulated nor accelerated translation path honors.
            .with_tables_preset(false)
            .with_queues_preset(false)
            .with_rel(false);

        // IDR3 is left at 0. TODO: support range invalidation (RIL). When
        // advertised (IDR3.RIL=1), the accel path must validate the host
        // SMMU supports RIL in resolve_host_caps.

        // The initial advertised OAS before any host resolution. For
        // non-accelerated SMMUs this is final; for accelerated SMMUs it is a
        // provisional value that may be raised or validated against the host
        // SMMU's OAS at device attach (see `resolve_host_caps`). The concrete
        // value is supplied by the caller in both cases.
        let oas_bits = match config.oas_policy {
            SmmuOasPolicy::Auto { provisional } => provisional,
            SmmuOasPolicy::Fixed(bits) => bits,
        };

        let idr5 = registers::Idr5::new()
            .with_oas(crate::spec::cd::Ips::from_bits(oas_bits).0)
            .with_gran4k(true)
            // TODO: support 16K/64K translation granules. When advertised, the
            // accel path must validate the host SMMU's GRAN16K/GRAN64K bits in
            // resolve_host_caps (host must support each granule the guest may
            // use).
            .with_gran16k(false)
            .with_gran64k(false);

        // GBPA resets to ABORT=0, so DMA bypasses (IOVA = GPA) while the SMMU
        // is disabled. This preserves boot-time passthrough for direct-boot
        // and UEFI, which use assigned devices before the guest enables the
        // SMMU. The non-accel translate path and the accel policy computation
        // both consult GBPA.ABORT for the disabled state, so leaving it set
        // here would fail-close at boot.
        let gbpa = registers::Gbpa::new().with_abort(false);

        let shared_state = SmmuSharedState::new(
            guest_memory.clone(),
            oas_bits,
            config.oas_policy,
            config.accel,
            evtq_irq,
            gerror_irq,
        );
        // Keep the shared state's mirror of GBPA.ABORT in sync with the
        // register's reset value.
        shared_state.set_gbpa_abort(gbpa.abort());

        SmmuDevice {
            mmio_region: (
                "smmu",
                mmio_base..=mmio_base + registers::MMIO_REGION_SIZE - 1,
            ),
            mmio_base,
            guest_memory,
            shared_state,

            idr0,
            idr1,
            idr2: 0,
            idr3: 0,
            idr4: 0,
            idr5,
            iidr: 0,
            aidr: 0x03, // SMMUv3.3

            cr0: registers::Cr0::new(),
            cr1: registers::Cr1::new(),
            cr2: registers::Cr2::new(),
            gbpa,

            irq_ctrl: registers::IrqCtrl::new(),
            irq_ctrlack: registers::IrqCtrl::new(),

            strtab_base: 0,
            strtab_base_cfg: registers::StrtabBaseCfg::new(),

            cmdq_base: 0,
            cmdq_prod: 0,
            cmdq_cons: registers::CmdqCons::new(),

            invalidation_batch: Vec::new(),

            evtq_base: 0,

            gerror_msi: MsiConfig::default(),
            evtq_msi: MsiConfig::default(),
            cmdq_msi: MsiConfig::default(),
        }
    }

    /// Returns the shared state for creating per-device translation wrappers.
    pub fn shared_state(&self) -> &Arc<SmmuSharedState> {
        &self.shared_state
    }

    /// Handles a 32-bit MMIO read at the given offset from the device base.
    fn read_reg32(&self, offset: u32) -> u32 {
        match offset as u16 {
            registers::IDR0 => self.idr0.into(),
            registers::IDR1 => self.idr1.into(),
            registers::IDR2 => self.idr2,
            registers::IDR3 => self.idr3,
            registers::IDR4 => self.idr4,
            registers::IDR5 => {
                // OAS is resolved dynamically (accel finalizes it against the
                // host SMMU at device-attach time); the granule bits are fixed.
                let oas = self.shared_state.oas_bits();
                self.idr5
                    .with_oas(crate::spec::cd::Ips::from_bits(oas).0)
                    .into()
            }
            registers::IIDR => self.iidr,
            registers::AIDR => self.aidr,

            registers::CR0 => self.cr0.into(),
            registers::CR0ACK => self.cr0.into(),
            registers::CR1 => self.cr1.into(),
            registers::CR2 => self.cr2.into(),
            registers::STATUSR => 0,
            registers::GBPA => self.gbpa.into(),
            registers::AGBPA => 0,

            registers::IRQ_CTRL => self.irq_ctrl.into(),
            registers::IRQ_CTRLACK => self.irq_ctrlack.into(),

            registers::GERROR => self.shared_state.read_gerror().into(),
            registers::GERRORN => self.shared_state.read_gerrorn().into(),

            registers::STRTAB_BASE_CFG => self.strtab_base_cfg.into(),

            registers::CMDQ_PROD => self.cmdq_prod,
            registers::CMDQ_CONS => self.cmdq_cons.into(),

            // Page 0 read of GERROR_IRQ_CFG1
            registers::GERROR_IRQ_CFG1 => self.gerror_msi.data,
            registers::GERROR_IRQ_CFG2 => self.gerror_msi.attr,

            // Page 0 read of EVENTQ_IRQ_CFG1
            registers::EVENTQ_IRQ_CFG1 => self.evtq_msi.data,
            registers::EVENTQ_IRQ_CFG2 => self.evtq_msi.attr,

            _ => {
                tracelimit::warn_ratelimited!(offset, "smmu: unhandled 32-bit MMIO read");
                0
            }
        }
    }

    /// Handles a 64-bit MMIO read at the given offset from the device base.
    fn read_reg64(&self, offset: u32) -> u64 {
        match offset as u16 {
            registers::STRTAB_BASE => self.strtab_base,
            registers::CMDQ_BASE => self.cmdq_base,
            registers::EVENTQ_BASE => self.evtq_base,
            registers::GERROR_IRQ_CFG0 => self.gerror_msi.addr,
            registers::EVENTQ_IRQ_CFG0 => self.evtq_msi.addr,
            _ => {
                tracelimit::warn_ratelimited!(offset, "smmu: unhandled 64-bit MMIO read");
                0
            }
        }
    }

    /// Handles a 32-bit MMIO write at the given offset.
    fn write_reg32(&mut self, offset: u32, value: u32) {
        match offset as u16 {
            // Read-only registers: ignore writes.
            registers::IDR0
            | registers::IDR1
            | registers::IDR2
            | registers::IDR3
            | registers::IDR4
            | registers::IDR5
            | registers::IIDR
            | registers::AIDR
            | registers::CR0ACK
            | registers::STATUSR
            | registers::IRQ_CTRLACK => {}

            registers::CR0 => {
                let requested = Self::sanitize_cr0(value);
                let previous = self.cr0;
                self.cr0 = requested;

                if requested.smmuen() != previous.smmuen() {
                    let mut policy = self.shared_state.translation_policy();
                    policy.enabled = requested.smmuen();
                    self.shared_state
                        .transition_translation_policy(policy, "SMMUEN transition");
                }

                self.shared_state.set_evtq_enabled(requested.eventqen());

                if !previous.cmdqen() && requested.cmdqen() {
                    self.process_cmdq();
                }
            }
            registers::CR1 => {
                self.cr1 = registers::Cr1::from(value);
            }
            registers::CR2 => {
                self.cr2 = registers::Cr2::from(value);
            }
            registers::GBPA => {
                let requested = registers::Gbpa::from(value);
                if !requested.update() {
                    return;
                }
                let gbpa = Self::sanitize_gbpa(value);

                if self.cr0.smmuen() {
                    // While enabled, GBPA only records the policy for a future
                    // disabled state and does not affect current STE policy.
                    self.shared_state.set_gbpa_abort(gbpa.abort());
                } else {
                    let mut policy = self.shared_state.translation_policy();
                    policy.gbpa_abort = gbpa.abort();
                    self.shared_state
                        .transition_translation_policy(policy, "GBPA transition");
                }
                self.gbpa = gbpa;
            }
            registers::IRQ_CTRL => {
                self.irq_ctrl = registers::IrqCtrl::from(value);
                // Immediate acknowledge.
                self.irq_ctrlack = self.irq_ctrl;
                self.shared_state
                    .set_irq_ctrl(self.irq_ctrl.eventq_irqen(), self.irq_ctrl.gerror_irqen());
            }
            registers::GERRORN => {
                self.shared_state.write_gerrorn(value);
            }

            registers::STRTAB_BASE_CFG => {
                let cfg = registers::StrtabBaseCfg::from(value);
                // Only linear stream tables are supported (IDR0.ST_LEVEL=0).
                // Force fmt to LINEAR if the guest programs anything else.
                if cfg.fmt() != registers::StrtabFmt::LINEAR.0 {
                    tracelimit::warn_ratelimited!(
                        fmt = cfg.fmt(),
                        "smmu: ignoring non-linear stream table format"
                    );
                }
                self.strtab_base_cfg = cfg.with_fmt(registers::StrtabFmt::LINEAR.0);
                self.sync_strtab_to_shared();
            }

            registers::CMDQ_PROD => {
                self.cmdq_prod = value;
                self.process_cmdq();
            }
            registers::CMDQ_CONS => {
                // Per IHI 0070H.a §6.3.28, CMDQ_CONS is RW when CMDQEN==0
                // (software initializes it before enabling the queue) and
                // RO when CMDQEN==1.
                if !self.cr0.cmdqen() {
                    self.cmdq_cons = registers::CmdqCons::from(value);
                }
            }

            registers::GERROR_IRQ_CFG1 => self.gerror_msi.data = value,
            registers::GERROR_IRQ_CFG2 => self.gerror_msi.attr = value,

            registers::EVENTQ_IRQ_CFG1 => self.evtq_msi.data = value,
            registers::EVENTQ_IRQ_CFG2 => self.evtq_msi.attr = value,

            _ => {
                tracelimit::warn_ratelimited!(offset, value, "smmu: unhandled 32-bit MMIO write");
            }
        }
    }

    /// Handles a 64-bit MMIO write at the given offset.
    fn write_reg64(&mut self, offset: u32, value: u64) {
        match offset as u16 {
            registers::STRTAB_BASE => {
                self.strtab_base = value;
                self.sync_strtab_to_shared();
            }
            registers::CMDQ_BASE => {
                self.cmdq_base = value;
            }
            registers::EVENTQ_BASE => {
                self.evtq_base = value;
                self.sync_evtq_to_shared();
            }
            registers::GERROR_IRQ_CFG0 => self.gerror_msi.addr = value,
            registers::EVENTQ_IRQ_CFG0 => self.evtq_msi.addr = value,

            _ => {
                tracelimit::warn_ratelimited!(offset, value, "smmu: unhandled 64-bit MMIO write");
            }
        }
    }

    /// Handles page 1 register reads (offset >= 0x10000).
    fn read_page1_reg32(&self, offset: u32) -> u32 {
        match offset {
            registers::EVENTQ_PROD_PAGE1 => self.shared_state.evtq_prod(),
            registers::EVENTQ_CONS_PAGE1 => self.shared_state.evtq_cons(),
            registers::CMDQ_IRQ_CFG1_PAGE1 => self.cmdq_msi.data,
            registers::CMDQ_IRQ_CFG2_PAGE1 => self.cmdq_msi.attr,
            _ => {
                tracelimit::warn_ratelimited!(offset, "smmu: unhandled page 1 32-bit MMIO read");
                0
            }
        }
    }

    /// Handles page 1 register reads (64-bit, offset >= 0x10000).
    fn read_page1_reg64(&self, offset: u32) -> u64 {
        match offset {
            registers::CMDQ_IRQ_CFG0_PAGE1 => self.cmdq_msi.addr,
            _ => {
                tracelimit::warn_ratelimited!(offset, "smmu: unhandled page 1 64-bit MMIO read");
                0
            }
        }
    }

    /// Handles page 1 register writes (offset >= 0x10000).
    fn write_page1_reg32(&mut self, offset: u32, value: u32) {
        match offset {
            registers::EVENTQ_PROD_PAGE1 => {
                // EVTQ_PROD on page 1 is writable by the SMMU only (for
                // writing events). Guest writes are ignored.
            }
            registers::EVENTQ_CONS_PAGE1 => {
                self.shared_state.set_evtq_cons(value);
            }
            registers::CMDQ_IRQ_CFG1_PAGE1 => self.cmdq_msi.data = value,
            registers::CMDQ_IRQ_CFG2_PAGE1 => self.cmdq_msi.attr = value,
            _ => {
                tracelimit::warn_ratelimited!(
                    offset,
                    value,
                    "smmu: unhandled page 1 32-bit MMIO write"
                );
            }
        }
    }

    /// Handles page 1 register writes (64-bit, offset >= 0x10000).
    fn write_page1_reg64(&mut self, offset: u32, value: u64) {
        match offset {
            registers::CMDQ_IRQ_CFG0_PAGE1 => self.cmdq_msi.addr = value,
            _ => {
                tracelimit::warn_ratelimited!(
                    offset,
                    value,
                    "smmu: unhandled page 1 64-bit MMIO write"
                );
            }
        }
    }

    // =========================================================================
    // Shared State Synchronization
    // =========================================================================

    /// Sync the stream table base address and size to the shared state.
    fn sync_strtab_to_shared(&self) {
        let base = registers::StrtabBase::from(self.strtab_base).addr();
        let log2size = self.strtab_base_cfg.log2size();
        self.shared_state.set_strtab(base, log2size);
    }

    /// Sync the event queue base address and size to the shared state.
    fn sync_evtq_to_shared(&self) {
        let base_addr = registers::QueueBase::from(self.evtq_base).addr();
        let raw_log2size = registers::QueueBase::from(self.evtq_base).log2size();
        let log2size = raw_log2size.min(self.idr1.eventqs());
        self.shared_state.set_evtq_config(base_addr, log2size);
    }

    // =========================================================================
    // Command Queue Processing
    // =========================================================================

    /// Returns the log2 size of the command queue from CMDQ_BASE,
    /// clamped to the maximum advertised in IDR1.CMDQS.
    fn cmdq_log2size(&self) -> u8 {
        let raw = registers::QueueBase::from(self.cmdq_base).log2size();
        let max = self.idr1.cmdqs();
        raw.min(max)
    }

    /// Returns the base GPA of the command queue from CMDQ_BASE.
    fn cmdq_base_addr(&self) -> u64 {
        registers::QueueBase::from(self.cmdq_base).addr()
    }

    /// Checks if CMDQ processing is enabled (CMDQEN set and SMMU enabled
    /// or at least CMDQEN in CR0).
    fn cmdq_enabled(&self) -> bool {
        self.cr0.cmdqen()
    }

    /// Returns true if the CMDQ has a pending (active, unacknowledged) error.
    fn cmdq_has_error(&self) -> bool {
        self.shared_state.cmdq_err_active()
    }

    /// Processes pending commands from `CMDQ_CONS` through `CMDQ_PROD`.
    ///
    /// Called when the guest advances `CMDQ_PROD` or enables a command queue
    /// that already contains commands.
    fn process_cmdq(&mut self) {
        if !self.cmdq_enabled() {
            return;
        }

        // Don't process if there's an outstanding CMDQ error.
        if self.cmdq_has_error() {
            return;
        }

        let log2size = self.cmdq_log2size() as u32;
        let max_entries = 1u32 << log2size;
        // The wrap mask includes the wrap bit: (2 * max_entries - 1).
        let index_mask = (max_entries << 1) - 1;
        let base_addr = self.cmdq_base_addr();

        // Extract the raw cons value (bits [19:0] include the wrap bit).
        let mut cons = self.cmdq_cons.rd();
        let prod = self.cmdq_prod & index_mask;

        // CMDQ index (with wrap bit) of the first entry in the currently
        // accumulating invalidation batch. Only meaningful while the batch is
        // non-empty; used to place CMDQ_CONS at the offending command if the
        // host rejects part of a flushed batch.
        let mut batch_start_cons = cons;

        // Held from the StreamID check that admits the first batch entry until
        // the host consumes the batch. Cloned out of `self` so the guard's
        // borrow does not conflict with the `&mut self` calls in the loop.
        let shared_state = self.shared_state.clone();
        let mut accel_devices: Option<crate::shared::AccelDevices<'_>> = None;

        // Limit iterations to prevent infinite loops on malformed state.
        let mut iterations = 0u32;

        while cons != prod {
            if iterations >= max_entries {
                // Safety valve: should never happen with well-behaved software.
                tracelimit::warn_ratelimited!("smmu: CMDQ processing exceeded max iterations");
                break;
            }
            iterations += 1;

            // Compute the entry address: index within the queue (without wrap bit).
            let index = cons & (max_entries - 1);
            let entry_addr = base_addr + (index as u64) * (size_of::<CmdEntry>() as u64);

            // Read the 16-byte command entry from guest memory.
            let entry = match self.guest_memory.read_plain::<CmdEntry>(entry_addr) {
                Ok(entry) => entry,
                Err(e) => {
                    tracelimit::warn_ratelimited!(
                        error = &e as &dyn std::error::Error,
                        entry_addr,
                        "smmu: failed to read CMDQ entry from guest memory"
                    );
                    // Forward any already-validated commands preceding the
                    // faulting entry before reporting the fetch abort. If the
                    // host rejects part of that earlier batch, stop at its
                    // first unhandled command instead; advancing to the later
                    // unreadable entry would lose an invalidation.
                    match self.flush_invalidation_batch(&mut accel_devices) {
                        Err(failed_index) => {
                            cons = (batch_start_cons + failed_index as u32) & index_mask;
                            self.set_cmdq_error(registers::CmdqError::CERROR_ILL);
                        }
                        Ok(()) => self.set_cmdq_error(registers::CmdqError::CERROR_ABT),
                    }
                    break;
                }
            };

            let opcode = entry.opcode();

            // Forwardable invalidation commands accumulate into a single
            // ordered batch forwarded to the host at the next flush boundary.
            // `ATC_INV` is only forwardable once ATS is enabled (`IDR0.ATS=1`);
            // while ATS is off it is illegal and falls through to the
            // illegal-opcode arm below.
            let forwardable = matches!(
                opcode,
                CmdOpcode::TLBI_NH_ALL
                    | CmdOpcode::TLBI_NH_ASID
                    | CmdOpcode::TLBI_NH_VA
                    | CmdOpcode::TLBI_NH_VAA
                    | CmdOpcode::TLBI_NSNH_ALL
                    | CmdOpcode::CFGI_CD
                    | CmdOpcode::CFGI_CD_ALL
            ) || (opcode == CmdOpcode::ATC_INV && self.idr0.ats());

            if forwardable {
                // SID-based invalidations (CFGI_CD/CFGI_CD_ALL, and ATC_INV
                // once ATS is on) target one stream and carry its StreamID at
                // qw0[63:32]. They only affect host state while the stream is
                // translating (S1_TRANS): they invalidate the context-
                // descriptor cache, and the device's ATS cache, which exist on
                // the host only for an attached nested domain. Skip (consume as
                // a no-op) any such command for a stream that is not currently
                // translating: there is nothing on the host to invalidate, no
                // vDevice is bound (it is allocated on the translate attach),
                // and forwarding would hit -EIO and surface as a spurious
                // CERROR_ILL. Because the guest writes the context descriptor
                // (issuing CFGI_CD) before installing the translating STE
                // (CFGI_STE), this also skips that first premature CFGI_CD.
                //
                // The registration lock taken here is held until the batch is
                // flushed, so the stream cannot stop translating between this
                // check and the host call that consumes the command.
                let sid_based = matches!(
                    opcode,
                    CmdOpcode::CFGI_CD | CmdOpcode::CFGI_CD_ALL | CmdOpcode::ATC_INV
                );
                let devices =
                    accel_devices.get_or_insert_with(|| shared_state.lock_accel_devices());
                if sid_based && !devices.is_translating(CmdCfgiCd::from(entry.qw0).sid()) {
                    // Flush the pending batch first so its CMDQ indices stay
                    // contiguous (the partial-failure → CMDQ_CONS mapping
                    // assumes no gaps), then consume this command as a no-op.
                    if let Err(failed_index) = self.flush_invalidation_batch(&mut accel_devices) {
                        cons = (batch_start_cons + failed_index as u32) & index_mask;
                        self.set_cmdq_error(registers::CmdqError::CERROR_ILL);
                        break;
                    }
                    cons = (cons + 1) & index_mask;
                    continue;
                }
                if self.invalidation_batch.is_empty() {
                    batch_start_cons = cons;
                }
                self.invalidation_batch.push([entry.qw0, entry.qw1]);
                cons = (cons + 1) & index_mask;
                continue;
            }

            // Non-forwardable command: flush the pending batch first so prior
            // invalidations complete before this barrier/config command, and
            // so `CMD_SYNC` keeps its "all prior commands complete" guarantee.
            // The host processes the array in order, so program order is
            // preserved. On partial failure, stop at the offending command.
            if let Err(failed_index) = self.flush_invalidation_batch(&mut accel_devices) {
                cons = (batch_start_cons + failed_index as u32) & index_mask;
                self.set_cmdq_error(registers::CmdqError::CERROR_ILL);
                break;
            }

            match opcode {
                // Configuration invalidation: CFGI_STE — re-drive the
                // backend (if any) to the stream's current policy.
                //
                // SMMUv3 §4.3.1 gives this command no content-derived failure,
                // so a backend that rejects the new STE is not a command error.
                // The stream is left aborting instead, which is exactly what an
                // ILLEGAL STE produces, and the guest's next CFGI_STE retries.
                CmdOpcode::CFGI_STE => {
                    let cmd = CmdCfgiSte::from(entry.qw0);
                    // Emulated devices: no-op (no STE cache). Accelerated
                    // streams: the emulator re-reads and re-applies the STE.
                    if let Err(e) = self.shared_state.apply_stream_config(cmd.sid()) {
                        tracelimit::warn_ratelimited!(
                            error = &*e as &dyn std::error::Error,
                            sid = cmd.sid(),
                            "smmu: stream left aborting after rejected CFGI_STE"
                        );
                    }
                }

                // CFGI_STE_RANGE: re-drive every backend in the SID range.
                CmdOpcode::CFGI_STE_RANGE => {
                    // §4.3.2: the bottom Range+1 bits of the StreamID are
                    // IGNORED, aligning the range to its size. Range=31
                    // (CFGI_ALL) spans the full 2^32 StreamIDs, so compute in
                    // u64 to keep the top of the range representable.
                    let cmd = CmdCfgiSteRange::from(entry.qw0);
                    let count = 1u64 << (CmdCfgiSteRange::range_from_entry(&entry) as u32 + 1);
                    let start = u64::from(cmd.sid()) & !(count - 1);
                    if let Err(e) = self
                        .shared_state
                        .apply_stream_configs_in_range(start..=start + count - 1)
                    {
                        tracelimit::warn_ratelimited!(
                            error = &*e as &dyn std::error::Error,
                            start,
                            count,
                            "smmu: stream left aborting after rejected CFGI_STE_RANGE"
                        );
                    }
                }

                // Prefetch hint — no-op (no caching to warm).
                CmdOpcode::PREFETCH_CFG => {}

                // TLBI_S12_VMALL is a stage-2 invalidation. This vSMMU
                // advertises IDR0.S2P=0 (stage-1 only), so per SMMUv3 §4.4.3.2
                // the command is illegal and must raise CERROR_ILL.
                CmdOpcode::TLBI_S12_VMALL => {
                    tracelimit::warn_ratelimited!(
                        "smmu: TLBI_S12_VMALL is illegal on a stage-1-only SMMU"
                    );
                    self.set_cmdq_error(registers::CmdqError::CERROR_ILL);
                    break;
                }

                // Synchronization command.
                CmdOpcode::CMD_SYNC => {
                    if !self.handle_cmd_sync(&entry) {
                        break;
                    }
                }

                // Unknown opcode — set CMDQ error.
                opcode => {
                    tracelimit::warn_ratelimited!(?opcode, "smmu: unknown CMDQ opcode");
                    self.set_cmdq_error(registers::CmdqError::CERROR_ILL);
                    break;
                }
            }

            // Advance cons with wrap.
            cons = (cons + 1) & index_mask;
        }

        // Flush any trailing forwardable commands (queue drained with no
        // following boundary command). Every error-break path above already
        // flushed the batch, so this only forwards a tail run on a clean drain.
        if let Err(failed_index) = self.flush_invalidation_batch(&mut accel_devices) {
            cons = (batch_start_cons + failed_index as u32) & index_mask;
            self.set_cmdq_error(registers::CmdqError::CERROR_ILL);
        }

        // Update the stored CMDQ_CONS (preserve error field, update rd).
        self.cmdq_cons.set_rd(cons);
    }

    /// Flushes the accumulated invalidation batch to the host sink as a single
    /// `IOMMU_HWPT_INVALIDATE`, releasing the registration lock afterwards.
    ///
    /// Returns `Ok(())` on full success, an empty batch, or an emulated-only
    /// SMMU (no host sink), with the scratch buffer cleared. On a host failure
    /// returns `Err(failed_index)` — the offset within the batch of the first
    /// command the host did not handle — so the caller stops draining at that
    /// command. The sink owns mapping the host's reported handled-count to this
    /// index (including its unreliable corners), so this is always a valid
    /// offset within the batch.
    fn flush_invalidation_batch(
        &mut self,
        accel_devices: &mut Option<crate::shared::AccelDevices<'_>>,
    ) -> Result<(), usize> {
        let result = match accel_devices.as_ref() {
            Some(devices) if !self.invalidation_batch.is_empty() => {
                SmmuSharedState::invalidate(devices, &self.invalidation_batch)
            }
            _ => Ok(()),
        };
        self.invalidation_batch.clear();
        *accel_devices = None;
        result
    }

    /// Handle a CMD_SYNC command.
    ///
    /// With IDR0.MSI=0, Linux uses CS=SIG_SEV and polls CMDQ_CONS for
    /// completion. The MSIWrite path is kept for spec compliance but won't
    /// be exercised by Linux when MSI is not advertised.
    /// Returns `true` on success, `false` if a CMDQ error was raised
    /// (caller must stop consuming).
    fn handle_cmd_sync(&mut self, entry: &CmdEntry) -> bool {
        let cmd = CmdSync::from(entry.qw0);
        let cs = SyncCs(cmd.cs());

        match cs {
            SyncCs::SIG_NONE | SyncCs::SIG_SEV => {
                // No signal or SEV — nothing to do. Linux polls CMDQ_CONS.
            }
            SyncCs::SIG_IRQ => {
                // Write MSI data to MSI address in guest memory (RAM polling).
                let msi_addr = CmdSync::msi_write_addr_from_entry(entry);
                let msi_data = cmd.msi_data();

                if msi_addr != 0 {
                    if let Err(e) = self
                        .guest_memory
                        .write_at(msi_addr, &msi_data.to_le_bytes())
                    {
                        tracelimit::warn_ratelimited!(
                            error = &e as &dyn std::error::Error,
                            msi_addr,
                            "smmu: failed to write CMD_SYNC MSI to guest memory"
                        );
                    }
                }
            }
            _ => {
                // CS=0b11 is reserved and causes CERROR_ILL per §4.7.3.
                self.set_cmdq_error(registers::CmdqError::CERROR_ILL);
                return false;
            }
        }
        true
    }

    /// Set a command queue error, toggling GERROR.CMDQ_ERR and storing the
    /// error code in CMDQ_CONS.
    fn set_cmdq_error(&mut self, error: registers::CmdqError) {
        // Set error code in CMDQ_CONS.
        self.cmdq_cons.set_err(error.0);
        // Toggle GERROR.CMDQ_ERR and update interrupt line (atomic).
        self.shared_state.toggle_cmdq_err();
    }

    // =========================================================================
    // Event Queue
    // =========================================================================
}

impl ChipsetDevice for SmmuDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

impl ChangeDeviceState for SmmuDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let SmmuDevice {
            // Static configuration — not reset.
            mmio_region: _,
            mmio_base: _,
            guest_memory: _,
            shared_state,

            // Identification registers — read-only, not reset.
            idr0: _,
            idr1: _,
            idr2: _,
            idr3: _,
            idr4: _,
            idr5: _,
            iidr: _,
            aidr: _,

            // Control registers — reset to power-on defaults.
            cr0,
            cr1,
            cr2,
            gbpa,

            // Interrupt control.
            irq_ctrl,
            irq_ctrlack,

            // Stream table base.
            strtab_base,
            strtab_base_cfg,

            // Command queue.
            cmdq_base,
            cmdq_prod,
            cmdq_cons,

            // Scratch batch buffer — transient; cleared on reset.
            invalidation_batch,

            // Event queue base register.
            evtq_base,

            // MSI configuration.
            gerror_msi,
            evtq_msi,
            cmdq_msi,
        } = self;

        let reset_cr0 = registers::Cr0::new();
        let reset_gbpa = registers::Gbpa::new().with_abort(false);
        let reset_strtab_base = 0;
        let reset_strtab_base_cfg = registers::StrtabBaseCfg::new();
        let TranslationPolicy { oas_mask, .. } = shared_state.translation_policy();
        let reset_policy = TranslationPolicy {
            enabled: reset_cr0.smmuen(),
            gbpa_abort: reset_gbpa.abort(),
            strtab_base: registers::StrtabBase::from(reset_strtab_base).addr(),
            strtab_log2size: reset_strtab_base_cfg.log2size(),
            oas_mask,
        };
        shared_state.transition_translation_policy(reset_policy, "SMMU reset");

        *cr0 = reset_cr0;
        *cr1 = registers::Cr1::new();
        *cr2 = registers::Cr2::new();
        // GBPA resets to ABORT=0 (disabled-state DMA bypasses), matching the
        // power-on default and preserving boot-time passthrough.
        *gbpa = reset_gbpa;

        *irq_ctrl = registers::IrqCtrl::new();
        *irq_ctrlack = registers::IrqCtrl::new();

        *strtab_base = reset_strtab_base;
        *strtab_base_cfg = reset_strtab_base_cfg;

        *cmdq_base = 0;
        *cmdq_prod = 0;
        *cmdq_cons = registers::CmdqCons::new();

        invalidation_batch.clear();

        *evtq_base = 0;

        *gerror_msi = MsiConfig::default();
        *evtq_msi = MsiConfig::default();
        *cmdq_msi = MsiConfig::default();

        // Reset EVTQ state (prod, cons, config, enabled).
        // Reset GERROR state and deassert interrupt.
        shared_state.reset_queue_state();
    }
}

impl SaveRestore for SmmuDevice {
    type SavedState = state::SavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        let &mut SmmuDevice {
            // Static configuration — not saved.
            mmio_region: _,
            mmio_base: _,
            guest_memory: _,
            ref shared_state,

            // Identification registers — read-only, not saved.
            idr0: _,
            idr1: _,
            idr2: _,
            idr3: _,
            idr4: _,
            idr5: _,
            iidr: _,
            aidr: _,

            // Control registers.
            cr0,
            cr1,
            cr2,
            gbpa,

            // Interrupt control.
            irq_ctrl,
            irq_ctrlack: _, // mirror of irq_ctrl (immediate ack)

            // Stream table base.
            strtab_base,
            strtab_base_cfg,

            // Command queue.
            cmdq_base,
            cmdq_prod,
            cmdq_cons,

            // Scratch batch buffer — transient, fully drained between CMDQ
            // processing passes, so there is nothing to save.
            invalidation_batch: _,

            // Event queue base register.
            evtq_base,

            // MSI configuration.
            ref gerror_msi,
            ref evtq_msi,
            ref cmdq_msi,
        } = self;

        let queue = shared_state.save_queue_state();

        Ok(state::SavedState {
            cr0: cr0.into(),
            cr1: cr1.into(),
            cr2: cr2.into(),
            gbpa: gbpa.into(),
            irq_ctrl: irq_ctrl.into(),
            strtab_base,
            strtab_base_cfg: strtab_base_cfg.into(),
            cmdq_base,
            cmdq_prod,
            cmdq_cons: cmdq_cons.into(),
            evtq_base,
            gerror_msi: state::SavedMsiConfig::save(gerror_msi),
            evtq_msi: state::SavedMsiConfig::save(evtq_msi),
            cmdq_msi: state::SavedMsiConfig::save(cmdq_msi),
            evtq_prod: queue.evtq_prod,
            evtq_cons: queue.evtq_cons,
            gerror: queue.gerror,
            gerrorn: queue.gerrorn,
        })
    }

    fn restore(&mut self, saved: Self::SavedState) -> Result<(), RestoreError> {
        let state::SavedState {
            cr0,
            cr1,
            cr2,
            gbpa,
            irq_ctrl,
            strtab_base,
            strtab_base_cfg,
            cmdq_base,
            cmdq_prod,
            cmdq_cons,
            evtq_base,
            gerror_msi,
            evtq_msi,
            cmdq_msi,
            evtq_prod,
            evtq_cons,
            gerror,
            gerrorn,
        } = saved;

        let restored_cr0 = Self::sanitize_cr0(cr0);
        let restored_cr1 = registers::Cr1::from(cr1);
        let restored_cr2 = registers::Cr2::from(cr2);
        let restored_gbpa = Self::sanitize_gbpa(gbpa);
        let restored_irq_ctrl = registers::IrqCtrl::from(irq_ctrl);
        let restored_strtab_base_cfg = registers::StrtabBaseCfg::from(strtab_base_cfg);

        let mut restored_policy = self.shared_state.translation_policy();
        restored_policy.enabled = restored_cr0.smmuen();
        restored_policy.gbpa_abort = restored_gbpa.abort();
        restored_policy.strtab_base = registers::StrtabBase::from(strtab_base).addr();
        restored_policy.strtab_log2size = restored_strtab_base_cfg.log2size();
        self.shared_state
            .transition_translation_policy(restored_policy, "SMMU restore");

        self.cr0 = restored_cr0;
        self.cr1 = restored_cr1;
        self.cr2 = restored_cr2;
        self.gbpa = restored_gbpa;
        self.irq_ctrl = restored_irq_ctrl;
        self.irq_ctrlack = restored_irq_ctrl;
        self.strtab_base = strtab_base;
        self.strtab_base_cfg = restored_strtab_base_cfg;
        self.cmdq_base = cmdq_base;
        self.cmdq_prod = cmdq_prod;
        self.cmdq_cons = registers::CmdqCons::from(cmdq_cons);
        self.evtq_base = evtq_base;
        self.gerror_msi = gerror_msi.restore();
        self.evtq_msi = evtq_msi.restore();
        self.cmdq_msi = cmdq_msi.restore();

        self.sync_evtq_to_shared();
        self.shared_state.set_evtq_enabled(self.cr0.eventqen());
        self.shared_state
            .set_irq_ctrl(self.irq_ctrl.eventq_irqen(), self.irq_ctrl.gerror_irqen());
        self.shared_state
            .restore_queue_state(crate::shared::SavedQueueState {
                evtq_prod,
                evtq_cons,
                gerror,
                gerrorn,
            });

        Ok(())
    }
}

mod state {
    use mesh::payload::Protobuf;
    use vmcore::save_restore::SavedStateRoot;

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "iommu.smmu")]
    pub struct SavedState {
        #[mesh(1)]
        pub(super) cr0: u32,
        #[mesh(2)]
        pub(super) cr1: u32,
        #[mesh(3)]
        pub(super) cr2: u32,
        #[mesh(4)]
        pub(super) gbpa: u32,
        #[mesh(5)]
        pub(super) irq_ctrl: u32,
        #[mesh(6)]
        pub(super) strtab_base: u64,
        #[mesh(7)]
        pub(super) strtab_base_cfg: u32,
        #[mesh(8)]
        pub(super) cmdq_base: u64,
        #[mesh(9)]
        pub(super) cmdq_prod: u32,
        #[mesh(10)]
        pub(super) cmdq_cons: u32,
        #[mesh(11)]
        pub(super) evtq_base: u64,
        #[mesh(12)]
        pub(super) gerror_msi: SavedMsiConfig,
        #[mesh(13)]
        pub(super) evtq_msi: SavedMsiConfig,
        #[mesh(14)]
        pub(super) cmdq_msi: SavedMsiConfig,
        #[mesh(15)]
        pub(super) evtq_prod: u32,
        #[mesh(16)]
        pub(super) evtq_cons: u32,
        #[mesh(17)]
        pub(super) gerror: u32,
        #[mesh(18)]
        pub(super) gerrorn: u32,
    }

    #[derive(Protobuf)]
    #[mesh(package = "iommu.smmu")]
    pub struct SavedMsiConfig {
        #[mesh(1)]
        pub addr: u64,
        #[mesh(2)]
        pub data: u32,
        #[mesh(3)]
        pub attr: u32,
    }

    impl SavedMsiConfig {
        pub(super) fn save(msi: &super::MsiConfig) -> Self {
            let super::MsiConfig { addr, data, attr } = *msi;
            Self { addr, data, attr }
        }

        pub(super) fn restore(self) -> super::MsiConfig {
            let Self { addr, data, attr } = self;
            super::MsiConfig { addr, data, attr }
        }
    }
}

impl MmioIntercept for SmmuDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        let offset = (addr - self.mmio_base) as u32;

        if offset >= 0x10000 {
            // Page 1 register access.
            match data.len() {
                4 => {
                    let value = self.read_page1_reg32(offset);
                    data.copy_from_slice(&value.to_le_bytes());
                }
                8 => {
                    let value = self.read_page1_reg64(offset);
                    data.copy_from_slice(&value.to_le_bytes());
                }
                _ => return IoResult::Err(IoError::InvalidAccessSize),
            }
        } else {
            // Page 0 register access.
            match data.len() {
                4 => {
                    let value = self.read_reg32(offset);
                    data.copy_from_slice(&value.to_le_bytes());
                }
                8 => {
                    let value = self.read_reg64(offset);
                    data.copy_from_slice(&value.to_le_bytes());
                }
                _ => return IoResult::Err(IoError::InvalidAccessSize),
            }
        }

        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        let offset = (addr - self.mmio_base) as u32;

        if offset >= 0x10000 {
            // Page 1 register access.
            match data.len() {
                4 => {
                    let value = u32::from_le_bytes(data.try_into().unwrap());
                    self.write_page1_reg32(offset, value);
                }
                8 => {
                    let value = u64::from_le_bytes(data.try_into().unwrap());
                    self.write_page1_reg64(offset, value);
                }
                _ => return IoResult::Err(IoError::InvalidAccessSize),
            }
        } else {
            // Page 0 register access.
            match data.len() {
                4 => {
                    let value = u32::from_le_bytes(data.try_into().unwrap());
                    self.write_reg32(offset, value);
                }
                8 => {
                    let value = u64::from_le_bytes(data.try_into().unwrap());
                    self.write_reg64(offset, value);
                }
                _ => return IoResult::Err(IoError::InvalidAccessSize),
            }
        }

        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        std::slice::from_ref(&self.mmio_region)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::events::EvtEntry;
    use crate::spec::registers::*;

    const TEST_MMIO_BASE: u64 = 0x0900_0000;

    /// A minimal non-accelerated `SmmuConfig` for tests: 16-bit StreamIDs and a
    /// fixed 40-bit OAS. Tests that need other values construct `SmmuConfig`
    /// directly.
    fn test_config() -> SmmuConfig {
        SmmuConfig {
            sidsize: 16,
            oas_policy: SmmuOasPolicy::Fixed(40),
            accel: false,
        }
    }

    fn make_test_device() -> SmmuDevice {
        let gm = GuestMemory::empty();
        SmmuDevice::new(TEST_MMIO_BASE, gm, &test_config(), None, None)
    }

    /// Helper to read a 32-bit register.
    fn read32(dev: &mut SmmuDevice, reg_offset: u16) -> u32 {
        let mut data = [0u8; 4];
        let result = dev.mmio_read(TEST_MMIO_BASE + reg_offset as u64, &mut data);
        assert!(matches!(result, IoResult::Ok));
        u32::from_le_bytes(data)
    }

    /// Helper to write a 32-bit register.
    fn write32(dev: &mut SmmuDevice, reg_offset: u16, value: u32) {
        let data = value.to_le_bytes();
        let result = dev.mmio_write(TEST_MMIO_BASE + reg_offset as u64, &data);
        assert!(matches!(result, IoResult::Ok));
    }

    /// Helper to read a 64-bit register.
    fn read64(dev: &mut SmmuDevice, reg_offset: u16) -> u64 {
        let mut data = [0u8; 8];
        let result = dev.mmio_read(TEST_MMIO_BASE + reg_offset as u64, &mut data);
        assert!(matches!(result, IoResult::Ok));
        u64::from_le_bytes(data)
    }

    /// Helper to write a 64-bit register.
    fn write64(dev: &mut SmmuDevice, reg_offset: u16, value: u64) {
        let data = value.to_le_bytes();
        let result = dev.mmio_write(TEST_MMIO_BASE + reg_offset as u64, &data);
        assert!(matches!(result, IoResult::Ok));
    }

    /// Helper to read a 32-bit page 1 register (offset >= 0x10000).
    fn read32_page1(dev: &mut SmmuDevice, abs_offset: u32) -> u32 {
        let mut data = [0u8; 4];
        let result = dev.mmio_read(TEST_MMIO_BASE + abs_offset as u64, &mut data);
        assert!(matches!(result, IoResult::Ok));
        u32::from_le_bytes(data)
    }

    /// Helper to write a 32-bit page 1 register.
    fn write32_page1(dev: &mut SmmuDevice, abs_offset: u32, value: u32) {
        let data = value.to_le_bytes();
        let result = dev.mmio_write(TEST_MMIO_BASE + abs_offset as u64, &data);
        assert!(matches!(result, IoResult::Ok));
    }

    /// Helper to read a 64-bit page 1 register.
    fn read64_page1(dev: &mut SmmuDevice, abs_offset: u32) -> u64 {
        let mut data = [0u8; 8];
        let result = dev.mmio_read(TEST_MMIO_BASE + abs_offset as u64, &mut data);
        assert!(matches!(result, IoResult::Ok));
        u64::from_le_bytes(data)
    }

    /// Helper to write a 64-bit page 1 register.
    fn write64_page1(dev: &mut SmmuDevice, abs_offset: u32, value: u64) {
        let data = value.to_le_bytes();
        let result = dev.mmio_write(TEST_MMIO_BASE + abs_offset as u64, &data);
        assert!(matches!(result, IoResult::Ok));
    }

    #[test]
    fn test_idr_readback() {
        let mut dev = make_test_device();

        // IDR0: S1P=1, TTF=0b10, COHACC=1, ASID16=1, MSI=1, TTENDIAN=0b10,
        //       ST_LVL=0b00
        let idr0 = Idr0::from(read32(&mut dev, IDR0));
        assert!(idr0.s1p());
        assert!(!idr0.s2p());
        assert_eq!(idr0.ttf(), 0b10);
        assert!(idr0.cohacc());
        assert!(idr0.asid16());
        assert!(!idr0.msi());
        assert_eq!(idr0.ttendian(), 0b10);
        assert_eq!(idr0.st_level(), 0b00);

        // IDR1: SIDSIZE=16, CMDQS=8, EVTQS=8, ATTR_TYPES_OVR=0
        let idr1 = Idr1::from(read32(&mut dev, IDR1));
        assert_eq!(idr1.sidsize(), 16);
        assert_eq!(idr1.cmdqs(), 8);
        assert_eq!(idr1.eventqs(), 8);
        assert!(!idr1.attr_types_ovr());
        assert!(!idr1.tables_preset());
        assert!(!idr1.queues_preset());
        assert!(!idr1.rel());

        // IDR2, IDR3, IDR4 = 0
        assert_eq!(read32(&mut dev, IDR2), 0);
        assert_eq!(read32(&mut dev, IDR3), 0);
        assert_eq!(read32(&mut dev, IDR4), 0);

        // IDR5: GRAN4K=1, OAS=0b010 (40-bit)
        let idr5 = Idr5::from(read32(&mut dev, IDR5));
        assert!(idr5.gran4k());
        assert!(!idr5.gran16k());
        assert!(!idr5.gran64k());
        assert_eq!(idr5.oas(), 0b010);

        // IIDR = 0
        assert_eq!(read32(&mut dev, IIDR), 0);

        // AIDR = 0x03 (SMMUv3.3)
        assert_eq!(read32(&mut dev, AIDR), 0x03);
    }

    #[test]
    fn test_cr0_ack_echo() {
        let mut dev = make_test_device();

        // Write CR0 with all enable bits.
        let cr0_val = Cr0::new()
            .with_smmuen(true)
            .with_cmdqen(true)
            .with_eventqen(true);
        write32(&mut dev, CR0, cr0_val.into());

        // CR0ACK should match.
        let ack = read32(&mut dev, CR0ACK);
        assert_eq!(ack, u32::from(cr0_val));
    }

    #[test]
    fn test_cr0_sanitizes_unsupported_fields() {
        let mut dev = make_test_device();
        write32(&mut dev, CR0, u32::MAX);

        let expected = Cr0::new()
            .with_smmuen(true)
            .with_cmdqen(true)
            .with_eventqen(true);
        assert_eq!(read32(&mut dev, CR0), u32::from(expected));
        assert_eq!(read32(&mut dev, CR0ACK), u32::from(expected));
    }

    #[test]
    fn test_cr0_enable_sequence() {
        let mut dev = make_test_device();

        // Step 1: Enable CMDQ.
        let cr0_cmdq = Cr0::new().with_cmdqen(true);
        write32(&mut dev, CR0, cr0_cmdq.into());
        let ack = Cr0::from(read32(&mut dev, CR0ACK));
        assert!(ack.cmdqen());
        assert!(!ack.eventqen());
        assert!(!ack.smmuen());

        // Step 2: Enable EVTQ.
        let cr0_evtq = cr0_cmdq.with_eventqen(true);
        write32(&mut dev, CR0, cr0_evtq.into());
        let ack = Cr0::from(read32(&mut dev, CR0ACK));
        assert!(ack.cmdqen());
        assert!(ack.eventqen());
        assert!(!ack.smmuen());

        // Step 3: Enable SMMU.
        let cr0_full = cr0_evtq.with_smmuen(true);
        write32(&mut dev, CR0, cr0_full.into());
        let ack = Cr0::from(read32(&mut dev, CR0ACK));
        assert!(ack.cmdqen());
        assert!(ack.eventqen());
        assert!(ack.smmuen());
    }

    #[test]
    fn test_strtab_base_readback() {
        let mut dev = make_test_device();

        // Write a 64-bit STRTAB_BASE with address and RA hint.
        let base = StrtabBase::new()
            .with_addr_bits(0x1234_5678_9AB0u64 >> 6)
            .with_ra(true);
        write64(&mut dev, STRTAB_BASE, base.into());

        let readback = StrtabBase::from(read64(&mut dev, STRTAB_BASE));
        assert_eq!(readback.addr(), base.addr());
        assert!(readback.ra());

        // Write STRTAB_BASE_CFG.
        let cfg = StrtabBaseCfg::new().with_log2size(10).with_fmt(0);
        write32(&mut dev, STRTAB_BASE_CFG, cfg.into());
        let readback_cfg = StrtabBaseCfg::from(read32(&mut dev, STRTAB_BASE_CFG));
        assert_eq!(readback_cfg.log2size(), 10);
        assert_eq!(readback_cfg.fmt(), 0);
    }

    #[test]
    fn test_irq_ctrl_ack() {
        let mut dev = make_test_device();

        let ctrl = IrqCtrl::new()
            .with_eventq_irqen(true)
            .with_gerror_irqen(true);
        write32(&mut dev, IRQ_CTRL, ctrl.into());

        let ack = IrqCtrl::from(read32(&mut dev, IRQ_CTRLACK));
        assert!(ack.eventq_irqen());
        assert!(ack.gerror_irqen());
    }

    #[test]
    fn test_gbpa_update_bit() {
        let mut dev = make_test_device();

        // Write GBPA with UPDATE=1 and ABORT=0.
        let gbpa = Gbpa::new().with_update(true).with_abort(false);
        write32(&mut dev, GBPA, gbpa.into());

        // Read back: UPDATE should be cleared, ABORT should be 0.
        let readback = Gbpa::from(read32(&mut dev, GBPA));
        assert!(!readback.update());
        assert!(!readback.abort());
    }

    #[test]
    fn test_gbpa_sanitizes_reserved_and_unsupported_fields() {
        let mut dev = make_test_device();
        write32(&mut dev, GBPA, u32::MAX);

        assert_eq!(
            read32(&mut dev, GBPA),
            u32::from(Gbpa::new().with_abort(true))
        );
    }

    #[test]
    fn test_gbpa_without_update_is_ignored() {
        let mut dev = make_test_device();
        write32(&mut dev, GBPA, Gbpa::new().with_abort(true).into());
        assert_eq!(read32(&mut dev, GBPA), u32::from(Gbpa::new()));
    }

    #[test]
    fn test_page1_register_access() {
        let mut dev = make_test_device();

        // EVTQ_CONS on page 1 is guest-writable.
        write32_page1(&mut dev, EVENTQ_CONS_PAGE1, 42);
        assert_eq!(read32_page1(&mut dev, EVENTQ_CONS_PAGE1), 42);

        // EVTQ_PROD on page 1 is SMMU-writable only (guest writes ignored).
        write32_page1(&mut dev, EVENTQ_PROD_PAGE1, 99);
        assert_eq!(read32_page1(&mut dev, EVENTQ_PROD_PAGE1), 0);
    }

    #[test]
    fn test_readonly_regs_ignore_writes() {
        let mut dev = make_test_device();

        let original_idr0 = read32(&mut dev, IDR0);
        write32(&mut dev, IDR0, 0xDEAD_BEEF);
        assert_eq!(read32(&mut dev, IDR0), original_idr0);

        let original_aidr = read32(&mut dev, AIDR);
        write32(&mut dev, AIDR, 0xCAFE);
        assert_eq!(read32(&mut dev, AIDR), original_aidr);

        // CR0ACK is read-only.
        write32(&mut dev, CR0ACK, 0xFFFF_FFFF);
        assert_eq!(read32(&mut dev, CR0ACK), 0);

        // IRQ_CTRLACK is read-only.
        write32(&mut dev, IRQ_CTRLACK, 0xFFFF_FFFF);
        assert_eq!(read32(&mut dev, IRQ_CTRLACK), 0);
    }

    #[test]
    fn test_cmdq_base_readback() {
        let mut dev = make_test_device();

        let base = QueueBase::new()
            .with_log2size(8)
            .with_addr_bits(0x8000_0000u64 >> 5);
        write64(&mut dev, CMDQ_BASE, base.into());
        let readback = QueueBase::from(read64(&mut dev, CMDQ_BASE));
        assert_eq!(readback.log2size(), 8);
        assert_eq!(readback.addr(), base.addr());
    }

    #[test]
    fn test_evtq_base_readback() {
        let mut dev = make_test_device();

        let base = QueueBase::new()
            .with_log2size(8)
            .with_addr_bits(0xA000_0000u64 >> 5);
        write64(&mut dev, EVENTQ_BASE, base.into());
        let readback = QueueBase::from(read64(&mut dev, EVENTQ_BASE));
        assert_eq!(readback.log2size(), 8);
        assert_eq!(readback.addr(), base.addr());
    }

    #[test]
    fn test_gerror_gerrorn_toggle() {
        let mut dev = make_test_device();

        // Initially GERROR = GERRORN = 0 (no active errors).
        assert_eq!(read32(&mut dev, GERROR), 0);
        assert_eq!(read32(&mut dev, GERRORN), 0);

        // Toggle CMDQ_ERR via shared state (as the emulator would).
        dev.shared_state.toggle_cmdq_err();
        let gerror = Gerror::from(read32(&mut dev, GERROR));
        assert!(gerror.cmdq_err());

        // Guest acknowledges by writing GERRORN to match GERROR.
        write32(&mut dev, GERRORN, gerror.into());
        let gerrorn = Gerror::from(read32(&mut dev, GERRORN));
        assert!(gerrorn.cmdq_err());
    }

    #[test]
    fn test_msi_config_registers() {
        let mut dev = make_test_device();

        // GERROR MSI config (page 0).
        write64(&mut dev, GERROR_IRQ_CFG0, 0xFEDC_BA98_7654_3210);
        assert_eq!(read64(&mut dev, GERROR_IRQ_CFG0), 0xFEDC_BA98_7654_3210);
        write32(&mut dev, GERROR_IRQ_CFG1, 0xAABB_CCDD);
        assert_eq!(read32(&mut dev, GERROR_IRQ_CFG1), 0xAABB_CCDD);
        write32(&mut dev, GERROR_IRQ_CFG2, 0x0000_000F);
        assert_eq!(read32(&mut dev, GERROR_IRQ_CFG2), 0x0000_000F);

        // EVENTQ MSI config (page 0).
        write64(&mut dev, EVENTQ_IRQ_CFG0, 0x1111_2222_3333_4444);
        assert_eq!(read64(&mut dev, EVENTQ_IRQ_CFG0), 0x1111_2222_3333_4444);
        write32(&mut dev, EVENTQ_IRQ_CFG1, 0x5555_6666);
        assert_eq!(read32(&mut dev, EVENTQ_IRQ_CFG1), 0x5555_6666);
        write32(&mut dev, EVENTQ_IRQ_CFG2, 0x0000_0003);
        assert_eq!(read32(&mut dev, EVENTQ_IRQ_CFG2), 0x0000_0003);

        // CMDQ MSI config (page 1).
        write64_page1(&mut dev, CMDQ_IRQ_CFG0_PAGE1, 0xAAAA_BBBB_CCCC_DDDD);
        assert_eq!(
            read64_page1(&mut dev, CMDQ_IRQ_CFG0_PAGE1),
            0xAAAA_BBBB_CCCC_DDDD
        );
        write32_page1(&mut dev, CMDQ_IRQ_CFG1_PAGE1, 0x1234_5678);
        assert_eq!(read32_page1(&mut dev, CMDQ_IRQ_CFG1_PAGE1), 0x1234_5678);
        write32_page1(&mut dev, CMDQ_IRQ_CFG2_PAGE1, 0x0000_0007);
        assert_eq!(read32_page1(&mut dev, CMDQ_IRQ_CFG2_PAGE1), 0x0000_0007);
    }

    #[test]
    fn test_invalid_access_size() {
        let mut dev = make_test_device();

        // 1-byte read should fail.
        let mut data = [0u8; 1];
        let result = dev.mmio_read(TEST_MMIO_BASE, &mut data);
        assert!(matches!(result, IoResult::Err(IoError::InvalidAccessSize)));

        // 1-byte write should fail.
        let result = dev.mmio_write(TEST_MMIO_BASE, &[0u8]);
        assert!(matches!(result, IoResult::Err(IoError::InvalidAccessSize)));

        // 3-byte read should fail.
        let mut data = [0u8; 3];
        let result = dev.mmio_read(TEST_MMIO_BASE, &mut data);
        assert!(matches!(result, IoResult::Err(IoError::InvalidAccessSize)));
    }

    #[test]
    fn test_cr1_cr2_readback() {
        let mut dev = make_test_device();

        let cr1 = Cr1::new()
            .with_queue_ic(0b01)
            .with_queue_oc(0b01)
            .with_queue_sh(0b11)
            .with_table_ic(0b01)
            .with_table_oc(0b01)
            .with_table_sh(0b11);
        write32(&mut dev, CR1, cr1.into());
        let readback = Cr1::from(read32(&mut dev, CR1));
        assert_eq!(readback.queue_ic(), 0b01);
        assert_eq!(readback.table_sh(), 0b11);

        let cr2 = Cr2::new().with_recinvsid(true);
        write32(&mut dev, CR2, cr2.into());
        let readback = Cr2::from(read32(&mut dev, CR2));
        assert!(readback.recinvsid());
    }

    #[test]
    fn test_cmdq_prod_readback() {
        let mut dev = make_test_device();

        write32(&mut dev, CMDQ_PROD, 0x0000_0005);
        assert_eq!(read32(&mut dev, CMDQ_PROD), 0x0000_0005);
    }

    // =========================================================================
    // CMDQ processing tests
    // =========================================================================

    /// Size of the test CMDQ: 2^3 = 8 entries.
    const TEST_CMDQ_LOG2SIZE: u8 = 3;
    /// GPA where the test CMDQ lives.
    const TEST_CMDQ_GPA: u64 = 0x1_0000;
    /// GPA where CMD_SYNC MSI writes go.
    const TEST_MSI_GPA: u64 = 0x2_0000;

    fn transition_to_enabled(state: &SmmuSharedState) {
        let mut policy = state.translation_policy();
        policy.enabled = true;
        state.transition_translation_policy(policy, "test enable transition");
    }

    /// Every StreamID, as `CMD_CFGI_ALL` (Range=31) covers.
    const ALL_SIDS: RangeInclusive<u64> = 0..=u32::MAX as u64;

    /// Whether `sid` is attached in translating mode, and so has its SID-based
    /// invalidations forwarded.
    fn is_translating(dev: &SmmuDevice, sid: u32) -> bool {
        dev.shared_state.lock_accel_devices().is_translating(sid)
    }

    /// Create a device with real guest memory and a configured CMDQ.
    fn make_cmdq_test_device() -> SmmuDevice {
        // Allocate enough guest memory for CMDQ + MSI target page.
        let gm = GuestMemory::allocate(0x4_0000);
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm, &test_config(), None, None);

        // Program CMDQ_BASE: address + log2size.
        let cmdq_base = QueueBase::new()
            .with_log2size(TEST_CMDQ_LOG2SIZE)
            .with_addr_bits(TEST_CMDQ_GPA >> 5);
        write64(&mut dev, CMDQ_BASE, cmdq_base.into());

        // Enable CMDQEN.
        let cr0 = Cr0::new().with_cmdqen(true);
        write32(&mut dev, CR0, cr0.into());

        dev
    }

    /// Write a command entry to the CMDQ at the given index.
    fn write_cmdq_entry(dev: &SmmuDevice, index: u32, entry: &CmdEntry) {
        let addr = TEST_CMDQ_GPA + (index as u64) * (size_of::<CmdEntry>() as u64);
        dev.guest_memory
            .write_plain(addr, entry)
            .expect("write cmd entry");
    }

    #[test]
    fn test_cmdq_basic_consumption() {
        let mut dev = make_cmdq_test_device();

        // Write 3 commands: CFGI_STE_RANGE (CFGI_ALL), TLBI_NSNH_ALL, CMD_SYNC(SEV).
        write_cmdq_entry(
            &dev,
            0,
            &CmdEntry {
                qw0: CmdOpcode::CFGI_STE_RANGE.0 as u64,
                qw1: 31, // Range=31 = ALL
            },
        );
        write_cmdq_entry(
            &dev,
            1,
            &CmdEntry {
                qw0: CmdOpcode::TLBI_NSNH_ALL.0 as u64,
                qw1: 0,
            },
        );
        let sync = CmdSync::new()
            .with_opcode(CmdOpcode::CMD_SYNC.0)
            .with_cs(SyncCs::SIG_SEV.0);
        write_cmdq_entry(
            &dev,
            2,
            &CmdEntry {
                qw0: sync.into(),
                qw1: 0,
            },
        );

        // Set PROD=3, triggering processing.
        write32(&mut dev, CMDQ_PROD, 3);

        // Verify CONS=3.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 3);
        assert_eq!(cons.err(), 0);
    }

    #[test]
    fn test_cmdq_sync_msi_write() {
        let mut dev = make_cmdq_test_device();

        let msi_data: u32 = 0xDEAD_BEEF;
        let msi_addr: u64 = TEST_MSI_GPA;

        // Build CMD_SYNC with CS=SIG_IRQ and MSI address/data.
        let sync = CmdSync::new()
            .with_opcode(CmdOpcode::CMD_SYNC.0)
            .with_cs(SyncCs::SIG_IRQ.0)
            .with_msi_data(msi_data);
        // MSI address goes in qw1 bits [119:66] → addr[55:2] at bits [53:0]
        // shifted left by 2 in qw1.
        let qw1 = (msi_addr >> 2) << 2;
        write_cmdq_entry(
            &dev,
            0,
            &CmdEntry {
                qw0: sync.into(),
                qw1,
            },
        );

        // Set PROD=1.
        write32(&mut dev, CMDQ_PROD, 1);

        // Verify CONS=1.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 1);

        // Verify MSI data written to the target GPA.
        let written: u32 = dev
            .guest_memory
            .read_plain(msi_addr)
            .expect("read MSI data");
        assert_eq!(written, msi_data);
    }

    #[test]
    fn test_cmdq_wrap() {
        let mut dev = make_cmdq_test_device();

        let max_entries = 1u32 << TEST_CMDQ_LOG2SIZE; // 8

        // Fill the queue completely: 8 CFGI_STE_RANGE commands.
        for i in 0..max_entries {
            write_cmdq_entry(
                &dev,
                i,
                &CmdEntry {
                    qw0: CmdOpcode::CFGI_STE_RANGE.0 as u64,
                    qw1: 31,
                },
            );
        }

        // Set PROD = 8 (which with wrap bit means index 0 with wrap=1).
        write32(&mut dev, CMDQ_PROD, max_entries);

        // CONS should advance to 8 (matching PROD with wrap).
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), max_entries);
        assert_eq!(cons.err(), 0);

        // Now write one more command at index 0 (wrapping around).
        write_cmdq_entry(
            &dev,
            0,
            &CmdEntry {
                qw0: CmdOpcode::TLBI_NH_ALL.0 as u64,
                qw1: 0,
            },
        );

        // PROD = 9 (wrap bit set, index 1).
        write32(&mut dev, CMDQ_PROD, max_entries + 1);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), max_entries + 1);
    }

    #[test]
    fn test_cmdq_unknown_opcode() {
        let mut dev = make_cmdq_test_device();

        // Write a command with unknown opcode 0xFF.
        write_cmdq_entry(&dev, 0, &CmdEntry { qw0: 0xFF, qw1: 0 });

        write32(&mut dev, CMDQ_PROD, 1);

        // CONS should have CERROR_ILL in the error field.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.err(), CmdqError::CERROR_ILL.0);

        // GERROR.CMDQ_ERR should be toggled (was 0, now 1).
        let gerror = Gerror::from(read32(&mut dev, GERROR));
        assert!(gerror.cmdq_err());
    }

    #[test]
    fn test_cmdq_log2size_clamped_to_idr1() {
        let gm = GuestMemory::allocate(0x4_0000);
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm, &test_config(), None, None);

        // IDR1.CMDQS = 8, IDR1.EVENTQS = 8. Program a larger value (20).
        let cmdq_base = QueueBase::new()
            .with_log2size(20)
            .with_addr_bits(TEST_CMDQ_GPA >> 5);
        write64(&mut dev, CMDQ_BASE, cmdq_base.into());

        // The effective log2size should be clamped to 8.
        assert_eq!(dev.cmdq_log2size(), 8);

        // A value within the limit should pass through unchanged.
        let cmdq_base = QueueBase::new()
            .with_log2size(5)
            .with_addr_bits(TEST_CMDQ_GPA >> 5);
        write64(&mut dev, CMDQ_BASE, cmdq_base.into());
        assert_eq!(dev.cmdq_log2size(), 5);
    }

    #[test]
    fn test_cmdq_linux_reset_sequence() {
        let mut dev = make_cmdq_test_device();

        // Linux reset sequence: CFGI_ALL + CMD_SYNC, TLBI_NSNH_ALL + CMD_SYNC.
        // Step 1: CFGI_ALL (CFGI_STE_RANGE with Range=31) + CMD_SYNC(SEV).
        write_cmdq_entry(
            &dev,
            0,
            &CmdEntry {
                qw0: CmdOpcode::CFGI_STE_RANGE.0 as u64,
                qw1: 31,
            },
        );
        let sync = CmdSync::new()
            .with_opcode(CmdOpcode::CMD_SYNC.0)
            .with_cs(SyncCs::SIG_SEV.0);
        write_cmdq_entry(
            &dev,
            1,
            &CmdEntry {
                qw0: sync.into(),
                qw1: 0,
            },
        );
        write32(&mut dev, CMDQ_PROD, 2);
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 2);
        assert_eq!(cons.err(), 0);

        // Step 2: TLBI_NSNH_ALL + CMD_SYNC(SEV).
        write_cmdq_entry(
            &dev,
            2,
            &CmdEntry {
                qw0: CmdOpcode::TLBI_NSNH_ALL.0 as u64,
                qw1: 0,
            },
        );
        write_cmdq_entry(
            &dev,
            3,
            &CmdEntry {
                qw0: sync.into(),
                qw1: 0,
            },
        );
        write32(&mut dev, CMDQ_PROD, 4);
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 4);
        assert_eq!(cons.err(), 0);

        // No errors should be set.
        let gerror = Gerror::from(read32(&mut dev, GERROR));
        assert!(!gerror.cmdq_err());
    }

    #[test]
    fn test_cmdq_error_stops_processing() {
        let mut dev = make_cmdq_test_device();

        // Write: unknown opcode, then a valid command.
        write_cmdq_entry(
            &dev,
            0,
            &CmdEntry {
                qw0: 0xFF, // Unknown
                qw1: 0,
            },
        );
        write_cmdq_entry(
            &dev,
            1,
            &CmdEntry {
                qw0: CmdOpcode::TLBI_NH_ALL.0 as u64,
                qw1: 0,
            },
        );

        write32(&mut dev, CMDQ_PROD, 2);

        // CONS should be at 0 — processing stopped at the unknown command.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 0);
        assert_eq!(cons.err(), CmdqError::CERROR_ILL.0);

        // Even if we write more PROD, processing should not resume (error active).
        write32(&mut dev, CMDQ_PROD, 2);
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 0);

        // Acknowledge the error by writing GERRORN to match GERROR.
        let gerror = read32(&mut dev, GERROR);
        write32(&mut dev, GERRORN, gerror);

        // Clear the error in CMDQ_CONS by resetting it internally.
        // In practice, the guest would reprogram CMDQ_BASE and re-enable,
        // but for this test we just verify the error flag blocks processing.
    }

    #[test]
    fn test_cmdq_disabled() {
        // Create device but do NOT enable CMDQEN.
        let gm = GuestMemory::allocate(0x4_0000);
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm, &test_config(), None, None);

        let cmdq_base = QueueBase::new()
            .with_log2size(TEST_CMDQ_LOG2SIZE)
            .with_addr_bits(TEST_CMDQ_GPA >> 5);
        write64(&mut dev, CMDQ_BASE, cmdq_base.into());

        // Write a command and set PROD without enabling CMDQEN.
        write_cmdq_entry(
            &dev,
            0,
            &CmdEntry {
                qw0: CmdOpcode::TLBI_NH_ALL.0 as u64,
                qw1: 0,
            },
        );
        write32(&mut dev, CMDQ_PROD, 1);

        // CONS should stay at 0 — CMDQ is disabled.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 0);

        // Enabling CMDQ consumes commands that were published while disabled.
        write32(&mut dev, CR0, Cr0::new().with_cmdqen(true).into());
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 1);
    }

    // =========================================================================
    // Accelerated invalidation batching tests
    // =========================================================================

    /// A mock invalidation sink that records each flushed batch and can be
    /// configured to fail after accepting a fixed number of entries (to
    /// exercise the partial-failure path).
    struct MockViommu {
        /// Each flushed batch, recorded in order as a list of `[qw0, qw1]`
        /// command pairs.
        batches: parking_lot::Mutex<Vec<Vec<[u64; 2]>>>,
        /// If `Some(n)`, every `invalidate` accepts at most `n` entries,
        /// returning a short count to simulate a host rejection.
        accept_limit: parking_lot::Mutex<Option<usize>>,
    }

    impl MockViommu {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                batches: parking_lot::Mutex::new(Vec::new()),
                accept_limit: parking_lot::Mutex::new(None),
            })
        }

        fn set_accept_limit(&self, limit: usize) {
            *self.accept_limit.lock() = Some(limit);
        }

        fn batches(&self) -> Vec<Vec<[u64; 2]>> {
            self.batches.lock().clone()
        }
    }

    impl crate::shared::Invalidate for MockViommu {
        fn invalidate(&self, entries: &[[u64; 2]]) -> Result<(), usize> {
            self.batches.lock().push(entries.to_vec());
            match *self.accept_limit.lock() {
                // Simulate the host rejecting the entry at index `limit`.
                Some(limit) if limit < entries.len() => Err(limit),
                _ => Ok(()),
            }
        }
    }

    /// A mock per-stream accel backend. Whether SID-based invalidations are
    /// forwarded is decided by the emulator from the stream's config, not by
    /// the backend.
    struct MockStreamBackend;

    impl crate::shared::AcceleratedStreamBackend for MockStreamBackend {
        fn set_stream_config(&self, _config: crate::shared::StreamConfig) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Stream table base/size used to set up a translating stream in tests.
    const TEST_STRTAB_GPA: u64 = 0x3_0000;
    const TEST_STRTAB_LOG2SIZE: u8 = 10; // 1024 entries, covers SIDs 0x100 and 0x200

    /// Register a per-stream accel backend for a device on `secondary_bus`
    /// (stream_id_base 0), returning the resulting StreamID. The SMMU is left
    /// disabled, so the stream's policy is bypass — SID-based invalidations
    /// for it are not forwarded.
    fn register_test_stream(dev: &SmmuDevice, secondary_bus: u8) -> u32 {
        let registration = dev
            .shared_state
            .register_accel_device(u32::from(secondary_bus) << 8, Arc::new(MockStreamBackend))
            .expect("register stream");
        // Test streams stay registered for the device's lifetime.
        std::mem::forget(registration);
        (secondary_bus as u32) << 8
    }

    /// Like [`register_test_stream`], but drives the stream to `S1_TRANS` (sets
    /// up the stream table + a translating STE and enables the SMMU) so
    /// SID-based invalidations for it are forwarded.
    fn register_translating_stream(dev: &SmmuDevice, secondary_bus: u8) -> u32 {
        use crate::spec::ste::STE_SIZE;
        use crate::spec::ste::Ste;
        use crate::spec::ste::SteConfig;
        use crate::spec::ste::SteDw0;
        use crate::spec::ste::SteDw1;

        let sid = (secondary_bus as u32) << 8;
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);
        let ste = Ste {
            qw0: SteDw0::new()
                .with_v(true)
                .with_config(SteConfig::S1_TRANS.0),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        };
        let ste_addr = TEST_STRTAB_GPA + sid as u64 * STE_SIZE as u64;
        dev.guest_memory
            .write_plain(ste_addr, &ste)
            .expect("write STE");
        register_test_stream(dev, secondary_bus)
    }

    // =========================================================================
    // Accel registration initial-policy tests
    //
    // Registration applies initial policy synchronously through shared state,
    // including while the chipset emulator is stopped.
    // =========================================================================

    /// A recording accel backend that captures each config applied to it.
    struct RecordingBackend {
        configs: parking_lot::Mutex<Vec<crate::shared::StreamConfig>>,
        fail_next: std::sync::atomic::AtomicBool,
    }

    impl RecordingBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                configs: parking_lot::Mutex::new(Vec::new()),
                fail_next: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn take(&self) -> Vec<crate::shared::StreamConfig> {
            std::mem::take(&mut *self.configs.lock())
        }

        fn fail_next(&self) {
            self.fail_next
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl crate::shared::AcceleratedStreamBackend for RecordingBackend {
        fn set_stream_config(&self, config: crate::shared::StreamConfig) -> anyhow::Result<()> {
            self.configs.lock().push(config);
            if self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                anyhow::bail!("injected stream config failure");
            }
            Ok(())
        }
    }

    /// A minimal accelerated device for registration tests.
    fn make_accel_device() -> SmmuDevice {
        let gm = GuestMemory::allocate(0x4_0000);
        let config = SmmuConfig {
            sidsize: 16,
            oas_policy: SmmuOasPolicy::Fixed(40),
            accel: true,
        };
        SmmuDevice::new(TEST_MMIO_BASE, gm, &config, None, None)
    }

    /// Host caps compatible with everything the emulator advertises.
    fn test_host_caps() -> HostSmmuCaps {
        HostSmmuCaps {
            oas: crate::spec::cd::Ips::IPS_48,
            ttf: Idr0Ttf::new().with_aarch64(true),
            ttendian: Idr0TtEndian::LE,
            gran4k: true,
        }
    }

    /// Write a valid STE with the given `Config` at `sid` in the test stream
    /// table.
    fn write_test_ste(dev: &SmmuDevice, sid: u32, config: crate::spec::ste::SteConfig) {
        use crate::spec::ste::STE_SIZE;
        use crate::spec::ste::Ste;
        use crate::spec::ste::SteDw0;
        use crate::spec::ste::SteDw1;
        let ste = Ste {
            qw0: SteDw0::new().with_v(true).with_config(config.0),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        };
        let addr = TEST_STRTAB_GPA + sid as u64 * STE_SIZE as u64;
        dev.guest_memory.write_plain(addr, &ste).expect("write STE");
    }

    /// Registering while the SMMU is disabled applies the disabled-state
    /// policy, which with `GBPA.ABORT=0` is bypass.
    #[test]
    fn test_register_applies_bypass_when_disabled() {
        let dev = make_accel_device();
        let backend = RecordingBackend::new();
        let _registration = dev
            .shared_state
            .register_accel_device(0x100, backend.clone())
            .expect("register backend");
        assert_eq!(backend.take(), vec![crate::shared::StreamConfig::Bypass]);
    }

    /// Registering while disabled with GBPA.ABORT=1 leaves the device aborting.
    #[test]
    fn test_register_applies_abort_when_disabled_gbpa_abort() {
        let dev = make_accel_device();
        dev.shared_state.set_gbpa_abort(true);
        let backend = RecordingBackend::new();
        let _registration = dev
            .shared_state
            .register_accel_device(0x100, backend.clone())
            .expect("register backend");
        assert_eq!(backend.take(), vec![crate::shared::StreamConfig::Abort]);
    }

    /// Registering while enabled applies that stream's STE policy.
    #[test]
    fn test_register_applies_ste_policy_when_enabled() {
        use crate::spec::ste::SteConfig;
        let dev = make_accel_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);

        let sid = 0x100u32;
        write_test_ste(&dev, sid, SteConfig::BYPASS);

        let backend = RecordingBackend::new();
        let _registration = dev
            .shared_state
            .register_accel_device(sid, backend.clone())
            .expect("register backend");
        assert_eq!(backend.take(), vec![crate::shared::StreamConfig::Bypass]);
    }

    /// A translating STE is applied at registration, without the backend ever
    /// being told its StreamID: it was registered against one.
    #[test]
    fn test_register_applies_translate_policy() {
        use crate::spec::ste::SteConfig;
        let dev = make_accel_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);

        let sid = 0x100u32;
        write_test_ste(&dev, sid, SteConfig::S1_TRANS);

        let backend = RecordingBackend::new();
        let _registration = dev
            .shared_state
            .register_accel_device(sid, backend.clone())
            .expect("register backend");
        let applied = backend.take();
        assert_eq!(applied.len(), 1);
        assert!(
            matches!(applied[0], crate::shared::StreamConfig::Translate { .. }),
            "expected Translate, got {:?}",
            applied[0]
        );
        assert!(is_translating(&dev, sid));
    }

    /// Unregistering drives the stream to abort before releasing its StreamID,
    /// so DMA stops while the SMMU still knows about the device, and stops
    /// forwarding that StreamID's invalidations afterwards.
    #[test]
    fn test_unregister_aborts_then_releases_stream_id() {
        use crate::spec::ste::SteConfig;
        let dev = make_accel_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);

        let sid = 0x100u32;
        write_test_ste(&dev, sid, SteConfig::S1_TRANS);

        let backend = RecordingBackend::new();
        let registration = dev
            .shared_state
            .register_accel_device(sid, backend.clone())
            .expect("register backend");
        backend.take();
        assert!(is_translating(&dev, sid));

        drop(registration);
        assert_eq!(backend.take(), vec![crate::shared::StreamConfig::Abort]);
        assert!(!is_translating(&dev, sid));

        // The StreamID is now unowned, so a re-drive has nothing to apply.
        dev.shared_state
            .apply_stream_configs_in_range(ALL_SIDS)
            .expect("re-drive with no registrations");
        assert!(backend.take().is_empty());
    }

    /// A guest moving a device to a new BDF retires the old StreamID through
    /// abort and registers the new one, which picks up its own STE policy.
    /// The backend is never told that its StreamID changed.
    #[test]
    fn test_rebind_retires_old_stream_id_then_applies_new() {
        use crate::spec::ste::SteConfig;

        let dev = make_accel_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);
        write_test_ste(&dev, 0x100, SteConfig::BYPASS);
        write_test_ste(&dev, 0x200, SteConfig::S1_TRANS);

        let backend = RecordingBackend::new();
        let registration = dev
            .shared_state
            .register_accel_device(0x100, backend.clone())
            .expect("register first StreamID");
        assert_eq!(backend.take(), vec![crate::shared::StreamConfig::Bypass]);

        drop(registration);
        assert_eq!(backend.take(), vec![crate::shared::StreamConfig::Abort]);

        let _registration = dev
            .shared_state
            .register_accel_device(0x200, backend.clone())
            .expect("register second StreamID");
        let applied = backend.take();
        assert!(
            matches!(applied[..], [crate::shared::StreamConfig::Translate { .. }]),
            "expected Translate, got {applied:?}"
        );
        assert!(!is_translating(&dev, 0x100));
        assert!(is_translating(&dev, 0x200));
    }

    /// A StreamID names exactly one device, because the host keys its vDevice
    /// table by it. A guest aliasing two devices mid bus-renumber leaves the
    /// newcomer unregistered — and so blocked — until the incumbent moves off.
    #[test]
    fn test_duplicate_stream_id_is_rejected() {
        let dev = make_accel_device();

        let first = RecordingBackend::new();
        let first_registration = dev
            .shared_state
            .register_accel_device(0x100, first.clone())
            .expect("register first backend");
        first.take();

        let second = RecordingBackend::new();
        let err = dev
            .shared_state
            .register_accel_device(0x100, second.clone())
            .expect_err("an aliased StreamID must be rejected")
            .to_string();
        assert!(err.contains("already registered"), "{err}");
        // Neither device was touched: the incumbent keeps its policy and the
        // newcomer was never attached.
        assert!(first.take().is_empty());
        assert!(second.take().is_empty());

        // Once the incumbent moves off, the retry succeeds.
        drop(first_registration);
        first.take();
        let _second_registration = dev
            .shared_state
            .register_accel_device(0x100, second.clone())
            .expect("retry once the conflict clears");
        assert_eq!(second.take(), vec![crate::shared::StreamConfig::Bypass]);
    }

    /// A backend that rejects its initial policy is left aborting, but stays
    /// registered: the guest's next `CFGI_STE` for that StreamID retries it.
    #[test]
    fn test_registration_survives_a_rejected_initial_policy() {
        use crate::spec::ste::SteConfig;

        let dev = make_accel_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);
        write_test_ste(&dev, 0x100, SteConfig::BYPASS);

        let backend = RecordingBackend::new();
        backend.fail_next();
        let _registration = dev
            .shared_state
            .register_accel_device(0x100, backend.clone())
            .expect("registration succeeds despite a rejected policy");
        // The rejected Bypass falls back to Abort rather than leaving the
        // device on its previous attachment.
        assert_eq!(
            backend.take(),
            vec![
                crate::shared::StreamConfig::Bypass,
                crate::shared::StreamConfig::Abort,
            ]
        );

        dev.shared_state
            .apply_stream_config(0x100)
            .expect("retry the stream's policy");
        assert_eq!(backend.take(), vec![crate::shared::StreamConfig::Bypass]);
    }
    /// Shared policy re-drive applies every registered backend's current
    /// policy (used on CR0/GBPA writes, reset, restore, and CFGI_ALL).
    #[test]
    fn test_redrive_reapplies_all() {
        use crate::spec::ste::SteConfig;
        let dev = make_accel_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);

        let backend = RecordingBackend::new();
        let _registration = dev
            .shared_state
            .register_accel_device(0x100, backend.clone())
            .expect("register backend");
        // Disabled + GBPA.ABORT=0 applies Bypass after RID capture.
        assert_eq!(
            backend.take().last().copied(),
            Some(crate::shared::StreamConfig::Bypass)
        );

        // Enable and program an abort STE, then re-drive.
        let sid = 0x100u32;
        write_test_ste(&dev, sid, SteConfig::ABORT);
        transition_to_enabled(&dev.shared_state);
        dev.shared_state
            .apply_stream_configs_in_range(ALL_SIDS)
            .expect("apply all stream configs");
        assert_eq!(
            backend.take().last().copied(),
            Some(crate::shared::StreamConfig::Abort)
        );
    }

    #[test]
    fn test_unrelated_cr0_writes_do_not_redrive_streams() {
        let mut dev = make_accel_device();
        let backend = RecordingBackend::new();
        let _registration = dev
            .shared_state
            .register_accel_device(0x100, backend.clone())
            .expect("register backend");
        backend.take();

        write32(&mut dev, CR0, Cr0::new().with_cmdqen(true).into());
        write32(
            &mut dev,
            CR0,
            Cr0::new().with_cmdqen(true).with_eventqen(true).into(),
        );
        write32(
            &mut dev,
            CR0,
            Cr0::new().with_cmdqen(true).with_eventqen(true).into(),
        );
        assert!(backend.take().is_empty());
    }

    /// A stream whose policy the backend rejects is left aborting; the
    /// transition still reaches every later registration and still acks CR0.
    #[test]
    fn test_smmuen_failure_aborts_stream_and_continues() {
        use crate::spec::ste::SteConfig;

        let mut dev = make_accel_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        let backends = [
            RecordingBackend::new(),
            RecordingBackend::new(),
            RecordingBackend::new(),
        ];
        let mut registrations = Vec::new();
        for (index, backend) in backends.iter().enumerate() {
            // Keep the SIDs inside the test stream table (512 entries).
            let rid = 0x100 + (index as u16) * 0x20;
            write_test_ste(&dev, u32::from(rid), SteConfig::BYPASS);
            let registration = dev
                .shared_state
                .register_accel_device(rid.into(), backend.clone())
                .expect("register backend");
            backend.take();
            registrations.push(registration);
        }

        backends[1].fail_next();
        write32(&mut dev, CR0, Cr0::new().with_smmuen(true).into());

        // CR0ACK must track CR0: a register update completes in finite time and
        // has no architectural reject path.
        assert!(Cr0::from(read32(&mut dev, CR0)).smmuen());
        assert!(Cr0::from(read32(&mut dev, CR0ACK)).smmuen());
        assert!(dev.shared_state.translation_policy().enabled);

        assert_eq!(
            backends[0].take(),
            vec![crate::shared::StreamConfig::Bypass]
        );
        assert_eq!(
            backends[1].take(),
            vec![
                crate::shared::StreamConfig::Bypass,
                crate::shared::StreamConfig::Abort,
            ]
        );
        assert_eq!(
            backends[2].take(),
            vec![crate::shared::StreamConfig::Bypass]
        );
    }

    #[test]
    fn test_gbpa_while_enabled_is_future_policy_only() {
        use crate::spec::ste::SteConfig;

        let mut dev = make_accel_device();
        write64(
            &mut dev,
            STRTAB_BASE,
            StrtabBase::new()
                .with_addr_bits(TEST_STRTAB_GPA >> 6)
                .into(),
        );
        write32(
            &mut dev,
            STRTAB_BASE_CFG,
            StrtabBaseCfg::new()
                .with_log2size(TEST_STRTAB_LOG2SIZE)
                .into(),
        );
        write_test_ste(&dev, 0x100, SteConfig::BYPASS);

        let backend = RecordingBackend::new();
        let _registration = dev
            .shared_state
            .register_accel_device(0x100, backend.clone())
            .expect("register backend");
        backend.take();

        write32(&mut dev, CR0, Cr0::new().with_smmuen(true).into());
        assert_eq!(backend.take(), vec![crate::shared::StreamConfig::Bypass]);

        write32(
            &mut dev,
            GBPA,
            Gbpa::new().with_update(true).with_abort(true).into(),
        );
        assert!(backend.take().is_empty());

        write32(&mut dev, CR0, Cr0::new().into());
        assert_eq!(backend.take(), vec![crate::shared::StreamConfig::Abort]);
    }

    /// Restore recomputes stream policy from guest memory, so a backend that
    /// rejects a guest-authored STE must not fail the restore — otherwise the
    /// guest could poison its own resume. The stream is left aborting instead.
    #[pal_async::async_test]
    async fn test_restore_survives_backend_failure() {
        use crate::spec::ste::SteConfig;

        let mut dev = make_accel_device();
        write64(
            &mut dev,
            STRTAB_BASE,
            StrtabBase::new()
                .with_addr_bits(TEST_STRTAB_GPA >> 6)
                .into(),
        );
        write32(
            &mut dev,
            STRTAB_BASE_CFG,
            StrtabBaseCfg::new()
                .with_log2size(TEST_STRTAB_LOG2SIZE)
                .into(),
        );
        write_test_ste(&dev, 0x100, SteConfig::BYPASS);

        let backend = RecordingBackend::new();
        let _registration = dev
            .shared_state
            .register_accel_device(0x100, backend.clone())
            .expect("register backend");
        backend.take();

        write32(&mut dev, CR0, Cr0::new().with_smmuen(true).into());
        let saved = dev.save().expect("save enabled state");
        dev.reset().await;
        backend.take();

        backend.fail_next();
        dev.restore(saved).expect("restore must not fail");
        assert!(Cr0::from(read32(&mut dev, CR0)).smmuen());
        assert!(Cr0::from(read32(&mut dev, CR0ACK)).smmuen());
        assert!(dev.shared_state.translation_policy().enabled);
        assert_eq!(
            backend.take(),
            vec![
                crate::shared::StreamConfig::Bypass,
                crate::shared::StreamConfig::Abort,
            ]
        );
    }

    /// A rejected policy replacement leaves the stream aborting rather than on
    /// its old translating attachment. `CMD_CFGI_STE` has no content-derived
    /// failure (SMMUv3 §4.3.1), so it is still consumed and raises no CMDQ
    /// error — the guest sees the same aborting stream an ILLEGAL STE gives it.
    #[test]
    fn test_cfgi_failure_aborts_stream() {
        use crate::spec::ste::SteConfig;

        let mut dev = make_cmdq_test_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);

        let sid = 0x100;
        write_test_ste(&dev, sid, SteConfig::S1_TRANS);
        let backend = RecordingBackend::new();
        let _registration = dev
            .shared_state
            .register_accel_device(0x100, backend.clone())
            .expect("register translating backend");
        assert!(is_translating(&dev, sid));

        write_test_ste(&dev, sid, SteConfig::BYPASS);
        backend.fail_next();
        let cfgi = CmdCfgiSte::new()
            .with_opcode(CmdOpcode::CFGI_STE.0)
            .with_sid(sid);
        write_cmdq_entry(
            &dev,
            0,
            &CmdEntry {
                qw0: cfgi.into(),
                qw1: 0,
            },
        );
        write_cmdq_entry(&dev, 1, &sync_entry());

        write32(&mut dev, CMDQ_PROD, 2);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 2);
        assert_eq!(cons.err(), 0);
        // No longer translating, so its SID-based invalidations stop forwarding.
        assert!(!is_translating(&dev, sid));
        assert_eq!(
            backend.take().last().copied(),
            Some(crate::shared::StreamConfig::Abort)
        );
    }

    /// A backend that flips a shared flag when dropped.
    struct DropTrackingBackend {
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::shared::AcceleratedStreamBackend for DropTrackingBackend {
        fn set_stream_config(&self, _config: crate::shared::StreamConfig) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl Drop for DropTrackingBackend {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Unregistering synchronously removes the device from the forwarding table
    /// and drops the backend without requiring emulator activity.
    #[test]
    fn test_unregister_removes_and_drops_backend() {
        use crate::spec::ste::SteConfig;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        let dev = make_accel_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);

        let sid = 0x100u32;
        write_test_ste(&dev, sid, SteConfig::S1_TRANS);

        let dropped = Arc::new(AtomicBool::new(false));
        // Register a translating stream. Do NOT keep a clone of the backend, so
        // the shared table holds the only `Arc`.
        let registration = dev
            .shared_state
            .register_accel_device(
                sid,
                Arc::new(DropTrackingBackend {
                    dropped: dropped.clone(),
                }),
            )
            .expect("register backend");
        // Device present, translating → its SID-based invalidations forward.
        assert!(is_translating(&dev, sid));
        assert!(!dropped.load(Ordering::Relaxed));

        // Unregister without touching the stopped emulator.
        drop(registration);
        assert!(!is_translating(&dev, sid));
        assert!(
            dropped.load(Ordering::Relaxed),
            "backend should be torn down synchronously"
        );
    }

    /// Dropping the [`AccelRegistration`] guard synchronously unregisters a
    /// removed/hot-unplugged device while the emulator is stopped.
    #[test]
    fn test_accel_registration_guard_unregisters_on_drop() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        let dev = make_accel_device();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = dev
            .shared_state
            .register_accel_device(
                0x100,
                Arc::new(DropTrackingBackend {
                    dropped: dropped.clone(),
                }),
            )
            .expect("register backend");
        assert!(!dropped.load(Ordering::Relaxed));

        // Device teardown drops the guard; no emulator turn is needed.
        drop(guard);
        assert!(dropped.load(Ordering::Relaxed));
    }

    /// The host vIOMMU's lifetime follows the stream backends that reference
    /// it rather than this table's occupancy, so it is released with the last
    /// accelerated device without the SMMU tracking that itself. Invalidations
    /// after that are no-ops.
    #[test]
    fn test_viommu_released_with_last_backend() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        struct DropTrackingViommu(Arc<AtomicBool>);

        impl crate::shared::Invalidate for DropTrackingViommu {
            fn invalidate(&self, _entries: &[[u64; 2]]) -> Result<(), usize> {
                Ok(())
            }
        }

        impl Drop for DropTrackingViommu {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        // Mirrors `IommufdStreamBackend`, which holds the `Arc<SmmuAccelState>`
        // that is the vIOMMU. Held only for its refcount.
        struct ViommuHoldingBackend(#[expect(dead_code)] Arc<DropTrackingViommu>);

        impl crate::shared::AcceleratedStreamBackend for ViommuHoldingBackend {
            fn set_stream_config(
                &self,
                _config: crate::shared::StreamConfig,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let dev = make_accel_device();
        let dropped = Arc::new(AtomicBool::new(false));
        let viommu = Arc::new(DropTrackingViommu(dropped.clone()));
        dev.shared_state
            .bind_accel_viommu(test_host_caps(), &viommu)
            .expect("bind host SMMU");

        let registrations: Vec<_> = [0x100u32, 0x200]
            .into_iter()
            .map(|sid| {
                dev.shared_state
                    .register_accel_device(sid, Arc::new(ViommuHoldingBackend(viommu.clone())))
                    .expect("register backend")
            })
            .collect();
        // Only the backends hold it now.
        drop(viommu);
        let mut registrations = registrations.into_iter();

        // One device left: the vIOMMU is still needed.
        drop(registrations.next().unwrap());
        assert!(!dropped.load(Ordering::Relaxed));

        drop(registrations.next().unwrap());
        assert!(dropped.load(Ordering::Relaxed));

        // An unreachable vIOMMU drops batches silently rather than faulting.
        let devices = dev.shared_state.lock_accel_devices();
        assert!(SmmuSharedState::invalidate(&devices, &[[0, 0]]).is_ok());
    }

    /// One vSMMU has one host vIOMMU, so a device naming a different live one
    /// is rejected — the same rule the host-caps check enforces for the
    /// physical SMMU. A device arriving after the last one left may bring a
    /// new vIOMMU, since the old one is gone.
    #[test]
    fn test_bind_accel_viommu_rejects_a_second_live_viommu() {
        let dev = make_accel_device();

        let first = MockViommu::new();
        dev.shared_state
            .bind_accel_viommu(test_host_caps(), &first)
            .expect("bind first vIOMMU");
        // A second device behind the same vSMMU names the same vIOMMU.
        dev.shared_state
            .bind_accel_viommu(test_host_caps(), &first)
            .expect("rebind the same vIOMMU");

        let second = MockViommu::new();
        let err = dev
            .shared_state
            .bind_accel_viommu(test_host_caps(), &second)
            .expect_err("a competing live vIOMMU must be rejected")
            .to_string();
        assert!(err.contains("cannot be backed by two"), "{err}");

        drop(first);
        dev.shared_state
            .bind_accel_viommu(test_host_caps(), &second)
            .expect("bind a replacement vIOMMU once the first is gone");
    }

    /// The registration table stays locked for the duration of the host
    /// invalidation, so a concurrent StreamID rebind or device teardown cannot
    /// retire a vDevice the in-flight batch names.
    #[test]
    fn test_invalidation_holds_registration_lock() {
        use crate::spec::ste::SteConfig;
        use std::sync::Weak;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        struct LockObservingSink {
            shared: Weak<SmmuSharedState>,
            locked_during_invalidate: AtomicBool,
        }

        impl crate::shared::Invalidate for LockObservingSink {
            fn invalidate(&self, _entries: &[[u64; 2]]) -> Result<(), usize> {
                let shared = self.shared.upgrade().expect("SMMU shared state");
                self.locked_during_invalidate
                    .store(shared.accel_devices_locked(), Ordering::Relaxed);
                Ok(())
            }
        }

        let mut dev = make_cmdq_test_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);

        let sid = 0x100;
        write_test_ste(&dev, sid, SteConfig::S1_TRANS);
        let registration = dev
            .shared_state
            .register_accel_device(0x100, Arc::new(MockStreamBackend))
            .expect("register translating backend");
        std::mem::forget(registration);

        let sink = Arc::new(LockObservingSink {
            shared: Arc::downgrade(&dev.shared_state),
            locked_during_invalidate: AtomicBool::new(false),
        });
        dev.shared_state
            .bind_accel_viommu(test_host_caps(), &sink)
            .expect("bind host SMMU");

        write_cmdq_entry(&dev, 0, &cfgi_cd_entry(CmdOpcode::CFGI_CD, sid, 0));
        write_cmdq_entry(&dev, 1, &sync_entry());
        write32(&mut dev, CMDQ_PROD, 2);

        assert!(sink.locked_during_invalidate.load(Ordering::Relaxed));
        // The lock is released once the batch is consumed.
        assert!(!dev.shared_state.accel_devices_locked());
    }

    /// `CFGI_STE_RANGE` re-drives only the streams inside the aligned range it
    /// names (SMMUv3 §4.3.2), leaving streams outside it untouched.
    #[test]
    fn test_cfgi_ste_range_honors_range() {
        use crate::spec::ste::SteConfig;

        let mut dev = make_cmdq_test_device();
        dev.shared_state
            .set_strtab(TEST_STRTAB_GPA, TEST_STRTAB_LOG2SIZE);
        transition_to_enabled(&dev.shared_state);

        // Two streams four StreamIDs apart, both currently on Bypass.
        let backends = [RecordingBackend::new(), RecordingBackend::new()];
        for (index, backend) in backends.iter().enumerate() {
            let sid = 0x100 + (index as u32) * 4;
            write_test_ste(&dev, sid, SteConfig::BYPASS);
            let registration = dev
                .shared_state
                .register_accel_device(sid, backend.clone())
                .expect("register backend");
            std::mem::forget(registration);
            backend.take();
        }

        // Flip both STEs, then invalidate a Range=1 (2 StreamIDs, aligned to
        // 0x100) span that covers only the first stream.
        write_test_ste(&dev, 0x100, SteConfig::ABORT);
        write_test_ste(&dev, 0x104, SteConfig::ABORT);
        write_cmdq_entry(
            &dev,
            0,
            &CmdEntry {
                qw0: CmdCfgiSteRange::new()
                    .with_opcode(CmdOpcode::CFGI_STE_RANGE.0)
                    .with_sid(0x100)
                    .into(),
                qw1: 1,
            },
        );
        write32(&mut dev, CMDQ_PROD, 1);

        assert_eq!(backends[0].take(), vec![crate::shared::StreamConfig::Abort]);
        assert!(backends[1].take().is_empty());

        // Range=31 is CFGI_ALL and reaches both.
        write_cmdq_entry(
            &dev,
            1,
            &CmdEntry {
                qw0: CmdCfgiSteRange::new()
                    .with_opcode(CmdOpcode::CFGI_STE_RANGE.0)
                    .with_sid(0x100)
                    .into(),
                qw1: 31,
            },
        );
        write32(&mut dev, CMDQ_PROD, 2);

        assert_eq!(backends[0].take(), vec![crate::shared::StreamConfig::Abort]);
        assert_eq!(backends[1].take(), vec![crate::shared::StreamConfig::Abort]);
    }

    /// Build a SID-based `CFGI_CD`/`CFGI_CD_ALL` entry for `sid`.
    fn cfgi_cd_entry(opcode: CmdOpcode, sid: u32, qw1: u64) -> CmdEntry {
        CmdEntry {
            qw0: CmdCfgiCd::new().with_opcode(opcode.0).with_sid(sid).into(),
            qw1,
        }
    }

    /// Build a `CMD_SYNC(SIG_SEV)` entry.
    fn sync_entry() -> CmdEntry {
        let sync = CmdSync::new()
            .with_opcode(CmdOpcode::CMD_SYNC.0)
            .with_cs(SyncCs::SIG_SEV.0);
        CmdEntry {
            qw0: sync.into(),
            qw1: 0,
        }
    }

    /// Build a forwardable TLBI entry with the given opcode and operand.
    fn tlbi_entry(opcode: CmdOpcode, qw1: u64) -> CmdEntry {
        CmdEntry {
            qw0: opcode.0 as u64,
            qw1,
        }
    }

    fn make_accel_cmdq_test_device() -> (SmmuDevice, Arc<MockViommu>) {
        let dev = make_cmdq_test_device();
        let sink = MockViommu::new();
        dev.shared_state
            .bind_accel_viommu(test_host_caps(), &sink)
            .expect("bind host SMMU");
        (dev, sink)
    }

    /// A run of forwardable commands is flushed as one batch at `CMD_SYNC`.
    #[test]
    fn test_cmdq_batch_flush_on_sync() {
        let (mut dev, sink) = make_accel_cmdq_test_device();

        write_cmdq_entry(&dev, 0, &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xA));
        write_cmdq_entry(&dev, 1, &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xB));
        write_cmdq_entry(&dev, 2, &tlbi_entry(CmdOpcode::TLBI_NH_ASID, 0xC));
        write_cmdq_entry(&dev, 3, &sync_entry());

        write32(&mut dev, CMDQ_PROD, 4);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 4);
        assert_eq!(cons.err(), 0);

        // A single batch of three entries, in program order.
        let batches = sink.batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0],
            vec![
                [CmdOpcode::TLBI_NH_VA.0 as u64, 0xA],
                [CmdOpcode::TLBI_NH_VA.0 as u64, 0xB],
                [CmdOpcode::TLBI_NH_ASID.0 as u64, 0xC],
            ]
        );
    }

    /// A configuration command between two invalidation runs forces a flush, so
    /// the runs land in separate batches.
    #[test]
    fn test_cmdq_batch_flush_on_config() {
        let (mut dev, sink) = make_accel_cmdq_test_device();

        write_cmdq_entry(&dev, 0, &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xA));
        write_cmdq_entry(&dev, 1, &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xB));
        // CFGI_STE is a boundary: flush before it. With no backend registered
        // for the SID it is otherwise a no-op.
        write_cmdq_entry(
            &dev,
            2,
            &CmdEntry {
                qw0: CmdOpcode::CFGI_STE.0 as u64,
                qw1: 0,
            },
        );
        write_cmdq_entry(&dev, 3, &tlbi_entry(CmdOpcode::TLBI_NH_VAA, 0xC));
        write_cmdq_entry(&dev, 4, &sync_entry());

        write32(&mut dev, CMDQ_PROD, 5);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 5);
        assert_eq!(cons.err(), 0);

        let batches = sink.batches();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[1], vec![[CmdOpcode::TLBI_NH_VAA.0 as u64, 0xC]]);
    }

    /// A trailing invalidation run with no following barrier is flushed at
    /// queue drain.
    #[test]
    fn test_cmdq_batch_flush_on_drain() {
        let (mut dev, sink) = make_accel_cmdq_test_device();

        write_cmdq_entry(&dev, 0, &tlbi_entry(CmdOpcode::TLBI_NH_ALL, 0));
        write_cmdq_entry(&dev, 1, &tlbi_entry(CmdOpcode::TLBI_NH_ALL, 0));

        write32(&mut dev, CMDQ_PROD, 2);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 2);
        assert_eq!(cons.err(), 0);

        let batches = sink.batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    /// `CFGI_CD` / `CFGI_CD_ALL` are forwarded to the host (in-place CD edits
    /// must invalidate the host's cached CD) once the stream is translating.
    #[test]
    fn test_cmdq_cfgi_cd_forwarded() {
        let (mut dev, sink) = make_accel_cmdq_test_device();
        // SID-based invalidations only forward once the stream is translating
        // (the guest has driven its STE to an `S1_TRANS` config).
        let sid = register_translating_stream(&dev, 1);

        let cfgi_cd = cfgi_cd_entry(CmdOpcode::CFGI_CD, sid, 0x1234);
        let cfgi_cd_all = cfgi_cd_entry(CmdOpcode::CFGI_CD_ALL, sid, 0);
        write_cmdq_entry(&dev, 0, &cfgi_cd);
        write_cmdq_entry(&dev, 1, &cfgi_cd_all);
        write_cmdq_entry(&dev, 2, &sync_entry());

        write32(&mut dev, CMDQ_PROD, 3);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 3);
        assert_eq!(cons.err(), 0);

        let batches = sink.batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0],
            vec![[cfgi_cd.qw0, 0x1234], [cfgi_cd_all.qw0, 0]]
        );
    }

    /// A SID-based invalidation for a stream that is not translating (e.g. the
    /// guest's `CFGI_CD` before it installs the translating STE) is skipped
    /// — consumed as a no-op, not forwarded, and never raises `CERROR_ILL`.
    #[test]
    fn test_cmdq_cfgi_cd_skipped_when_not_translating() {
        let (mut dev, sink) = make_accel_cmdq_test_device();
        let sid = register_test_stream(&dev, 1);

        write_cmdq_entry(&dev, 0, &cfgi_cd_entry(CmdOpcode::CFGI_CD, sid, 0x1234));
        write_cmdq_entry(&dev, 1, &sync_entry());

        write32(&mut dev, CMDQ_PROD, 2);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 2); // both consumed
        assert_eq!(cons.err(), 0); // no CMDQ error

        // Nothing forwarded (the batch was empty at the CMD_SYNC flush).
        assert!(sink.batches().is_empty());
    }

    /// A skipped SID-based invalidation flushes any pending batch first, so the
    /// partial-failure → `CMDQ_CONS` index mapping stays correct (no gaps).
    #[test]
    fn test_cmdq_skip_flushes_pending_batch() {
        let (mut dev, sink) = make_accel_cmdq_test_device();
        let sid = register_test_stream(&dev, 1); // not translating → CFGI_CD skipped

        write_cmdq_entry(&dev, 0, &tlbi_entry(CmdOpcode::TLBI_NH_ALL, 0xA));
        write_cmdq_entry(&dev, 1, &cfgi_cd_entry(CmdOpcode::CFGI_CD, sid, 0));
        write_cmdq_entry(&dev, 2, &tlbi_entry(CmdOpcode::TLBI_NH_ALL, 0xB));
        write_cmdq_entry(&dev, 3, &sync_entry());

        write32(&mut dev, CMDQ_PROD, 4);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 4);
        assert_eq!(cons.err(), 0);

        // The leading TLBI flushed as its own batch (before the skip), then the
        // trailing TLBI flushed at CMD_SYNC.
        let batches = sink.batches();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0], vec![[CmdOpcode::TLBI_NH_ALL.0 as u64, 0xA]]);
        assert_eq!(batches[1], vec![[CmdOpcode::TLBI_NH_ALL.0 as u64, 0xB]]);
    }

    /// `TLBI_S12_VMALL` is illegal on a stage-1-only SMMU and raises
    /// `CERROR_ILL`, stopping at the offending command — it is never forwarded.
    #[test]
    fn test_cmdq_s12_vmall_illegal() {
        let (mut dev, sink) = make_accel_cmdq_test_device();

        write_cmdq_entry(&dev, 0, &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xA));
        write_cmdq_entry(
            &dev,
            1,
            &CmdEntry {
                qw0: CmdOpcode::TLBI_S12_VMALL.0 as u64,
                qw1: 0,
            },
        );
        write_cmdq_entry(&dev, 2, &sync_entry());

        write32(&mut dev, CMDQ_PROD, 3);

        // Processing stops at the illegal command (index 1).
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 1);
        assert_eq!(cons.err(), CmdqError::CERROR_ILL.0);

        // The preceding forwardable command was flushed before the illegal one.
        let batches = sink.batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![[CmdOpcode::TLBI_NH_VA.0 as u64, 0xA]]);
    }

    /// `ATC_INV` is illegal while ATS is disabled (`IDR0.ATS=0`) and raises
    /// `CERROR_ILL` instead of being forwarded.
    #[test]
    fn test_cmdq_atc_inv_illegal_when_ats_disabled() {
        let (mut dev, sink) = make_accel_cmdq_test_device();
        assert!(!dev.idr0.ats());

        write_cmdq_entry(
            &dev,
            0,
            &CmdEntry {
                qw0: CmdOpcode::ATC_INV.0 as u64,
                qw1: 0,
            },
        );
        write_cmdq_entry(&dev, 1, &sync_entry());

        write32(&mut dev, CMDQ_PROD, 2);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 0);
        assert_eq!(cons.err(), CmdqError::CERROR_ILL.0);
        assert!(sink.batches().is_empty());
    }

    /// On a partial host failure, `CMDQ_CONS` lands on the offending entry
    /// (batch start + processed count) and `CERROR_ILL` is raised.
    #[test]
    fn test_cmdq_partial_failure_cons() {
        let (mut dev, sink) = make_accel_cmdq_test_device();
        // The host accepts only the first entry of any batch.
        sink.set_accept_limit(1);

        write_cmdq_entry(&dev, 0, &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xA));
        write_cmdq_entry(&dev, 1, &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xB));
        write_cmdq_entry(&dev, 2, &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xC));
        write_cmdq_entry(&dev, 3, &sync_entry());

        write32(&mut dev, CMDQ_PROD, 4);

        // Batch started at index 0, host processed 1 → offending entry = 1.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 1);
        assert_eq!(cons.err(), CmdqError::CERROR_ILL.0);
    }

    /// A host failure flushing commands before an unreadable CMDQ entry takes
    /// precedence, so `CMDQ_CONS` does not advance past an invalidation the
    /// host never handled.
    #[test]
    fn test_cmdq_partial_failure_precedes_fetch_abort() {
        // Place exactly two entries at the end of mapped memory. PROD=3 makes
        // the third fetch cross the boundary and fail.
        const FETCH_ABORT_CMDQ_GPA: u64 = 0x3fe0;
        let gm = GuestMemory::allocate(0x4000);
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm, &test_config(), None, None);
        let cmdq_base = QueueBase::new()
            .with_log2size(TEST_CMDQ_LOG2SIZE)
            .with_addr_bits(FETCH_ABORT_CMDQ_GPA >> 5);
        write64(&mut dev, CMDQ_BASE, cmdq_base.into());
        write32(&mut dev, CR0, Cr0::new().with_cmdqen(true).into());

        let sink = MockViommu::new();
        sink.set_accept_limit(1);
        dev.shared_state
            .bind_accel_viommu(test_host_caps(), &sink)
            .expect("bind host SMMU");

        dev.guest_memory
            .write_plain(
                FETCH_ABORT_CMDQ_GPA,
                &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xA),
            )
            .expect("write first CMDQ entry");
        dev.guest_memory
            .write_plain(
                FETCH_ABORT_CMDQ_GPA + size_of::<CmdEntry>() as u64,
                &tlbi_entry(CmdOpcode::TLBI_NH_VA, 0xB),
            )
            .expect("write second CMDQ entry");
        write32(&mut dev, CMDQ_PROD, 3);

        // The host accepted entry 0 and rejected entry 1. The later fetch
        // abort at entry 2 must not move CONS past the rejected invalidation.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 1);
        assert_eq!(cons.err(), CmdqError::CERROR_ILL.0);
    }

    // =========================================================================
    // EVTQ tests
    // =========================================================================

    /// Size of the test EVTQ: 2^3 = 8 entries.
    const TEST_EVTQ_LOG2SIZE: u8 = 3;
    /// GPA where the test EVTQ lives.
    const TEST_EVTQ_GPA: u64 = 0x3_0000;
    /// GPA where the EVTQ MSI writes go.
    const TEST_EVTQ_MSI_GPA: u64 = 0x2_0100;

    /// Create a device with EVTQ configured and enabled.
    fn make_evtq_test_device() -> SmmuDevice {
        let gm = GuestMemory::allocate(0x4_0000);
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm, &test_config(), None, None);

        // Program EVTQ_BASE.
        let evtq_base = QueueBase::new()
            .with_log2size(TEST_EVTQ_LOG2SIZE)
            .with_addr_bits(TEST_EVTQ_GPA >> 5);
        write64(&mut dev, EVENTQ_BASE, evtq_base.into());

        // Program EVTQ MSI config.
        write64(&mut dev, EVENTQ_IRQ_CFG0, TEST_EVTQ_MSI_GPA);
        write32(&mut dev, EVENTQ_IRQ_CFG1, 0xBEEF);

        // Enable EVTQEN + EVENTQ_IRQEN.
        let cr0 = Cr0::new().with_eventqen(true);
        write32(&mut dev, CR0, cr0.into());
        let irq_ctrl = IrqCtrl::new().with_eventq_irqen(true);
        write32(&mut dev, IRQ_CTRL, irq_ctrl.into());

        dev
    }

    #[test]
    fn test_evtq_write_and_read() {
        let mut dev = make_evtq_test_device();

        let event = EvtEntry::translation_fault(42, 0x1000_0000, false);
        dev.shared_state().write_event(event);

        // EVTQ_PROD should advance to 1.
        assert_eq!(read32_page1(&mut dev, EVENTQ_PROD_PAGE1), 1);

        // Read the event record from guest memory.
        let written: EvtEntry = dev
            .guest_memory
            .read_plain(TEST_EVTQ_GPA)
            .expect("read event");
        assert_eq!(
            written.header.event_id(),
            crate::spec::events::EventId::F_TRANSLATION
        );
        assert_eq!(written.sid, 42);
        assert_eq!(written.input_addr, 0x1000_0000);
        assert!(written.flags.rnw()); // read (rnw=true because write=false)
    }

    #[test]
    fn test_evtq_write_advances_prod() {
        let mut dev = make_evtq_test_device();

        // Write two events and verify PROD advances each time.
        let event1 = EvtEntry::translation_fault(1, 0x2000, true);
        dev.shared_state().write_event(event1);
        assert_eq!(read32_page1(&mut dev, EVENTQ_PROD_PAGE1), 1);

        let event2 = EvtEntry::translation_fault(2, 0x3000, false);
        dev.shared_state().write_event(event2);
        assert_eq!(read32_page1(&mut dev, EVENTQ_PROD_PAGE1), 2);

        // Verify both events are in guest memory.
        let e1: EvtEntry = dev.guest_memory.read_plain(TEST_EVTQ_GPA).expect("read");
        assert_eq!(e1.sid, 1);
        let e2: EvtEntry = dev
            .guest_memory
            .read_plain(TEST_EVTQ_GPA + EvtEntry::SIZE as u64)
            .expect("read");
        assert_eq!(e2.sid, 2);
    }

    #[test]
    fn test_evtq_full() {
        let mut dev = make_evtq_test_device();

        let max_entries = 1u32 << TEST_EVTQ_LOG2SIZE; // 8
        for i in 0..max_entries {
            let event = EvtEntry::translation_fault(i, 0x1000 * i as u64, false);
            dev.shared_state().write_event(event);
        }

        // Queue should be full now. PROD = 8 (wrap), CONS = 0.
        assert_eq!(read32_page1(&mut dev, EVENTQ_PROD_PAGE1), max_entries);

        // Writing one more should be dropped (queue full).
        let event = EvtEntry::translation_fault(99, 0xDEAD, false);
        dev.shared_state().write_event(event);

        // PROD should NOT advance (event dropped).
        assert_eq!(read32_page1(&mut dev, EVENTQ_PROD_PAGE1), max_entries);
    }

    #[test]
    fn test_evtq_cons_frees_space() {
        let mut dev = make_evtq_test_device();

        let max_entries = 1u32 << TEST_EVTQ_LOG2SIZE; // 8
        for i in 0..max_entries {
            let event = EvtEntry::translation_fault(i, 0x1000 * i as u64, false);
            dev.shared_state().write_event(event);
        }

        // Queue is full. Advance CONS to consume 3 entries.
        write32_page1(&mut dev, EVENTQ_CONS_PAGE1, 3);

        // Should be able to write 3 more events.
        for i in 0..3u32 {
            let event = EvtEntry::translation_fault(100 + i, 0xF000, false);
            dev.shared_state().write_event(event);
        }

        // PROD should now be at 7 + 3 = 10 (with wrap).
        assert_eq!(read32_page1(&mut dev, EVENTQ_PROD_PAGE1), max_entries + 3);
    }

    // =========================================================================
    // Sub-phase 1J: End-to-End Integration Test
    // =========================================================================

    /// End-to-end test that exercises the full SMMU stack:
    /// MMIO register programming → command queue → stream table → context
    /// descriptor → page table walk → translated DMA read/write → MSI
    /// translation.
    ///
    /// Mimics the Linux SMMUv3 driver initialization sequence:
    /// 1. Probe: read IDR registers, verify feature bits.
    /// 2. Reset: disable SMMU, program CR1, stream table, queues, enable.
    /// 3. Attach: configure STE and CD for a device.
    /// 4. DMA: read/write through TranslatingMemory.
    /// 5. MSI: fire MSI through SmmuSignalMsi with translated address.
    /// 6. Fault: access unmapped IOVA, verify EVTQ event.
    #[test]
    fn test_end_to_end_linux_driver_sequence() {
        use crate::SmmuSignalMsi;
        use crate::spec::cd::Cd;
        use crate::spec::cd::CdDw0;
        use crate::spec::cd::CdDw1;
        use crate::spec::cd::Ips;
        use crate::spec::cd::Tg0;
        use crate::spec::commands::CmdCfgiCd;
        use crate::spec::commands::CmdCfgiSte;
        use crate::spec::commands::CmdCfgiSteRange;
        use crate::spec::commands::CmdOpcode;
        use crate::spec::commands::CmdSync;
        use crate::spec::commands::SyncCs;
        use crate::spec::events::EventId;
        use crate::spec::pt::ApBits;
        use crate::spec::pt::PtDesc;
        use crate::spec::ste::STE_SIZE;
        use crate::spec::ste::Ste;
        use crate::spec::ste::SteConfig;
        use crate::spec::ste::SteDw0;
        use crate::spec::ste::SteDw1;
        use parking_lot::Mutex;
        use pci_core::bus_range::AssignedBusRange;
        use pci_core::msi::SignalMsi;
        use std::sync::Arc;

        // =====================================================================
        // Memory layout constants
        // =====================================================================

        const STRTAB_GPA: u64 = 0x10_0000; // Stream table
        const STRTAB_LOG2SIZE: u8 = 10; // 1024 entries
        const CMDQ_GPA: u64 = 0x20_0000; // Command queue
        const CMDQ_LOG2SIZE: u8 = 5; // 32 entries
        const EVTQ_GPA: u64 = 0x30_0000; // Event queue
        const EVTQ_LOG2SIZE: u8 = 5; // 32 entries
        const CD_GPA: u64 = 0x40_0000; // Context descriptor table
        const PT_L1_GPA: u64 = 0x50_1000; // L1 page table
        const PT_L2_GPA: u64 = 0x50_2000; // L2 page table
        const PT_L3_GPA: u64 = 0x50_3000; // L3 page table
        const DATA_GPA: u64 = 0x60_0000; // Translated target page
        const SYNC_MSI_GPA: u64 = 0x70_0000; // CMD_SYNC MSI target
        const EVTQ_MSI_GPA: u64 = 0x70_0100; // EVTQ MSI target
        // DOORBELL_GPA is a translation output only — never accessed
        // directly by the test. It can exceed the guest memory allocation.
        const DOORBELL_GPA: u64 = 0x7000_0000; // MSI doorbell physical page

        // IOVA space layout (guest-programmed)
        const DMA_IOVA: u64 = 0x0000_0000; // Maps to DATA_GPA
        const DOORBELL_IOVA: u64 = 0x0800_0000; // Maps to DOORBELL_GPA

        // Device identity
        const SEGMENT: u16 = 0;
        const BUS: u8 = 1;
        const STREAM_ID_BASE: u32 = (SEGMENT as u32) << 16;
        const STREAM_ID: u32 = STREAM_ID_BASE + ((BUS as u32) << 8);

        // =====================================================================
        // Mock MSI target
        // =====================================================================

        struct MockSignalMsi {
            calls: Mutex<Vec<(Option<u32>, u64, u32)>>,
        }

        impl MockSignalMsi {
            fn new() -> Arc<Self> {
                Arc::new(Self {
                    calls: Mutex::new(Vec::new()),
                })
            }

            fn take_calls(&self) -> Vec<(Option<u32>, u64, u32)> {
                std::mem::take(&mut *self.calls.lock())
            }
        }

        impl SignalMsi for MockSignalMsi {
            fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
                self.calls.lock().push((devid, address, data));
            }
        }

        // Helper to write a command entry to the CMDQ at a given index.
        fn write_cmd(gm: &GuestMemory, index: u32, entry: &CmdEntry) {
            let addr = CMDQ_GPA + (index as u64) * (size_of::<CmdEntry>() as u64);
            gm.write_plain(addr, entry).expect("write cmd entry");
        }

        // =====================================================================
        // Allocate guest memory and create device
        // =====================================================================

        let gm = GuestMemory::allocate(0x80_0000); // 8 MiB
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm.clone(), &test_config(), None, None);

        // =====================================================================
        // Step 1: Probe — read IDR registers (arm_smmu_device_hw_probe)
        // =====================================================================

        let idr0 = Idr0::from(read32(&mut dev, IDR0));
        assert!(idr0.s1p(), "S1 translation must be supported");
        assert_eq!(idr0.ttf(), 0b10, "TTF must include AArch64");
        assert!(!idr0.msi(), "MSI must not be advertised (wired SPIs)");
        assert_eq!(idr0.ttendian(), 0b10, "Must be little-endian");
        assert_eq!(idr0.st_level(), 0b00, "Must be linear stream table");

        let idr1 = Idr1::from(read32(&mut dev, IDR1));
        assert_eq!(idr1.sidsize(), 16);
        assert!(idr1.cmdqs() >= 5, "CMDQS must support our queue size");

        let idr5 = Idr5::from(read32(&mut dev, IDR5));
        assert!(idr5.gran4k(), "4K granule must be supported");

        // =====================================================================
        // Step 2: Reset — arm_smmu_device_reset() sequence
        // =====================================================================

        // 2a. Disable SMMU.
        write32(&mut dev, CR0, 0);
        assert_eq!(
            read32(&mut dev, CR0ACK),
            0,
            "CR0ACK must reflect disabled state"
        );

        // 2b. Program CR1 (memory attributes for table walks).
        let cr1 = Cr1::new()
            .with_table_sh(0b11) // Inner shareable
            .with_table_oc(0b01) // Write-back
            .with_table_ic(0b01) // Write-back
            .with_queue_sh(0b11)
            .with_queue_oc(0b01)
            .with_queue_ic(0b01);
        write32(&mut dev, CR1, cr1.into());

        // 2c. Program stream table base.
        let strtab_base = StrtabBase::new().with_addr_bits(STRTAB_GPA >> 6);
        write64(&mut dev, STRTAB_BASE, strtab_base.into());
        let strtab_cfg = StrtabBaseCfg::new()
            .with_log2size(STRTAB_LOG2SIZE)
            .with_fmt(0); // Linear
        write32(&mut dev, STRTAB_BASE_CFG, strtab_cfg.into());

        // Verify readback.
        assert_eq!(
            StrtabBase::from(read64(&mut dev, STRTAB_BASE)).addr(),
            STRTAB_GPA
        );

        // 2d. Program CMDQ.
        let cmdq_base = QueueBase::new()
            .with_log2size(CMDQ_LOG2SIZE)
            .with_addr_bits(CMDQ_GPA >> 5);
        write64(&mut dev, CMDQ_BASE, cmdq_base.into());
        write32(&mut dev, CMDQ_PROD, 0);
        // CMDQ_CONS is SMMU-writable only; starts at 0.

        // 2e. Enable CMDQEN.
        let cr0_cmdqen = Cr0::new().with_cmdqen(true);
        write32(&mut dev, CR0, cr0_cmdqen.into());
        assert_eq!(
            Cr0::from(read32(&mut dev, CR0ACK)).cmdqen(),
            true,
            "CMDQEN must be acknowledged"
        );

        // 2f. Issue CFGI_ALL + CMD_SYNC (invalidate all cached STEs).
        let mut cmd_idx: u32 = 0;

        let cfgi_all = CmdEntry {
            qw0: CmdCfgiSteRange::new()
                .with_opcode(CmdOpcode::CFGI_STE_RANGE.0)
                .into(),
            qw1: CmdCfgiSteRange::RANGE_ALL as u64,
        };
        write_cmd(&gm, cmd_idx, &cfgi_all);
        cmd_idx += 1;

        let sync0 = CmdEntry {
            qw0: CmdSync::new()
                .with_opcode(CmdOpcode::CMD_SYNC.0)
                .with_cs(SyncCs::SIG_IRQ.0)
                .with_msi_data(0xAAAA)
                .into(),
            qw1: (SYNC_MSI_GPA >> 2) << 2,
        };
        write_cmd(&gm, cmd_idx, &sync0);
        cmd_idx += 1;

        write32(&mut dev, CMDQ_PROD, cmd_idx);

        // Verify CONS advanced.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), cmd_idx, "CMDQ_CONS must advance to PROD");

        // Verify CMD_SYNC MSI written.
        let sync_val: u32 = gm.read_plain(SYNC_MSI_GPA).expect("read sync MSI");
        assert_eq!(sync_val, 0xAAAA, "CMD_SYNC MSI data must match");

        // 2g. Issue TLBI_NSNH_ALL + CMD_SYNC.
        let tlbi_all = CmdEntry {
            qw0: CmdOpcode::TLBI_NSNH_ALL.0 as u64,
            qw1: 0,
        };
        write_cmd(&gm, cmd_idx, &tlbi_all);
        cmd_idx += 1;

        // Reset sync target.
        gm.write_at(SYNC_MSI_GPA, &0u32.to_le_bytes()).unwrap();

        let sync1 = CmdEntry {
            qw0: CmdSync::new()
                .with_opcode(CmdOpcode::CMD_SYNC.0)
                .with_cs(SyncCs::SIG_IRQ.0)
                .with_msi_data(0xBBBB)
                .into(),
            qw1: (SYNC_MSI_GPA >> 2) << 2,
        };
        write_cmd(&gm, cmd_idx, &sync1);
        cmd_idx += 1;

        write32(&mut dev, CMDQ_PROD, cmd_idx);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), cmd_idx);
        let sync_val: u32 = gm.read_plain(SYNC_MSI_GPA).expect("read sync MSI");
        assert_eq!(sync_val, 0xBBBB);

        // 2h. Program EVTQ.
        let evtq_base = QueueBase::new()
            .with_log2size(EVTQ_LOG2SIZE)
            .with_addr_bits(EVTQ_GPA >> 5);
        write64(&mut dev, EVENTQ_BASE, evtq_base.into());

        // Program EVTQ MSI config.
        write64(&mut dev, EVENTQ_IRQ_CFG0, EVTQ_MSI_GPA);
        write32(&mut dev, EVENTQ_IRQ_CFG1, 0xDEAD);

        // 2i. Enable EVTQEN.
        let cr0_evtqen = Cr0::new().with_cmdqen(true).with_eventqen(true);
        write32(&mut dev, CR0, cr0_evtqen.into());
        assert!(Cr0::from(read32(&mut dev, CR0ACK)).eventqen());

        // 2j. Enable EVENTQ IRQ.
        let irq_ctrl = IrqCtrl::new().with_eventq_irqen(true);
        write32(&mut dev, IRQ_CTRL, irq_ctrl.into());
        assert!(IrqCtrl::from(read32(&mut dev, IRQ_CTRLACK)).eventq_irqen());

        // 2k. Enable SMMUEN.
        let cr0_full = Cr0::new()
            .with_cmdqen(true)
            .with_eventqen(true)
            .with_smmuen(true);
        write32(&mut dev, CR0, cr0_full.into());
        let cr0ack = Cr0::from(read32(&mut dev, CR0ACK));
        assert!(cr0ack.smmuen(), "SMMUEN must be acknowledged");
        assert!(cr0ack.cmdqen());
        assert!(cr0ack.eventqen());

        // =====================================================================
        // Step 3: Attach device — configure STE and CD for stream ID
        // =====================================================================

        // 3a. Write STE: S1_TRANS mode, point to CD table at CD_GPA.
        let ste = Ste {
            qw0: SteDw0::new()
                .with_v(true)
                .with_config(SteConfig::S1_TRANS.0)
                .with_s1_context_ptr(CD_GPA >> 6)
                .with_s1_cd_max(0), // Single CD (SSID=0 only)
            qw1: SteDw1::new(),
            _qw2_7: [0u64; 6],
        };
        let ste_addr = STRTAB_GPA + (STREAM_ID as u64) * (STE_SIZE as u64);
        gm.write_plain(ste_addr, &ste).expect("write STE");

        // 3b. Write CD: TTB0 = PT_L1_GPA, T0SZ=32 (32-bit VA), 4K granule, 40-bit OAS.
        let cd = Cd {
            qw0: CdDw0::new()
                .with_v(true)
                .with_t0sz(32)
                .with_tg0(Tg0::GRAN_4K.0)
                .with_ips(Ips::IPS_40.0)
                .with_aa64(true)
                .with_a(true)
                .with_asid(1),
            qw1: CdDw1::new().with_ttb0(PT_L1_GPA >> 4),
            _qw2: 0,
            mair0: 0xFF440C0400,
            mair1: 0,
            _qw5_7: [0; 3],
        };
        let cd_addr = CD_GPA; // SSID=0
        gm.write_plain(cd_addr, &cd).expect("write CD");

        // 3c. Build page table hierarchy for DMA region:
        //     IOVA 0x0000_0000..0x0000_0FFF → DATA_GPA
        //     T0SZ=32, 4K granule → 3-level walk (L1, L2, L3).
        //
        // L1[0] → L2 table
        let l1_desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_addr_bits(PT_L2_GPA >> 12);
        gm.write_plain::<u64>(PT_L1_GPA, &l1_desc.into())
            .expect("write L1");

        // L2[0] → L3 table
        let l2_desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_addr_bits(PT_L3_GPA >> 12);
        gm.write_plain::<u64>(PT_L2_GPA, &l2_desc.into())
            .expect("write L2");

        // L3[0] → page at DATA_GPA (RW, AF=1)
        let l3_desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true) // L3: type=1 means page
            .with_af(true)
            .with_ap(ApBits::RW_EL1.0)
            .with_addr_bits(DATA_GPA >> 12);
        gm.write_plain::<u64>(PT_L3_GPA, &l3_desc.into())
            .expect("write L3[0]");

        // 3d. Build page table for doorbell region (for MSI translation):
        //     IOVA 0x0800_0000 → DOORBELL_GPA
        //     L1 index = 0x0800_0000 >> 30 = 0 (same L1 entry)
        //     L2 index = (0x0800_0000 >> 21) & 0x1FF = 64
        //     L3 index = (0x0800_0000 >> 12) & 0x1FF = 0
        //
        // We need a separate L2→L3 chain for L2[64].
        const DOORBELL_PT_L3_GPA: u64 = 0x50_4000;

        // L2[64] → doorbell L3 table
        let l2_doorbell_desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_addr_bits(DOORBELL_PT_L3_GPA >> 12);
        let l2_doorbell_offset = 64 * 8; // L2 index 64, 8 bytes per entry
        gm.write_plain::<u64>(PT_L2_GPA + l2_doorbell_offset, &l2_doorbell_desc.into())
            .expect("write L2[64]");

        // Doorbell L3[0] → page at DOORBELL_GPA
        let l3_doorbell_desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_af(true)
            .with_ap(ApBits::RW_EL1.0)
            .with_addr_bits(DOORBELL_GPA >> 12);
        gm.write_plain::<u64>(DOORBELL_PT_L3_GPA, &l3_doorbell_desc.into())
            .expect("write doorbell L3[0]");

        // 3e. Issue CFGI_STE + CFGI_CD + CMD_SYNC via CMDQ.
        let cfgi_ste = CmdEntry {
            qw0: CmdCfgiSte::new()
                .with_opcode(CmdOpcode::CFGI_STE.0)
                .with_sid(STREAM_ID)
                .into(),
            qw1: 0,
        };
        write_cmd(&gm, cmd_idx, &cfgi_ste);
        cmd_idx += 1;

        let cfgi_cd = CmdEntry {
            qw0: CmdCfgiCd::new()
                .with_opcode(CmdOpcode::CFGI_CD.0)
                .with_sid(STREAM_ID)
                .with_ssid(0)
                .into(),
            qw1: 0,
        };
        write_cmd(&gm, cmd_idx, &cfgi_cd);
        cmd_idx += 1;

        // Reset sync target.
        gm.write_at(SYNC_MSI_GPA, &0u32.to_le_bytes()).unwrap();

        let sync2 = CmdEntry {
            qw0: CmdSync::new()
                .with_opcode(CmdOpcode::CMD_SYNC.0)
                .with_cs(SyncCs::SIG_IRQ.0)
                .with_msi_data(0xCCCC)
                .into(),
            qw1: (SYNC_MSI_GPA >> 2) << 2,
        };
        write_cmd(&gm, cmd_idx, &sync2);
        cmd_idx += 1;

        write32(&mut dev, CMDQ_PROD, cmd_idx);

        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), cmd_idx, "All commands must be consumed");
        let sync_val: u32 = gm.read_plain(SYNC_MSI_GPA).expect("read sync MSI");
        assert_eq!(sync_val, 0xCCCC, "CFGI+SYNC completion must be signaled");

        // =====================================================================
        // Step 4: DMA — read/write through TranslatingMemory
        // =====================================================================

        // Create per-device wrappers.
        let shared_state = dev.shared_state().clone();
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(BUS, BUS);
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, smmu_msi) = {
            let translator = shared_state.translator(STREAM_ID_BASE);
            let gm_wrapper = iommu_common::TranslatingMemory::new_guest_memory(
                "smmu-translating",
                translator,
                bus_range,
                gm.clone(),
            );
            let msi = Arc::new(SmmuSignalMsi::new(
                shared_state.clone(),
                STREAM_ID_BASE,
                mock_msi.clone(),
            ));
            (gm_wrapper, msi)
        };

        // 4a. Write test data at DATA_GPA via raw guest memory.
        let test_data = b"Hello from SMMU end-to-end test!";
        gm.write_at(DATA_GPA, test_data).unwrap();

        // 4b. Read via IOVA → should get data from DATA_GPA.
        let mut buf = vec![0u8; test_data.len()];
        translating_gm
            .read_at(DMA_IOVA, &mut buf)
            .expect("DMA read through SMMU must succeed");
        assert_eq!(&buf, test_data, "Translated read must return correct data");

        // 4c. Write via IOVA with an offset.
        let write_data = b"DMA write OK";
        let write_offset = 0x100u64;
        translating_gm
            .write_at(DMA_IOVA + write_offset, write_data)
            .expect("DMA write through SMMU must succeed");

        // Verify at raw GPA.
        let mut verify_buf = vec![0u8; write_data.len()];
        gm.read_at(DATA_GPA + write_offset, &mut verify_buf)
            .unwrap();
        assert_eq!(
            &verify_buf, write_data,
            "Translated write must land at correct GPA"
        );

        // =====================================================================
        // Step 5: MSI — translate MSI address through SMMU
        // =====================================================================

        // Fire MSI with address = DOORBELL_IOVA + 0x40 (intra-page offset).
        // The SMMU should translate DOORBELL_IOVA → DOORBELL_GPA.
        // devid is a RID: (bus << 8 | devfn). Must be within the device's
        // assigned bus range for the SMMU to accept it.
        let device_rid = (BUS as u32) << 8; // devfn = 0
        smmu_msi.signal_msi(Some(device_rid), DOORBELL_IOVA + 0x40, 0x1234);

        let msi_calls = mock_msi.take_calls();
        assert_eq!(msi_calls.len(), 1, "Exactly one MSI must be forwarded");
        let (devid, addr, data) = &msi_calls[0];
        assert_eq!(*devid, Some(device_rid), "devid must be passed through");
        assert_eq!(
            *addr,
            DOORBELL_GPA + 0x40,
            "MSI address must be translated with offset"
        );
        assert_eq!(*data, 0x1234, "MSI data must be passed through");

        // =====================================================================
        // Step 6: Fault — access unmapped IOVA, verify EVTQ event
        // =====================================================================

        // IOVA 0x1000_0000 has no page table mapping → translation fault.
        let unmapped_iova: u64 = 0x1000_0000;
        let mut fault_buf = [0u8; 4];
        let result = translating_gm.read_at(unmapped_iova, &mut fault_buf);
        assert!(result.is_err(), "Read from unmapped IOVA must return error");

        // The fault event is queued in shared state. Trigger a drain by
        // writing CMDQ_PROD (which drains pending events).
        write32(&mut dev, CMDQ_PROD, cmd_idx); // No new commands, just drain.

        // Verify EVTQ_PROD advanced (an event was written).
        let evtq_prod = read32_page1(&mut dev, EVENTQ_PROD_PAGE1);
        assert!(evtq_prod > 0, "EVTQ must have at least one event");

        // Read the event from guest memory.
        let event: EvtEntry = gm.read_plain(EVTQ_GPA).expect("read fault event");
        assert_eq!(
            event.header.event_id(),
            EventId::F_TRANSLATION,
            "Fault must be a translation fault"
        );
        assert_eq!(event.sid, STREAM_ID, "Fault SID must match device");
        assert_eq!(
            event.input_addr, unmapped_iova,
            "Fault IOVA must match access"
        );
    }

    // =========================================================================
    // Save/Restore tests
    // =========================================================================

    /// Verifies that DMA translation through TranslatingMemory
    /// continues to work after a save/restore cycle.
    ///
    /// This tests the critical restore path: re-syncing SharedStateInner
    /// (enabled, strtab_base, strtab_log2size) and QueueErrorState from
    /// the restored register values. If any of these are missed, the
    /// translating memory wrapper — which holds the same Arc<SmmuSharedState>
    /// — will see stale state and translation will break.
    #[pal_async::async_test]
    async fn test_save_restore_translation_roundtrip() {
        use crate::spec::cd::Cd;
        use crate::spec::cd::CdDw0;
        use crate::spec::cd::CdDw1;
        use crate::spec::cd::Ips;
        use crate::spec::cd::Tg0;
        use crate::spec::pt::ApBits;
        use crate::spec::pt::PtDesc;
        use crate::spec::ste::STE_SIZE;
        use crate::spec::ste::Ste;
        use crate::spec::ste::SteConfig;
        use crate::spec::ste::SteDw0;
        use crate::spec::ste::SteDw1;
        use pci_core::bus_range::AssignedBusRange;

        const STRTAB_GPA: u64 = 0x10_0000;
        const STRTAB_LOG2SIZE: u8 = 10;
        const CD_GPA: u64 = 0x40_0000;
        const PT_L1_GPA: u64 = 0x50_1000;
        const PT_L2_GPA: u64 = 0x50_2000;
        const PT_L3_GPA: u64 = 0x50_3000;
        const DATA_GPA: u64 = 0x60_0000;
        const DMA_IOVA: u64 = 0x0000_0000;
        const BUS: u8 = 1;
        const STREAM_ID_BASE: u32 = 0;
        const STREAM_ID: u32 = (BUS as u32) << 8;

        let gm = GuestMemory::allocate(0x80_0000);
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm.clone(), &test_config(), None, None);

        // Set up stream table, CD, and page tables in guest memory.
        let ste = Ste {
            qw0: SteDw0::new()
                .with_v(true)
                .with_config(SteConfig::S1_TRANS.0)
                .with_s1_context_ptr(CD_GPA >> 6)
                .with_s1_cd_max(0),
            qw1: SteDw1::new(),
            _qw2_7: [0u64; 6],
        };
        let ste_addr = STRTAB_GPA + (STREAM_ID as u64) * (STE_SIZE as u64);
        gm.write_plain(ste_addr, &ste).unwrap();

        let cd = Cd {
            qw0: CdDw0::new()
                .with_v(true)
                .with_t0sz(32)
                .with_tg0(Tg0::GRAN_4K.0)
                .with_ips(Ips::IPS_40.0)
                .with_aa64(true)
                .with_a(true)
                .with_asid(1),
            qw1: CdDw1::new().with_ttb0(PT_L1_GPA >> 4),
            _qw2: 0,
            mair0: 0xFF440C0400,
            mair1: 0,
            _qw5_7: [0; 3],
        };
        gm.write_plain(CD_GPA, &cd).unwrap();

        // L1[0] → L2, L2[0] → L3, L3[0] → DATA_GPA
        let l1 = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_addr_bits(PT_L2_GPA >> 12);
        gm.write_plain::<u64>(PT_L1_GPA, &l1.into()).unwrap();
        let l2 = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_addr_bits(PT_L3_GPA >> 12);
        gm.write_plain::<u64>(PT_L2_GPA, &l2.into()).unwrap();
        let l3 = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_af(true)
            .with_ap(ApBits::RW_EL1.0)
            .with_addr_bits(DATA_GPA >> 12);
        gm.write_plain::<u64>(PT_L3_GPA, &l3.into()).unwrap();

        // Program SMMU registers: STRTAB_BASE, STRTAB_BASE_CFG, enable.
        write64(
            &mut dev,
            STRTAB_BASE,
            StrtabBase::new().with_addr_bits(STRTAB_GPA >> 6).into(),
        );
        write32(
            &mut dev,
            STRTAB_BASE_CFG,
            StrtabBaseCfg::new()
                .with_log2size(STRTAB_LOG2SIZE)
                .with_fmt(0)
                .into(),
        );
        write32(
            &mut dev,
            CR0,
            Cr0::new()
                .with_smmuen(true)
                .with_cmdqen(true)
                .with_eventqen(true)
                .into(),
        );

        // Create translating memory wrapper (holds Arc to same shared state).
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(BUS, BUS);
        let shared_state = dev.shared_state().clone();
        let translator = shared_state.translator(STREAM_ID_BASE);
        let translating_gm = iommu_common::TranslatingMemory::new_guest_memory(
            "smmu-translating",
            translator,
            bus_range,
            gm.clone(),
        );

        // Write test data and verify DMA read works.
        let test_data = b"save-restore-test";
        gm.write_at(DATA_GPA, test_data).unwrap();
        let mut buf = vec![0u8; test_data.len()];
        translating_gm.read_at(DMA_IOVA, &mut buf).unwrap();
        assert_eq!(&buf, test_data, "DMA must work before save");

        // Save.
        let saved = dev.save().expect("save must succeed");

        // Reset the device, as the state unit framework would between
        // save and restore (e.g., hibernate/migrate cycle). This clears
        // all register and shared state.
        dev.reset().await;

        // With SMMU disabled after reset, DMA bypasses translation
        // (IOVA = GPA). Reading at DMA_IOVA (0x0) should now return
        // whatever is at GPA 0x0 instead of DATA_GPA.
        gm.write_at(0, b"BYPASS!BYPASS!BYP").unwrap();
        let mut buf2 = vec![0u8; test_data.len()];
        translating_gm.read_at(DMA_IOVA, &mut buf2).unwrap();
        assert_eq!(
            &buf2, b"BYPASS!BYPASS!BYP",
            "after reset, DMA must bypass (read raw GPA)"
        );

        // Restore.
        dev.restore(saved).expect("restore must succeed");

        // DMA must work again through the same translating memory wrapper.
        let mut buf3 = vec![0u8; test_data.len()];
        translating_gm.read_at(DMA_IOVA, &mut buf3).unwrap();
        assert_eq!(&buf3, test_data, "DMA must work after restore");

        // Verify the SMMU is actually translating (not just bypassing).
        // Write different data at GPA 0x0 (the IOVA value). If the SMMU
        // is bypassing, we'd read this instead of DATA_GPA's contents.
        gm.write_at(0, b"BYPASS!BYPASS!BYP").unwrap();
        let mut buf4 = vec![0u8; test_data.len()];
        translating_gm.read_at(DMA_IOVA, &mut buf4).unwrap();
        assert_eq!(
            &buf4, test_data,
            "must read from DATA_GPA, not bypass to IOVA address"
        );
    }

    /// Verifies that a CMDQ error (split across cmdq_cons.err in the
    /// device and gerror/gerrorn in shared state) survives save/restore
    /// and continues to block command processing until acknowledged.
    #[pal_async::async_test]
    async fn test_save_restore_cmdq_error_persists() {
        let mut dev = make_cmdq_test_device();

        // Trigger CMDQ error with an unknown opcode.
        write_cmdq_entry(&dev, 0, &CmdEntry { qw0: 0xFF, qw1: 0 });
        write32(&mut dev, CMDQ_PROD, 1);

        // Verify error is active.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.err(), CmdqError::CERROR_ILL.0, "error must be set");
        assert_eq!(cons.rd(), 0, "CONS must not advance past error");
        let gerror = Gerror::from(read32(&mut dev, GERROR));
        assert!(gerror.cmdq_err(), "GERROR.CMDQ_ERR must be toggled");

        // Save, reset, and restore — matching the state-unit lifecycle
        // (hibernate/migration resets between save and restore).
        let saved = dev.save().expect("save");
        dev.reset().await;
        dev.restore(saved).expect("restore");

        // Error must still be active after restore.
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(
            cons.err(),
            CmdqError::CERROR_ILL.0,
            "error must survive restore"
        );
        let gerror = Gerror::from(read32(&mut dev, GERROR));
        assert!(gerror.cmdq_err(), "GERROR.CMDQ_ERR must survive restore");

        // Processing must still be blocked: write a valid command and
        // advance PROD, verify CONS doesn't advance.
        write_cmdq_entry(
            &dev,
            1,
            &CmdEntry {
                qw0: CmdOpcode::TLBI_NH_ALL.0 as u64,
                qw1: 0,
            },
        );
        write32(&mut dev, CMDQ_PROD, 2);
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(
            cons.rd(),
            0,
            "CMDQ must remain blocked until error is acknowledged"
        );

        // Acknowledge the error.
        write32(&mut dev, GERRORN, gerror.into());

        // Now the error should be cleared and processing should resume.
        assert!(
            !dev.shared_state.cmdq_err_active(),
            "error must be cleared after acknowledge"
        );
    }

    /// Per IHI 0070H.a §6.3.28, CMDQ_CONS is RW when CMDQEN==0 and
    /// CR0ACK.CMDQEN==0, allowing software to initialize it before
    /// enabling the queue. It becomes RO when CMDQEN==1.
    #[test]
    fn test_cmdq_cons_writable_when_disabled() {
        let gm = GuestMemory::allocate(0x40_0000);
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm.clone(), &test_config(), None, None);

        // CMDQEN is 0 at reset — CMDQ_CONS should be writable.
        assert!(!Cr0::from(read32(&mut dev, CR0)).cmdqen());

        // Write a non-zero value to CMDQ_CONS.
        write32(&mut dev, CMDQ_CONS, 0x05);
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(
            cons.rd(),
            0x05,
            "CMDQ_CONS.RD must accept writes when CMDQEN==0"
        );

        // Now enable CMDQEN — CMDQ_CONS should become read-only.
        let cmdq_base = QueueBase::new()
            .with_log2size(5)
            .with_addr_bits(0x20_0000u64 >> 5);
        write64(&mut dev, CMDQ_BASE, cmdq_base.into());
        // Re-init CONS to 0 before enabling (required by spec).
        write32(&mut dev, CMDQ_CONS, 0);
        write32(&mut dev, CMDQ_PROD, 0);
        write32(&mut dev, CR0, Cr0::new().with_cmdqen(true).into());
        assert!(Cr0::from(read32(&mut dev, CR0ACK)).cmdqen());

        // Writes to CMDQ_CONS while CMDQEN==1 must be ignored.
        write32(&mut dev, CMDQ_CONS, 0x10);
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(cons.rd(), 0, "CMDQ_CONS must be read-only when CMDQEN==1");
    }

    /// Per IHI 0070H.a §4.7.3, CMD_SYNC with CS=0b11 is reserved and
    /// must cause CERROR_ILL. The SMMU should stop consuming commands
    /// and toggle GERROR.CMDQ_ERR.
    #[test]
    fn test_cmd_sync_reserved_cs_causes_cerror_ill() {
        use crate::spec::commands::CmdEntry;
        use crate::spec::commands::CmdOpcode;
        use crate::spec::commands::CmdSync;

        let gm = GuestMemory::allocate(0x40_0000);
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm.clone(), &test_config(), None, None);

        const CMDQ_GPA: u64 = 0x20_0000;

        // Set up CMDQ.
        let cmdq_base = QueueBase::new()
            .with_log2size(5)
            .with_addr_bits(CMDQ_GPA >> 5);
        write64(&mut dev, CMDQ_BASE, cmdq_base.into());
        write32(&mut dev, CMDQ_PROD, 0);
        write32(&mut dev, CMDQ_CONS, 0);

        // Enable CMDQ.
        write32(&mut dev, CR0, Cr0::new().with_cmdqen(true).into());
        assert!(Cr0::from(read32(&mut dev, CR0ACK)).cmdqen());

        // Write a CMD_SYNC with CS=0b11 (reserved).
        let bad_sync = CmdEntry {
            qw0: CmdSync::new()
                .with_opcode(CmdOpcode::CMD_SYNC.0)
                .with_cs(0b11) // Reserved — must cause CERROR_ILL
                .into(),
            qw1: 0,
        };
        let cmd_addr = CMDQ_GPA;
        gm.write_plain(cmd_addr, &bad_sync).expect("write cmd");

        // Advance PROD to trigger processing.
        write32(&mut dev, CMDQ_PROD, 1);

        // GERROR.CMDQ_ERR should now be active (toggled != GERRORN).
        let gerror = Gerror::from(read32(&mut dev, GERROR));
        let gerrorn = Gerror::from(read32(&mut dev, GERRORN));
        assert_ne!(
            gerror.cmdq_err(),
            gerrorn.cmdq_err(),
            "GERROR.CMDQ_ERR must be active after CS=0b11"
        );

        // CMDQ_CONS.ERR must be CERROR_ILL (1).
        let cons = CmdqCons::from(read32(&mut dev, CMDQ_CONS));
        assert_eq!(
            cons.err(),
            CmdqError::CERROR_ILL.0,
            "CMDQ_CONS.ERR must be CERROR_ILL"
        );
    }
}
