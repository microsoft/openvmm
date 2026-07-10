// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 device emulator — register file and MMIO dispatch.

use crate::shared::SmmuSharedState;
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

/// SMMUv3 device configuration.
#[derive(Debug, Clone)]
pub struct SmmuConfig {
    /// Number of StreamID bits (max 32, typically 16).
    pub sidsize: u8,
    /// Output address size in bits (e.g., 40 for 40-bit physical addresses).
    /// Must be one of: 32, 36, 40, 42, 44, 48, 52.
    pub oas: u8,
}

impl Default for SmmuConfig {
    fn default() -> Self {
        Self {
            sidsize: 16,
            oas: 40,
        }
    }
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
    cr0ack: registers::Cr0,
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
            .with_ttf(0b10) // AArch64 only
            .with_cohacc(true)
            .with_asid16(true)
            .with_msi(false)
            .with_ttendian(0b10) // Little-endian
            .with_stall_model(0b01) // Stall not supported
            .with_term_model(true) // Terminate faults (no stall)
            .with_st_level(0b00); // Linear stream table only

        let idr1 = registers::Idr1::new()
            .with_sidsize(config.sidsize)
            .with_ssidsize(0)
            .with_cmdqs(8) // 256 entries max
            .with_eventqs(8) // 256 entries max
            .with_attr_types_ovr(true)
            .with_tables_preset(false)
            .with_queues_preset(false)
            .with_rel(false);

        let idr5 = registers::Idr5::new()
            .with_oas(crate::spec::cd::Ips::from_bits(config.oas).0)
            .with_gran4k(true)
            .with_gran16k(false)
            .with_gran64k(false);

        // GBPA defaults to ABORT=1 (abort all transactions when SMMU is disabled).
        let gbpa = registers::Gbpa::new().with_abort(true);

        let shared_state =
            SmmuSharedState::new(guest_memory.clone(), config.oas, evtq_irq, gerror_irq);

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
            cr0ack: registers::Cr0::new(),
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
            registers::IDR5 => self.idr5.into(),
            registers::IIDR => self.iidr,
            registers::AIDR => self.aidr,

