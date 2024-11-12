// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PIIX4 - PCI to ISA Bridge

use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use chipset_device::pio::PortIoIntercept;
use chipset_device::ChipsetDevice;
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

/// IO ports as specified by the PIIX4 data sheet
mod io_ports {
    pub const FAST_A20_GATE: u16 = 0x92;
    pub const MATH_COPROC0: u16 = 0xF0;
    pub const MATH_COPROC1: u16 = 0xF1;
}

struct PciIsaBridgeRuntime {
    reset_evt: Box<dyn Fn() + Send + Sync>,
    set_a20_signal: Box<dyn FnMut(bool) + Send + Sync>,
}

/// PIIX4 (PCI device function 0) - PCI to ISA Bridge
///
/// See section 3.1 in the PIIX4 data sheet.
#[derive(InspectMut)]
pub struct PciIsaBridge {
    // Runtime glue
    #[inspect(skip)]
    rt: PciIsaBridgeRuntime,

    // Sub-emulators
    cfg_space: ConfigSpaceType0Emulator,

    // Volatile state
    state: PciIsaBridgeState,
}

#[derive(Inspect)]
struct PciIsaBridgeState {
    pci_irq_routing: u32,
    smi_control: u32,
    smi_request: u32,
    system_event: u32,
    clock_scale: u32,
    apic_base: u32,
    a20_gate_enabled: bool,
}

impl PciIsaBridgeState {
    fn new() -> Self {
        Self {
            // hard-code PCI lane A to IRQ 11, disabling all other PCI IRQ lines
            pci_irq_routing: 0x80808000 | 11,
            smi_control: 0x00000008,
            smi_request: 0x0000000F,
            system_event: 0,
            clock_scale: 0,
            apic_base: 0,
            a20_gate_enabled: true,
        }
    }
}

impl PciIsaBridge {
    pub fn new(
        reset_evt: Box<dyn Fn() + Send + Sync>,
        set_a20_signal: Box<dyn FnMut(bool) + Send + Sync>,
    ) -> Self {
        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x8086,
                device_id: 0x7110,
                revision_id: 0x03,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::BRIDGE_ISA,
                base_class: ClassCode::BRIDGE,
                type0_sub_vendor_id: 0x1414,
                type0_sub_system_id: 0,
            },
            Vec::new(),
            DeviceBars::new(),
        )
        .with_multi_function_bit(true);

        Self {
            rt: PciIsaBridgeRuntime {
                reset_evt,
                set_a20_signal,
            },

            cfg_space,
            state: PciIsaBridgeState::new(),
        }
    }

    fn handle_math_coproc_read(&mut self, max_access_size: usize, data: &mut [u8]) {
        if data.len() > max_access_size {
            tracelimit::warn_ratelimited!(?max_access_size, len = ?data.len(), "unexpected MATH_COPROC read len");
            data.fill(0xff);
            return;
        }

        // ..but also, on a valid read, we still just return all Fs
        data.fill(0xff)
    }

    fn handle_math_coproc_write(&mut self, max_access_size: usize, data: &[u8]) {
        if data.len() > max_access_size {
            tracelimit::warn_ratelimited!(?max_access_size, len = ?data.len(), "unexpected MATH_COPROC write len");
            return;
        }

        // the legacy stack would deassert IRQ number 13 here, but AFAIK,
        // nothing ever actually asserted that IRQ in the first place?
        let _ = data;
    }

    fn handle_fast_a20_read(&mut self, data: &mut [u8]) {
        if data.len() != 1 {
            tracelimit::warn_ratelimited!(len = ?data.len(), "unexpected FAST_A20_GATE read len");
            return;
        }

        // Clear bit 1 if the fast A20 gate is enabled.
        data[0] = if self.state.a20_gate_enabled {
            0x00
        } else {
            0x02
        };
    }

    fn handle_fast_a20_write(&mut self, data: &[u8]) {
        if data.len() != 1 {
            tracelimit::warn_ratelimited!(len = ?data.len(), "unexpected FAST_A20_GATE write len");
            return;
        }

        let v = data[0];

        if v & 0x01 != 0 {
            tracing::info!("initiating guest reset via FAST_A20_GATE");
            (self.rt.reset_evt)();
            return;
        }

        self.state.a20_gate_enabled = v & 0x02 == 0;
        (self.rt.set_a20_signal)(self.state.a20_gate_enabled);
    }
}

