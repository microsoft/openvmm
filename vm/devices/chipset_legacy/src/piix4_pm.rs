// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PIIX4 - Power Management

use chipset::pm::PmTimerAssist;
use chipset::pm::PowerAction;
use chipset::pm::PowerActionFn;
use chipset::pm::PowerManagementDevice;
use chipset_device::ChipsetDevice;
use chipset_device::interrupt::LineInterruptTarget;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use inspect::Inspect;
use inspect::InspectMut;
use open_enum::open_enum;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::vmtime::VmTimeAccess;

/// IO ports used in the legacy Hyper-V implementation
pub mod io_ports {
    // TODO: add an assert during PM construction that enforces this \/
    // N.B. PM_BASE must be a multiple of 0x100 since the PM device looks at the bottom byte
    // to determine the offset. It also must be >= 0x100 so that it doesn't overlap
    // with the status or control port.
    //
    // N.B. Note that these values must also match what is reported in UEFI, as these
    // values are also reported in the FADT.
    // MsvmPkg: PowerManagementInterface.h
    // MsvmPkg: Fadt.aslc
    pub const DEFAULT_DYN_BASE: u16 = 0x400;

    pub const CONTROL_PORT: u16 = 0xB2;
    pub const STATUS_PORT: u16 = 0xB3;
}

#[derive(Debug)]
enum StaticReg {
    Control,
    Status,
}

struct Piix4PmRt {
    pio_static_control: Box<dyn ControlPortIoIntercept>,
    pio_static_status: Box<dyn ControlPortIoIntercept>,
}

impl std::fmt::Debug for Piix4PmRt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Piix4PmRt").finish()
    }
}

#[derive(Debug, Inspect)]
struct Piix4PmState {
    power_status: u8,
    power_control: u8,

    smbus_io_enabled: bool,
    #[inspect(hex)]
    base_io_addr: u16,
    base_io_enable: bool,
    counter_info_a: u32,
    counter_info_b: u32,
    general_purpose_config_info: u32,
    #[inspect(hex, iter_by_index)]
    device_resource_flags: [u32; 10],
    #[inspect(hex, iter_by_index)]
    device_activity_flags: [u32; 2],
}

impl Piix4PmState {
    fn new() -> Self {
        Self {
            power_status: 0,
            power_control: 0,

            smbus_io_enabled: false,
            base_io_addr: 0,
            base_io_enable: false,
            counter_info_a: 0,
            counter_info_b: 0,
            general_purpose_config_info: 0,
            device_resource_flags: [0; 10],
            device_activity_flags: [0; 2],
        }
    }
}

/// PIIX4 (PCI device function 3) - Power Management
///
/// See section 3.4 in the PIIX4 data sheet.
#[derive(InspectMut)]
pub struct Piix4Pm {
    // Runtime glue
    #[inspect(skip)]
    rt: Piix4PmRt,

    // Sub-emulators
    #[inspect(mut)]
    inner: PowerManagementDevice,
    cfg_space: ConfigSpaceType0Emulator,

    // Volatile state
    state: Piix4PmState,
}

impl Piix4Pm {
    pub fn new(
        power_action: PowerActionFn,
        interrupt: LineInterrupt,
        register_pio: &mut dyn RegisterPortIoIntercept,
        vmtime: VmTimeAccess,
        pm_timer_assist: Option<Box<dyn PmTimerAssist>>,
    ) -> Self {
        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x8086,
                device_id: 0x7113,
                revision_id: 0x02,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::BRIDGE_OTHER,
                base_class: ClassCode::BRIDGE,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            Vec::new(),
            DeviceBars::new(),
        );

        let mut pio_static_control = register_pio.new_io_region("control", 1);
        let mut pio_static_status = register_pio.new_io_region("status", 1);

        pio_static_control.map(io_ports::CONTROL_PORT);
        pio_static_status.map(io_ports::STATUS_PORT);

