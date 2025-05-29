// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 440BX Host to PCI Bridge

pub use pam::GpaState;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use open_enum::open_enum;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use vmcore::device_state::ChangeDeviceState;

/// A trait to create GPA alias ranges.
pub trait AdjustGpaRange: Send {
    /// Adjusts a memory range's mapping state.
    ///
    /// This will only be called for memory ranges supported by the i440BX PAM
    /// registers, or for VGA memory.
    fn adjust_gpa_range(&mut self, range: MemoryRange, state: GpaState);
}

struct HostPciBridgeRuntime {
    adjust_gpa_range: Box<dyn AdjustGpaRange>,
}

/// 440BX Host to PCI Bridge
///
/// See section 3.3 in the 440BX data sheet.
#[derive(InspectMut)]
pub struct HostPciBridge {
    // Runtime glue
    #[inspect(skip)]
    rt: HostPciBridgeRuntime,

    // Sub-emulators
    cfg_space: ConfigSpaceType0Emulator,

    // Volatile state
    state: HostPciBridgeState,
}

#[derive(Debug, Inspect)]
struct HostPciBridgeState {
    host_pci_dram1: u32,
    host_pci_dram2: u32,
    pam_reg1: u32,
    pam_reg2: u32,
    bios_scratch1: u32,
    bios_scratch2: u32,
    smm_config_word: u16,
}

// All unmapped.
const INITIAL_PAM_REG1: u32 = 0x00000003;
const INITIAL_PAM_REG2: u32 = 0;

impl HostPciBridgeState {
    fn new() -> Self {
        Self {
            // magic numbers lifted straight from Hyper-V source code
            host_pci_dram1: 0x02020202,
            host_pci_dram2: 0x00000002,
            pam_reg1: INITIAL_PAM_REG1,
            pam_reg2: INITIAL_PAM_REG2,
            bios_scratch1: 0,
            bios_scratch2: 0,
            smm_config_word: 0x3802,
        }
    }
}

impl HostPciBridge {
    pub fn new(adjust_gpa_range: Box<dyn AdjustGpaRange>, is_restoring: bool) -> Self {
        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x8086,
                device_id: 0x7192,
                revision_id: 0x03,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::BRIDGE_HOST,
                base_class: ClassCode::BRIDGE,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            Vec::new(),
            DeviceBars::new(),
        );

        let mut dev = Self {
            rt: HostPciBridgeRuntime { adjust_gpa_range },

            cfg_space,

            state: HostPciBridgeState::new(),
        };

        if !is_restoring {
            // Hard code VGA decoding to on. We don't support the register used to
            // control this, and the BIOS doesn't try to set it.
            dev.rt
                .adjust_gpa_range
                .adjust_gpa_range(MemoryRange::new(0xa0000..0xc0000), GpaState::Mmio);

            dev.adjust_bios_override_ranges(dev.state.pam_reg1, dev.state.pam_reg2, true);
        }

        dev
    }
}

impl HostPciBridge {
    // This routine is called when the PAM (physical address management) PCI
    // configuration registers are modified.
    //
    // It gives us a chance to adjust the physical mappings for the addresses
    // corresponding to the system BIOS (E0000-FFFFF).
    fn adjust_bios_override_ranges(&mut self, new_reg1: u32, new_reg2: u32, force: bool) {
        tracing::trace!(?self.state.pam_reg1, ?self.state.pam_reg2, new_reg1, new_reg2, "updating PAM registers");

        let old = pam::parse_pam_registers(self.state.pam_reg1, self.state.pam_reg2);
        let new = pam::parse_pam_registers(new_reg1, new_reg2);

        for ((range, old_state), (_, new_state)) in old.zip(new) {
            if old_state != new_state || force {
                self.rt.adjust_gpa_range.adjust_gpa_range(range, new_state);
            }
        }

        self.state.pam_reg1 = new_reg1;
        self.state.pam_reg2 = new_reg2;
    }
}

impl ChangeDeviceState for HostPciBridge {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.cfg_space.reset();
        self.state = HostPciBridgeState::new();

        self.adjust_bios_override_ranges(INITIAL_PAM_REG1, INITIAL_PAM_REG2, true);
    }
}

