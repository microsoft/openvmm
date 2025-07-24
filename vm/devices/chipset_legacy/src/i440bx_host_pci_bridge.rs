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
            ConfigSpace::DRB_1 => self.state.host_pci_dram1,
            ConfigSpace::DRB_2 => self.state.host_pci_dram2,
            // Specify the default value: No AGP, fast CPU startup,
            // and default low byte of SCRR in our top byte.
            ConfigSpace::PGPOL => 0x380A0000,
            ConfigSpace::BSPAD_1 => self.state.bios_scratch1,
            ConfigSpace::BSPAD_2 => self.state.bios_scratch2,
            ConfigSpace::SMRAM => {
                // Bits 7, 2 & 0 are always clear.
                // Bit 13-11 & 1 are always set.
                ((self.state.smm_config_word & 0b01111010 | 0b00111000_00000010) as u32) << 16
            }
            ConfigSpace::MANUFACTURER_ID => 0x00000F20,
            ConfigSpace::BUFFC
            | ConfigSpace::SDRAMC
            | ConfigSpace::NBXCFG
            | ConfigSpace::DRAMC
            | ConfigSpace::MBSC_1
            | ConfigSpace::SCRR_2
            | ConfigSpace::ERR
            | ConfigSpace::ACAPID
            | ConfigSpace::AGPSTAT
            | ConfigSpace::AGPCMD
            | ConfigSpace::AGPCTRL
            | ConfigSpace::APSIZE
            | ConfigSpace::ATTBASE
            | ConfigSpace::UNKNOWN_BC
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
            ConfigSpace::DRB_1 => self.state.host_pci_dram1 = value,
            ConfigSpace::DRB_2 => self.state.host_pci_dram2 = value,
            ConfigSpace::PAM1 => {
                self.adjust_bios_override_ranges(value, self.state.pam_reg2, false);
            }
            ConfigSpace::PAM2 => {
                self.adjust_bios_override_ranges(self.state.pam_reg1, value, false);
            }
            ConfigSpace::BSPAD_1 => self.state.bios_scratch1 = value,
            ConfigSpace::BSPAD_2 => self.state.bios_scratch2 = value,
            ConfigSpace::SMRAM => {
                // Configuration registers 70-71 are reserved. Only 72-73 (the top 16
                // bits of this four-byte range) are defined. We'll therefore shift
                // off the bottom portion.
                let mut new_smm_word = (value >> 16) as u16;

                // If the register is "locked" (i.e. bit 4 has been set), then
                // all of the other bits become read-only.
                if self.state.smm_config_word & 0x10 == 0 {
                    // Make sure they aren't enabling features we don't currently support.
                    const UNSUPPORTED_BITS: u16 = 0b10000111_00000000;
                    if new_smm_word & UNSUPPORTED_BITS != 0 {
                        tracelimit::warn_ratelimited!(
                            bits = new_smm_word & !UNSUPPORTED_BITS,
                            "guest set unsupported feature bits"
                        );
                    }

                    new_smm_word &= !UNSUPPORTED_BITS;
                    // Bits 7, 2 & 0 are always clear.
                    new_smm_word &= 0b01111010;
                    // Bit 13-11 & 1 are always set.
                    new_smm_word |= 0b00111000_00000010;
                    // We never set bit 14 that indicates that SMM memory was accessed
                    // by the CPU when not in SMM mode.
                    new_smm_word &= !0b01000000_00000000;

                    // Make sure no one is trying to enable SMM RAM.
                    if new_smm_word & 0b01000000 != 0 {
                        tracelimit::warn_ratelimited!("guest attempted to enable SMM RAM");
                    }
                    new_smm_word &= !0b01000000;

                    self.state.smm_config_word = new_smm_word;
                }
            }
            ConfigSpace::BUFFC
            | ConfigSpace::SDRAMC
            | ConfigSpace::NBXCFG
            | ConfigSpace::DRAMC
            | ConfigSpace::MBSC_1
            | ConfigSpace::PGPOL
            | ConfigSpace::SCRR_2
            | ConfigSpace::ERR
            | ConfigSpace::ACAPID
            | ConfigSpace::AGPSTAT
            | ConfigSpace::AGPCMD
            | ConfigSpace::AGPCTRL
            | ConfigSpace::APSIZE
            | ConfigSpace::ATTBASE
            | ConfigSpace::UNKNOWN_BC
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
    /// Note that all accesses will be 4-byte aligned, so this enum sets values
    /// to the expected offsets we will receive. When the actual register is not
    /// a full 4 bytes aligned to 4 bytes it is documented here.
    enum ConfigSpace: u16 {
        NBXCFG          = 0x50,
        /// Only comprises offset 0x57.
        DRAMC           = 0x54,
        /// Only comprises offset 0x58.
        DRAMT           = 0x58,
        /// Comprises offsets 0x59-0x5B.
        PAM1            = 0x58,
        PAM2            = 0x5C,
        DRB_1           = 0x60,
        DRB_2           = 0x64,
        /// Only comprises offset 0x68.
        FDHC            = 0x68,
        /// Comprises offsets 0x69-0x6B.
        MBSC_1          = 0x68,
        /// Comprises offsets 0x6C-0x6E.
        MBSC_2          = 0x6C,
        /// Comprises offsets 0x72-0x73.
        SMRAM           = 0x70,
        SDRAMC          = 0x74,
        /// Comprises offsets 0x78-0x7A.
        PGPOL           = 0x78,
        /// Only comprises offset 0x7B.
        SCRR_1          = 0x78,
        /// Only comprises offset 0x7C.
        SCRR_2          = 0x7C,
        /// Comprises offsets 0x90-0x92.
        ERR             = 0x90,
        ACAPID          = 0xA0,
        AGPSTAT         = 0xA4,
        AGPCMD          = 0xA8,
        AGPCTRL         = 0xB0,
        /// Only comprises offset 0xB4.
        APSIZE          = 0xB4,
        ATTBASE         = 0xB8,
        /// Documented as Reserved.
        UNKNOWN_BC      = 0xBC,
        MBFS            = 0xCC,
        BSPAD_1         = 0xD0,
        BSPAD_2         = 0xD4,
        /// Comprises offsets 0xF0-0xF1.
        BUFFC           = 0xF0,
        /// Documented as Intel Reserved.
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