        Self {
            inner: PowerManagementDevice::new(
                power_action,
                interrupt,
                register_pio,
                vmtime,
                None, // manually configured
                pm_timer_assist,
            ),
            cfg_space,
            rt: Piix4PmRt {
                pio_static_control,
                pio_static_status,
            },
            state: Piix4PmState::new(),
        }
    }

    fn update_io_mappings(&mut self) {
        if self.state.base_io_enable && self.state.base_io_addr != 0 {
            self.inner
                .update_dynamic_pio_mappings(Some(self.state.base_io_addr))
        } else {
            self.inner.update_dynamic_pio_mappings(None)
        }
    }

    fn read_static(&mut self, reg: StaticReg, data: &mut [u8]) {
        if data.len() != 1 {
            tracelimit::warn_ratelimited!(?reg, ?data, "unexpected read");
            return;
        }

        data[0] = match reg {
            StaticReg::Control => self.state.power_control,
            StaticReg::Status => self.state.power_status,
        }
    }

    fn write_static(&mut self, reg: StaticReg, data: &[u8]) {
        if data.len() != 1 {
            tracelimit::warn_ratelimited!(?reg, ?data, "unexpected write");
            return;
        }

        let data = data[0];
        match reg {
            StaticReg::Control => {
                // If bit 25 is set in the Device Activity B register, we need
                // to generate an SMI on writes to port 0xB2 (the APMC).
                if self.state.device_activity_flags[1] & 1 << 25 != 0 {
                    // Normally, a write to port 0xB2 would generate an SMI which would
                    // invoke the ACPI BIOS. Virtualizing SMI is difficult, so we'll just
                    // emulate the important side-effects of the ACPI routines. The only
                    // important side effect expected by ACPI-aware OSes is that the SCI_EN
                    // bit in the power management control register is set and the PM timer
                    // overflow is enabled.
                    //
                    // The values 0xE1 and 0x1E are not defined by the chipset. Rather, they
                    // come from the system BIOS's ACPI table. If the BIOS is modified, the
                    // values below should be changed to match the ACPI_ENABLE and ACPI_DISABLE
                    // parameters within the FACP (fixed ACPI description) table.
                    if data == 0xE1 {
                        self.inner.pcat_facp_acpi_enable(true);
                    } else if data == 0x1E {
                        self.inner.pcat_facp_acpi_enable(false);
                    }
                }

                let old_control = self.state.power_control;
                self.state.power_control = data;

                // Handle accesses to the control port. This kludge was originally
                // only in the MR BIOS, so we could conditionalize based on this.
                // However, the OS/2 additions created by Innotek now use this kludge
                // too, so we need to keep it around. - Eric
                if old_control == b'E' && self.state.power_control == b'T' {
                    // If the sequence 'ET' is written to this port, the BIOS
                    // is telling us to power down. We will just quit. Note that
                    // this is not standard. Typically, the Triton chip set does
                    // not provide a way for desktop machines to power down.
                    // However, we have modified the BIOS to accept the power-
                    // down command (INT 15_5307, bx=0001, cx=0003) and generate
                    // this I/O write pattern.
                    (self.inner.power_action())(PowerAction::PowerOff)
                }
            }
            StaticReg::Status => self.state.power_status = data,
        }
    }
}

impl ChangeDeviceState for Piix4Pm {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.inner.reset().await;
        self.cfg_space.reset();
        self.state = Piix4PmState::new();

        self.update_io_mappings()
    }
}

impl ChipsetDevice for Piix4Pm {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_line_interrupt_target(&mut self) -> Option<&mut dyn LineInterruptTarget> {
        Some(self)
    }
}

impl PortIoIntercept for Piix4Pm {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        // 1-byte control register
        if let Some(0) = self.rt.pio_static_control.offset_of(io_port) {
            self.read_static(StaticReg::Control, data);
            return IoResult::Ok;
        }

        // 1-byte status register
        if let Some(0) = self.rt.pio_static_status.offset_of(io_port) {
            self.read_static(StaticReg::Status, data);
            return IoResult::Ok;
        }

        self.inner.io_read(io_port, data)
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        // 1-byte control register
        if let Some(0) = self.rt.pio_static_control.offset_of(io_port) {
            self.write_static(StaticReg::Control, data);

            self.inner.check_interrupt_assertion();
            return IoResult::Ok;
        }

        // 1-byte status register
        if let Some(0) = self.rt.pio_static_status.offset_of(io_port) {
            self.write_static(StaticReg::Status, data);

            self.inner.check_interrupt_assertion();
            return IoResult::Ok;
        }

        self.inner.io_write(io_port, data)
    }
}

/// Target for lines corresponding to bits in General Purpose Event Block 0.
///
/// For a specific description of an implementation of this, see the PIIX4
/// manual, section 7.2. The PIIX4 manual calls this register the "General
/// Purpose Status Register"
impl LineInterruptTarget for Piix4Pm {
    fn set_irq(&mut self, vector: u32, high: bool) {
        LineInterruptTarget::set_irq(&mut self.inner, vector, high)
    }