impl ChipsetDevice for HostPciBridge {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for HostPciBridge {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        *value = match ConfigSpace(offset) {
            // for bug-for-bug compat with the hyper-v implementation: return
            // hardcoded status register instead of letting the config space
            // emulator take care of it
            _ if offset == pci_core::spec::cfg_space::HeaderType00::STATUS_COMMAND.0 => 0x02000006,
            _ if offset < 0x40 => return self.cfg_space.read_u32(offset, value),
            ConfigSpace::PAM1 => self.state.pam_reg1,
            ConfigSpace::PAM2 => self.state.pam_reg2,
            ConfigSpace::DRAM1 => self.state.host_pci_dram1,
            ConfigSpace::DRAM2 => self.state.host_pci_dram2,
            // Specify the default value: No AGP, fast CPU startup
            ConfigSpace::PAGING_POLICY => 0x380A0000,
            ConfigSpace::BIOS_SCRATCH1 => self.state.bios_scratch1,
            ConfigSpace::BIOS_SCRATCH2 => self.state.bios_scratch2,
            ConfigSpace::SYS_MNG => {
                // Bits 7 and 2, 0 are always clear.
                // Bit 13-12, 1 are always set.
                (self.state.smm_config_word as u32 & 0xC77C | 0x3802) << 16
            }
            ConfigSpace::MANUFACTURER_ID => 0x00000F20,
            ConfigSpace::BUFFER_CONTROL
            | ConfigSpace::SDRAM_CONTROL
            | ConfigSpace::CACHE
            | ConfigSpace::DRAM_C
            | ConfigSpace::DRAM_RT1
            | ConfigSpace::UNKNOWN_F4 => 0, // Hyper-V always returns 0, so do we.
            _ => {
                tracing::debug!(?offset, "unimplemented config space read");
                return IoResult::Err(IoError::InvalidRegister);
            }
        };

        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match ConfigSpace(offset) {
            _ if offset < 0x40 => return self.cfg_space.write_u32(offset, value),
            ConfigSpace::DRAM1 => self.state.host_pci_dram1 = value,
            ConfigSpace::DRAM2 => self.state.host_pci_dram2 = value,
            ConfigSpace::PAM1 => {
                self.adjust_bios_override_ranges(value, self.state.pam_reg2, false);
            }
            ConfigSpace::PAM2 => {
                self.adjust_bios_override_ranges(self.state.pam_reg1, value, false);
            }
            ConfigSpace::BIOS_SCRATCH1 => self.state.bios_scratch1 = value,
            ConfigSpace::BIOS_SCRATCH2 => self.state.bios_scratch2 = value,
            ConfigSpace::SYS_MNG => {
                // Configuration registers 70-71 are reserved. Only 72-73 (the top 16
                // bits of this four-byte range) are defined. We'll therefore shift
                // off the bottom portion.
                let mut new_smm_word = (value >> 16) as u16;

                // If the register is "locked" (i.e. bit 4 has been set), then
                // all of the other bits become read-only.
                if self.state.smm_config_word & 0x10 == 0 {
                    // Make sure they aren't enabling features we don't currently support.
                    if new_smm_word & 0x8700 != 0 {
                        tracelimit::warn_ratelimited!(bits = ?new_smm_word & !0x8700, "guest set unsupported feature bits");
                    }

                    new_smm_word &= !0x8700;
                    // Bits 7 and 2, 0 are always clear.
                    new_smm_word &= 0xC77C;
                    // Bit 13-12, 1 are always set.
                    new_smm_word |= 0x3802;
                    // We never set bit 14 that indicates that SMM memory was accessed
                    // by the CPU when not in SMM mode.
                    new_smm_word &= !0x4000;

                    // Make sure no one is trying to enable SMM RAM.
                    if new_smm_word & 0x0040 != 0 {
                        tracelimit::warn_ratelimited!("guest attempted to enable SMM RAM");
                    }
                    new_smm_word &= !0x0040;

                    self.state.smm_config_word = new_smm_word;
                }
            }
            ConfigSpace::BUFFER_CONTROL
            | ConfigSpace::SDRAM_CONTROL
            | ConfigSpace::CACHE
            | ConfigSpace::DRAM_C
            | ConfigSpace::DRAM_RT1
            | ConfigSpace::PAGING_POLICY
            | ConfigSpace::UNKNOWN_F4 => {} // Hyper-V ignores these, so do we.
            _ => {
                tracing::debug!(?offset, ?value, "unimplemented config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        Some((0, 0, 0)) // as per i440bx spec
    }
}

open_enum! {
    enum ConfigSpace: u16 {
        CACHE           = 0x50,
        DRAM_C          = 0x54,
        TIMING          = 0x58,
        PAM1            = 0x58,
        PAM2            = 0x5C,
        DRAM1           = 0x60,
        DRAM2           = 0x64,
        DRAM_RT1        = 0x68,
        DRAM_RT2        = 0x6C,
        SYS_MNG         = 0x70,
        SDRAM_CONTROL   = 0x74,
        PAGING_POLICY   = 0x78,
        SUSPEND_CBR     = 0x7C, // Register spans 7B-7C
        MEM_BUFF_FREQ   = 0xCC,
        BIOS_SCRATCH1   = 0xD0,
        BIOS_SCRATCH2   = 0xD4,
        BUFFER_CONTROL  = 0xF0,
        UNKNOWN_F4      = 0xF4,
        MANUFACTURER_ID = 0xF8,
    }
}

mod pam {
    use memory_range::MemoryRange;

    #[derive(Default, Copy, Clone, Debug, PartialEq, Eq)]
    pub enum GpaState {
        /// Reads and writes go to RAM.
        #[default]
        Writable,
        /// Reads go to RAM, writes go to MMIO.
        WriteProtected,
        /// Reads go to ROM, writes go to RAM.
        WriteOnly,
        /// Reads and writes go to MMIO.
        Mmio,
    }

    pub const PAM_RANGES: &[MemoryRange; 13] = &[
        MemoryRange::new(0xf0000..0x100000),
        MemoryRange::new(0xc0000..0xc4000),
        MemoryRange::new(0xc4000..0xc8000),
        MemoryRange::new(0xc8000..0xcc000),
        MemoryRange::new(0xcc000..0xd0000),
        MemoryRange::new(0xd0000..0xd4000),
        MemoryRange::new(0xd4000..0xd8000),
        MemoryRange::new(0xd8000..0xdc000),
        MemoryRange::new(0xdc000..0xe0000),
        MemoryRange::new(0xe0000..0xe4000),
        MemoryRange::new(0xe4000..0xe8000),
        MemoryRange::new(0xe8000..0xec000),
        MemoryRange::new(0xec000..0xf0000),
    ];

    pub fn parse_pam_registers(
        reg1: u32,
        reg2: u32,
    ) -> impl Iterator<Item = (MemoryRange, GpaState)> {
        // Grab the two PAM (physical address management) registers which
        // consist of 16 four-bit fields. We never look at the first two bits
        // of these fields. The second two bits encode the following:
        //    xx00    => Rom only mapping (shadow RAM is inaccessible)
        //    xx01    => Read-only RAM (writes go to Rom and are ignored)
        //    xx10    => Write-only RAM (reads come from Rom - not supported by us)
        //    xx11    => RAM-only (Rom is inaccessible)
        let reg = ((reg2 as u64) << 32) | reg1 as u64;
        PAM_RANGES.iter().enumerate().map(move |(i, range)| {
            let state = match (reg >> ((i + 3) * 4)) & 3 {
                0b00 => GpaState::Mmio,
                0b01 => GpaState::WriteProtected,
                0b10 => GpaState::WriteOnly,
                0b11 => GpaState::Writable,
                _ => unreachable!(),
            };
            (*range, state)
        })
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.i440bx.host_pci_bridge")]
        pub struct SavedState {
            #[mesh(1)]
            pub host_pci_dram1: u32,
            #[mesh(2)]
            pub host_pci_dram2: u32,
            #[mesh(3)]
            pub pam_reg1: u32,
            #[mesh(4)]
            pub pam_reg2: u32,
            #[mesh(5)]
            pub bios_scratch1: u32,
            #[mesh(6)]
            pub bios_scratch2: u32,
            #[mesh(7)]
            pub smm_config_word: u16,
            #[mesh(8)]
            pub cfg_space: <ConfigSpaceType0Emulator as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for HostPciBridge {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let HostPciBridgeState {
                host_pci_dram1,
                host_pci_dram2,
                pam_reg1,
                pam_reg2,
                bios_scratch1,
                bios_scratch2,
                smm_config_word,
            } = self.state;

            Ok(state::SavedState {
                host_pci_dram1,
                host_pci_dram2,
                pam_reg1,
                pam_reg2,
                bios_scratch1,
                bios_scratch2,
                smm_config_word,
                cfg_space: self.cfg_space.save()?,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                host_pci_dram1,
                host_pci_dram2,
                pam_reg1,
                pam_reg2,
                bios_scratch1,
                bios_scratch2,
                smm_config_word,
                cfg_space,
            } = state;

            self.state = HostPciBridgeState {
                host_pci_dram1,
                host_pci_dram2,
                pam_reg1,
                pam_reg2,
                bios_scratch1,
                bios_scratch2,
                smm_config_word,
            };

            self.adjust_bios_override_ranges(pam_reg1, pam_reg2, true);

            self.cfg_space.restore(cfg_space)?;

            Ok(())
        }
    }
}