            registers::CR0 => self.cr0.into(),
            registers::CR0ACK => self.cr0ack.into(),
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
                self.cr0 = registers::Cr0::from(value);
                // Immediate acknowledge — no async enable sequence.
                self.cr0ack = self.cr0;
                // Sync enable state to shared state for per-device wrappers.
                self.shared_state.set_enabled(self.cr0.smmuen());
                self.shared_state.set_evtq_enabled(self.cr0.eventqen());
            }
            registers::CR1 => {
                self.cr1 = registers::Cr1::from(value);
            }
            registers::CR2 => {
                self.cr2 = registers::Cr2::from(value);
            }
            registers::GBPA => {
                // Clear the UPDATE bit on write (the SMMU "completes" the
                // update immediately).
                let mut gbpa = registers::Gbpa::from(value);
                gbpa.set_update(false);
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

    /// Process all pending commands in the command queue.
    ///
    /// Called when the guest writes CMDQ_PROD. Consumes commands from
    /// CMDQ_CONS up to CMDQ_PROD, dispatching each by opcode.
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
                    // Set CMDQ error: abort.
                    self.set_cmdq_error(registers::CmdqError::CERROR_ABT);
                    break;
                }
            };

            match entry.opcode() {
                // Configuration invalidation commands — no-op (no cache yet).
                CmdOpcode::PREFETCH_CFG
                | CmdOpcode::CFGI_STE
                | CmdOpcode::CFGI_STE_RANGE
                | CmdOpcode::CFGI_CD
                | CmdOpcode::CFGI_CD_ALL => {}

                // TLB invalidation commands — no-op (no TLB yet).
                CmdOpcode::TLBI_NH_ALL
                | CmdOpcode::TLBI_NH_ASID
                | CmdOpcode::TLBI_NH_VA
                | CmdOpcode::TLBI_NH_VAA
                | CmdOpcode::TLBI_S12_VMALL
                | CmdOpcode::TLBI_NSNH_ALL => {}

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

        // Update the stored CMDQ_CONS (preserve error field, update rd).
        self.cmdq_cons.set_rd(cons);
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
            cr0ack,
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

            // Event queue base register.
            evtq_base,

            // MSI configuration.
            gerror_msi,
            evtq_msi,
            cmdq_msi,
        } = self;

        *cr0 = registers::Cr0::new();
        *cr0ack = registers::Cr0::new();
        *cr1 = registers::Cr1::new();
        *cr2 = registers::Cr2::new();
        *gbpa = registers::Gbpa::new().with_abort(true);

        *irq_ctrl = registers::IrqCtrl::new();
        *irq_ctrlack = registers::IrqCtrl::new();

        *strtab_base = 0;
        *strtab_base_cfg = registers::StrtabBaseCfg::new();

        *cmdq_base = 0;
        *cmdq_prod = 0;
        *cmdq_cons = registers::CmdqCons::new();

        *evtq_base = 0;

        *gerror_msi = MsiConfig::default();
        *evtq_msi = MsiConfig::default();
        *cmdq_msi = MsiConfig::default();

        // Sync disabled state to shared state so per-device wrappers
        // bypass translation immediately.
        shared_state.set_enabled(false);
        shared_state.set_strtab(0, 0);
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
            cr0ack: _, // mirror of cr0 (immediate ack)
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

        self.cr0 = registers::Cr0::from(cr0);
        self.cr0ack = self.cr0; // immediate ack
        self.cr1 = registers::Cr1::from(cr1);
        self.cr2 = registers::Cr2::from(cr2);
        self.gbpa = registers::Gbpa::from(gbpa);

        self.irq_ctrl = registers::IrqCtrl::from(irq_ctrl);
        self.irq_ctrlack = self.irq_ctrl; // immediate ack

        self.strtab_base = strtab_base;
        self.strtab_base_cfg = registers::StrtabBaseCfg::from(strtab_base_cfg);

        self.cmdq_base = cmdq_base;
        self.cmdq_prod = cmdq_prod;
        self.cmdq_cons = registers::CmdqCons::from(cmdq_cons);

        self.evtq_base = evtq_base;

        self.gerror_msi = gerror_msi.restore();
        self.evtq_msi = evtq_msi.restore();
        self.cmdq_msi = cmdq_msi.restore();

        // Re-sync derived state in SmmuSharedState.
        self.shared_state.set_enabled(self.cr0.smmuen());
        self.sync_strtab_to_shared();
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

    fn make_test_device() -> SmmuDevice {
        let gm = GuestMemory::empty();
        SmmuDevice::new(TEST_MMIO_BASE, gm, &SmmuConfig::default(), None, None)
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

        // IDR1: SIDSIZE=16, CMDQS=8, EVTQS=8, ATTR_TYPES_OVR=1
        let idr1 = Idr1::from(read32(&mut dev, IDR1));
        assert_eq!(idr1.sidsize(), 16);
        assert_eq!(idr1.cmdqs(), 8);
        assert_eq!(idr1.eventqs(), 8);
        assert!(idr1.attr_types_ovr());
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

    /// Create a device with real guest memory and a configured CMDQ.
    fn make_cmdq_test_device() -> SmmuDevice {
        // Allocate enough guest memory for CMDQ + MSI target page.
        let gm = GuestMemory::allocate(0x4_0000);
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm, &SmmuConfig::default(), None, None);

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
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm, &SmmuConfig::default(), None, None);

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
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm, &SmmuConfig::default(), None, None);

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
        let mut dev = SmmuDevice::new(TEST_MMIO_BASE, gm, &SmmuConfig::default(), None, None);

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
            written.event_id(),
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
        let mut dev = SmmuDevice::new(
            TEST_MMIO_BASE,
            gm.clone(),
            &SmmuConfig::default(),
            None,
            None,
        );

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
            event.event_id(),
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
        let mut dev = SmmuDevice::new(
            TEST_MMIO_BASE,
            gm.clone(),
            &SmmuConfig::default(),
            None,
            None,
        );

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
        let mut dev = SmmuDevice::new(
            TEST_MMIO_BASE,
            gm.clone(),
            &SmmuConfig::default(),
            None,
            None,
        );

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
        let mut dev = SmmuDevice::new(
            TEST_MMIO_BASE,
            gm.clone(),
            &SmmuConfig::default(),
            None,
            None,
        );

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