    fn valid_lines(&self) -> &[std::ops::RangeInclusive<u32>] {
        // PIIX4 manual dictates all other bits are marked as reserved.
        &[0..=0, 8..=11]
    }
}

/// Sidestep the config space emulator, and match legacy stub behavior directly
impl PciConfigSpace for Piix4Pm {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        *value = match ConfigSpace(offset) {
            // for bug-for-bug compat with the hyper-v implementation: return
            // hardcoded status register instead of letting the config space
            // emulator take care of it
            _ if offset == pci_core::spec::cfg_space::HeaderType00::STATUS_COMMAND.0 => {
                let mut v = 0x02800000;
                if self.state.smbus_io_enabled {
                    v |= 1;
                }
                v
            }
            // ditto for the latency/interrupt register
            _ if offset == pci_core::spec::cfg_space::HeaderType00::LATENCY_INTERRUPT.0 => {
                // report that the device is hard-wired to PCI interrupt lane A
                // (even though we don't actually use the IRQ for anything)
                let res = self.cfg_space.read_u32(offset, value);
                *value = *value & 0xff | (1 << 8);
                return res;
            }
            _ if offset < 0x40 => return self.cfg_space.read_u32(offset, value),
            // The bottom bit is always 1 to indicate an I/O address.
            ConfigSpace::IO_BASE => self.state.base_io_addr as u32 | 1,
            ConfigSpace::COUNT_A => self.state.counter_info_a,
            ConfigSpace::COUNT_B => self.state.counter_info_b,
            ConfigSpace::GENERAL_PURPOSE => self.state.general_purpose_config_info,
            ConfigSpace::ACTIVITY_A => self.state.device_activity_flags[0],
            ConfigSpace::ACTIVITY_B => self.state.device_activity_flags[1],
            ConfigSpace::RESOURCE_A => self.state.device_resource_flags[0],
            ConfigSpace::RESOURCE_B => self.state.device_resource_flags[1],
            ConfigSpace::RESOURCE_C => self.state.device_resource_flags[2],
            ConfigSpace::RESOURCE_D => self.state.device_resource_flags[3],
            ConfigSpace::RESOURCE_E => self.state.device_resource_flags[4],
            ConfigSpace::RESOURCE_F => self.state.device_resource_flags[5],
            ConfigSpace::RESOURCE_G => self.state.device_resource_flags[6],
            ConfigSpace::RESOURCE_H => self.state.device_resource_flags[7],
            ConfigSpace::RESOURCE_I => self.state.device_resource_flags[8],
            ConfigSpace::RESOURCE_J => self.state.device_resource_flags[9],
            ConfigSpace::IO_ENABLE => self.state.base_io_enable as u32,
            ConfigSpace::SM_BASE | ConfigSpace::SM_HOST => 0, // Hyper-V always returns 0, so do we.
            _ => {
                tracing::debug!(?offset, "unimplemented config space read");
                return IoResult::Err(IoError::InvalidRegister);
            }
        };

        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match ConfigSpace(offset) {
            // intercept smbus_io_enabled bit
            // We don't have SMBus support, but the bit still needs to get latched.
            _ if offset == pci_core::spec::cfg_space::HeaderType00::STATUS_COMMAND.0 => {
                self.state.smbus_io_enabled = value & 1 != 0;
                return self.cfg_space.write_u32(offset, value);
            }
            _ if offset < 0x40 => return self.cfg_space.write_u32(offset, value),
            ConfigSpace::IO_BASE => {
                // mask off the read-only bits
                //
                // NOTE: this implies that the base address of the pm device
                // will always be a multiple of 0x100
                self.state.base_io_addr = (value & 0xFFC0) as u16;
                self.update_io_mappings()
            }
            ConfigSpace::COUNT_A => self.state.counter_info_a = value,
            ConfigSpace::COUNT_B => self.state.counter_info_b = value,
            ConfigSpace::GENERAL_PURPOSE => self.state.general_purpose_config_info = value,
            ConfigSpace::ACTIVITY_A => self.state.device_activity_flags[0] = value,
            ConfigSpace::ACTIVITY_B => self.state.device_activity_flags[1] = value,
            ConfigSpace::RESOURCE_A => self.state.device_resource_flags[0] = value,
            ConfigSpace::RESOURCE_B => self.state.device_resource_flags[1] = value,
            ConfigSpace::RESOURCE_C => self.state.device_resource_flags[2] = value,
            ConfigSpace::RESOURCE_D => self.state.device_resource_flags[3] = value,
            ConfigSpace::RESOURCE_E => self.state.device_resource_flags[4] = value,
            ConfigSpace::RESOURCE_F => self.state.device_resource_flags[5] = value,
            ConfigSpace::RESOURCE_G => self.state.device_resource_flags[6] = value,
            ConfigSpace::RESOURCE_H => self.state.device_resource_flags[7] = value,
            ConfigSpace::RESOURCE_I => self.state.device_resource_flags[8] = value,
            ConfigSpace::RESOURCE_J => self.state.device_resource_flags[9] = value,
            ConfigSpace::IO_ENABLE => {
                self.state.base_io_enable = value & 1 != 0;
                self.update_io_mappings()
            }
            ConfigSpace::SM_BASE | ConfigSpace::SM_HOST => {} // Hyper-V ignores these, so do we.
            _ => {
                tracelimit::warn_ratelimited!(?offset, ?value, "unimplemented config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        Some((0, 7, 3)) // as per PIIX4 spec
    }
}

open_enum! {
    enum ConfigSpace: u16 {
        IO_BASE         = 0x40,
        COUNT_A         = 0x44,
        COUNT_B         = 0x48,
        GENERAL_PURPOSE = 0x4C,
        RESOURCE_D      = 0x50,
        ACTIVITY_A      = 0x54,
        ACTIVITY_B      = 0x58,
        RESOURCE_A      = 0x5C,
        RESOURCE_B      = 0x60,
        RESOURCE_C      = 0x64,
        RESOURCE_E      = 0x68,
        RESOURCE_F      = 0x6C,
        RESOURCE_G      = 0x70,
        RESOURCE_H      = 0x74,
        RESOURCE_I      = 0x78,
        RESOURCE_J      = 0x7C,
        IO_ENABLE       = 0x80,
        SM_BASE         = 0x90,
        SM_HOST         = 0xD0,
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use chipset::pm::PowerManagementDevice;
        use mesh::payload::Protobuf;
        use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.piix4.pm")]
        pub struct SavedState {
            #[mesh(1)]
            pub power_status: u8,
            #[mesh(2)]
            pub power_control: u8,
            #[mesh(3)]
            pub smbus_io_enabled: bool,
            #[mesh(4)]
            pub base_io_addr: u16,
            #[mesh(5)]
            pub base_io_enable: bool,
            #[mesh(6)]
            pub counter_info_a: u32,
            #[mesh(7)]
            pub counter_info_b: u32,
            #[mesh(8)]
            pub general_purpose_config_info: u32,
            #[mesh(9)]
            pub device_resource_flags: [u32; 10],
            #[mesh(10)]
            pub device_activity_flags: [u32; 2],
            #[mesh(11)]
            pub cfg_space: <ConfigSpaceType0Emulator as SaveRestore>::SavedState,
            #[mesh(12)]
            pub inner: <PowerManagementDevice as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for Piix4Pm {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Piix4PmState {
                power_status,
                power_control,
                smbus_io_enabled,
                base_io_addr,
                base_io_enable,
                counter_info_a,
                counter_info_b,
                general_purpose_config_info,
                device_resource_flags,
                device_activity_flags,
            } = self.state;

            let saved_state = state::SavedState {
                power_status,
                power_control,
                smbus_io_enabled,
                base_io_addr,
                base_io_enable,
                counter_info_a,
                counter_info_b,
                general_purpose_config_info,
                device_resource_flags,
                device_activity_flags,
                cfg_space: self.cfg_space.save()?,
                inner: self.inner.save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                power_status,
                power_control,
                smbus_io_enabled,
                base_io_addr,
                base_io_enable,
                counter_info_a,
                counter_info_b,
                general_purpose_config_info,
                device_resource_flags,
                device_activity_flags,
                cfg_space,
                inner,
            } = state;

            let state = Piix4PmState {
                power_status,
                power_control,
                smbus_io_enabled,
                base_io_addr,
                base_io_enable,
                counter_info_a,
                counter_info_b,
                general_purpose_config_info,
                device_resource_flags,
                device_activity_flags,
            };

            self.state = state;

            self.update_io_mappings();
            self.cfg_space.restore(cfg_space)?;
            self.inner.restore(inner)?;
            Ok(())
        }
    }
}