impl ChangeDeviceState for PciIsaBridge {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        // Assume the caller will reset the A20 state to its initial state.
        self.state = PciIsaBridgeState::new();
        self.cfg_space.reset();
    }
}

impl ChipsetDevice for PciIsaBridge {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PortIoIntercept for PciIsaBridge {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        use self::io_ports::*;
        match io_port {
            FAST_A20_GATE => self.handle_fast_a20_read(data),
            MATH_COPROC0 => self.handle_math_coproc_read(2, data),
            MATH_COPROC1 => self.handle_math_coproc_read(1, data),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        use self::io_ports::*;
        match io_port {
            FAST_A20_GATE => self.handle_fast_a20_write(data),
            MATH_COPROC0 => self.handle_math_coproc_write(2, data),
            MATH_COPROC1 => self.handle_math_coproc_write(1, data),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, std::ops::RangeInclusive<u16>)] {
        use self::io_ports::*;

        &[
            ("fast_a20_gate", FAST_A20_GATE..=FAST_A20_GATE),
            ("math_coproc", MATH_COPROC0..=MATH_COPROC1),
            // NOTE: we don't explicitly claim RESET_CF9 port here, since that
            // would result in a conflict with the PCI bus's ADDR/DATA IO ports.
            // ("reset_cf9", RESET_CF9..=RESET_CF9),
        ]
    }
}

impl PciConfigSpace for PciIsaBridge {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        *value = match ConfigSpace(offset) {
            // for bug-for-bug compat with the hyper-v implementation: return
            // hardcoded status register instead of letting the config space
            // emulator take care of it
            _ if offset == pci_core::spec::cfg_space::HeaderType00::STATUS_COMMAND.0 => 0x02000007,
            _ if offset < 0x40 => return self.cfg_space.read_u32(offset, value),
            // these magic values mainly consist of default values pulled from
            // the PIIX4 documentation
            ConfigSpace::TOP => 0x00000200,
            ConfigSpace::IO_REC => 0x0003004D,
            ConfigSpace::PIRQ => self.state.pci_irq_routing,
            ConfigSpace::SER_IRQ => 0x0000000D0,
            ConfigSpace::SMI => self.state.smi_control,
            ConfigSpace::SEE => self.state.system_event,
            ConfigSpace::FTM => self.state.smi_request,
            ConfigSpace::CTL_TMR => self.state.clock_scale,
            ConfigSpace::RTC_CONFIG => 0x25000000,
            ConfigSpace::MANUF_ID => 0x00000F30,
            ConfigSpace::APIC_BASE => self.state.apic_base,
            ConfigSpace::DMA_CFG1
            | ConfigSpace::DMA_CFG2
            | ConfigSpace::IRQ_RT
            | ConfigSpace::DMA
            | ConfigSpace::PCSC
            | ConfigSpace::GEN_CONFIG => 0,
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
            ConfigSpace::PIRQ => {
                if self.state.pci_irq_routing != value {
                    tracelimit::info_ratelimited!(new_pci_irq_routing = ?value, "custom PCI IRQ routing is not implemented!");
                }

                self.state.pci_irq_routing = value;
            }
            ConfigSpace::SER_IRQ => {
                if !(value == 0x0000000D0 || value == 0x000000010) {
                    tracelimit::warn_ratelimited!(
                        ?value,
                        "set invalid serial IRQ control register value"
                    );
                }
            }
            ConfigSpace::TOP => {
                // Make sure ISA/DMA 512-640K Region Forwarding Enable is never cleared.
                // This controls whether addresses 512K to 640K are forwarded to RAM
                // (which we always want to do).
                if (value & 0x00000200) == 0 {
                    tracing::debug!("ISA/DMA 512-640K Region Forwarding Enable was cleared!");
                }
            }
            ConfigSpace::SMI => self.state.smi_control = value & 0x00FF001F,
            ConfigSpace::SEE => self.state.system_event = value,
            ConfigSpace::FTM => self.state.smi_request = value,
            ConfigSpace::CTL_TMR => self.state.clock_scale = value,
            ConfigSpace::RTC_CONFIG => {
                // For now, the code assumes the default value. We don't support
                // disabling extended CMOS (upper 128 bytes).
                if (value & 0x04000000) == 0 {
                    tracing::debug!("Trying to disable extended CMOS - not supported")
                }

                if value != 0x25000000 {
                    tracelimit::warn_ratelimited!(?value, "unexpected value for RTC_CONFIG write")
                }
            }
            ConfigSpace::APIC_BASE => {
                // If any of bits 0..5 have changed, then we need to change the base of
                // the IoApic.
                if (value & 0x3F) != (self.state.apic_base & 0x3F) {
                    // DEVNOTE: this shouldn't actually be all that difficult to
                    // implement, but until there is definitive proof that there
                    // is a supported guest OS that makes use of this bit of
                    // functionality, its best to just keep things simple and
                    // cross-deps minimal.
                    tracelimit::error_ratelimited!(
                        ?value,
                        "changing the IOAPIC base is not implemented!"
                    );
                }

                self.state.apic_base = value;
            }
            ConfigSpace::GEN_CONFIG
            | ConfigSpace::DMA_CFG1
            | ConfigSpace::DMA_CFG2
            | ConfigSpace::IO_REC
            | ConfigSpace::IRQ_RT
            | ConfigSpace::DMA
            | ConfigSpace::PCSC => {
                // always ignored
            }
            _ => {
                tracelimit::warn_ratelimited!(?offset, ?value, "unimplemented config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        Some((0, 7, 0)) // as per PIIX4 spec
    }
}

open_enum! {
    enum ConfigSpace: u16 {
        IO_REC     = 0x4C,
        PIRQ       = 0x60,
        SER_IRQ    = 0x64,
        TOP        = 0x68,
        IRQ_RT     = 0x70,
        DMA        = 0x74,
        PCSC       = 0x78,
        APIC_BASE  = 0x80,
        DMA_CFG1   = 0x90,
        DMA_CFG2   = 0x94,
        SMI        = 0xA0,
        SEE        = 0xA4,
        FTM        = 0xA8,
        CTL_TMR    = 0xAC,
        GEN_CONFIG = 0xB0,
        RTC_CONFIG = 0xC8,
        MANUF_ID   = 0xF8,
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
        #[mesh(package = "chipset.piix4.pci_isa_bridge")]
        pub struct SavedState {
            #[mesh(1)]
            pub pci_irq_routing: u32,
            #[mesh(2)]
            pub smi_control: u32,
            #[mesh(3)]
            pub smi_request: u32,
            #[mesh(4)]
            pub system_event: u32,
            #[mesh(5)]
            pub clock_scale: u32,
            #[mesh(6)]
            pub apic_base: u32,
            #[mesh(7)]
            pub a20_gate_enabled: bool,
            #[mesh(8)]
            pub cfg_space: <ConfigSpaceType0Emulator as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for PciIsaBridge {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let PciIsaBridgeState {
                pci_irq_routing,
                smi_control,
                smi_request,
                system_event,
                clock_scale,
                apic_base,
                a20_gate_enabled,
            } = self.state;

            let saved_state = state::SavedState {
                pci_irq_routing,
                smi_control,
                smi_request,
                system_event,
                clock_scale,
                apic_base,
                a20_gate_enabled,
                cfg_space: self.cfg_space.save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                pci_irq_routing,
                smi_control,
                smi_request,
                system_event,
                clock_scale,
                apic_base,
                a20_gate_enabled,
                cfg_space,
            } = state;

            let state = PciIsaBridgeState {
                pci_irq_routing,
                smi_control,
                smi_request,
                system_event,
                clock_scale,
                apic_base,
                a20_gate_enabled,
            };

            self.state = state;

            // sync a20 signal
            (self.rt.set_a20_signal)(self.state.a20_gate_enabled);

            self.cfg_space.restore(cfg_space)?;

            Ok(())
        }
    }
}
