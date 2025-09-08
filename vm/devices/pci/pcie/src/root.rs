// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express root complex and root port emulation.

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use inspect::Inspect;
use inspect::InspectMut;
use pci_bus::GenericPciBusDevice;
use std::collections::HashMap;
use std::sync::Arc;
use vmcore::device_state::ChangeDeviceState;
use zerocopy::IntoBytes;

/// A generic PCI Express root complex emulator.
#[derive(InspectMut)]
pub struct GenericPcieRootComplex {
    /// The lowest valid bus number under the root complex.
    start_bus: u8,
    /// The highest valid bus number under the root complex.
    end_bus: u8,
    /// Intercept control for the ECAM MMIO region.
    ecam: Box<dyn ControlMmioIntercept>,
    /// Map of root ports attached to the root complex, indexed by combined device and function numbers.
    #[inspect(with = "|x| inspect::iter_by_key(x).map_value(|(name, _)| name)")]
    ports: HashMap<u8, (Arc<str>, RootPort)>,
}

/// A description of a generic PCIe root port.
pub struct GenericPcieRootPortDefinition {
    /// The name of the root port.
    pub name: Arc<str>,
}

enum DecodedEcamAccess<'a> {
    UnexpectedIntercept(),
    Unroutable(),
    InternalBus(&'a mut RootPort, u16),
    DownstreamPort(&'a mut RootPort, u8, u16),
}

impl GenericPcieRootComplex {
    /// Constructs a new `GenericPcieRootComplex` emulator.
    pub fn new(
        register_mmio: &mut dyn RegisterMmioIntercept,
        start_bus: u8,
        end_bus: u8,
        ecam_base: u64,
        ports: Vec<GenericPcieRootPortDefinition>,
    ) -> Self {
        let bus_count = (end_bus as u16) - (start_bus as u16) + 1;
        let ecam_size = (bus_count as u64) * 256 * 4096;
        let mut ecam = register_mmio.new_io_region("ecam", ecam_size);
        ecam.map(ecam_base);

        let port_map = ports
            .into_iter()
            .enumerate()
            .map(|(i, definition)| {
                let device_number = (i << 3) as u8;
                let emulator = RootPort::new();
                (device_number, (definition.name, emulator))
            })
            .collect();

        Self {
            start_bus,
            end_bus,
            ecam,
            ports: port_map,
        }
    }

    /// Attach the provided `GenericPciBusDevice` to the port identified.
    pub fn add_pcie_device<D: GenericPciBusDevice>(
        &mut self,
        port: u8,
        name: impl AsRef<str>,
        dev: D,
    ) -> Result<(), (D, Arc<str>)> {
        let (_, root_port) = self.ports.get_mut(&port).unwrap();
        root_port.connect_device(name, dev)?;
        Ok(())
    }

    /// Enumerate the downstream ports of the root complex.
    pub fn downstream_ports(&self) -> Vec<(u8, Arc<str>)> {
        self.ports
            .iter()
            .map(|(port, (name, _))| (*port, name.clone()))
            .collect()
    }

    fn decode_ecam_access<'a>(&'a mut self, addr: u64) -> DecodedEcamAccess<'a> {
        let ecam_offset = match self.ecam.offset_of(addr) {
            Some(offset) => offset,
            None => {
                return DecodedEcamAccess::UnexpectedIntercept();
            }
        };

        let cfg_offset_within_function = (ecam_offset % 4096) as u16;
        let bdf_offset_within_ecam = (ecam_offset / 4096) & 0xFFFF;
        let bus_offset_within_ecam = ((bdf_offset_within_ecam & 0xFF00) >> 8) as u8;
        let bus_number = bus_offset_within_ecam + self.start_bus;
        let device_function = (bdf_offset_within_ecam & 0xFF) as u8;

        if bus_number == self.start_bus {
            match self.ports.get_mut(&device_function) {
                Some((_, port)) => {
                    return DecodedEcamAccess::InternalBus(port, cfg_offset_within_function);
                }
                None => return DecodedEcamAccess::Unroutable(),
            }
        } else if bus_number > self.start_bus && bus_number <= self.end_bus {
            for (_, port) in self.ports.values_mut() {
                if port.assigned_bus_number(bus_number) {
                    return DecodedEcamAccess::DownstreamPort(
                        port,
                        bus_number,
                        cfg_offset_within_function,
                    );
                }
            }
            return DecodedEcamAccess::Unroutable();
        }

        DecodedEcamAccess::UnexpectedIntercept()
    }
}

fn shift_read_value(cfg_offset: u16, len: usize, value: u32) -> u32 {
    let shift = (cfg_offset & 0x3) * 8;
    match len {
        4 => value,
        2 => value >> shift & 0xFFFF,
        1 => value >> shift & 0xFF,
        _ => unreachable!(),
    }
}

fn combine_old_new_values(cfg_offset: u16, old_value: u32, new_value: u32, len: usize) -> u32 {
    let shift = (cfg_offset & 0x3) * 8;
    let mask = (1 << (len * 8)) - 1;
    (old_value & !(mask << shift)) | (new_value << shift)
}

impl ChangeDeviceState for GenericPcieRootComplex {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {}
}

impl ChipsetDevice for GenericPcieRootComplex {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

macro_rules! validate_ecam_intercept {
    ($address:ident, $data:ident) => {
        if !matches!($data.len(), 1 | 2 | 4) {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        if !((($data.len() == 4) && ($address & 3 == 0))
            || (($data.len() == 2) && ($address & 1 == 0))
            || ($data.len() == 1))
        {
            return IoResult::Err(IoError::UnalignedAccess);
        }
    };
}

impl MmioIntercept for GenericPcieRootComplex {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        validate_ecam_intercept!(addr, data);

        let mut value = !0;
        match self.decode_ecam_access(addr) {
            DecodedEcamAccess::UnexpectedIntercept() => {
                tracing::error!("unexpected intercept at address 0x{:16x}", addr);
            }
            DecodedEcamAccess::Unroutable() => {
                tracelimit::warn_ratelimited!("unroutable config space access");
            }
            DecodedEcamAccess::InternalBus(port, cfg_offset) => {
                let _ = port.pci_cfg_read(cfg_offset & !3, &mut value);
                value = shift_read_value(cfg_offset, data.len(), value);
            }
            DecodedEcamAccess::DownstreamPort(port, bus_number, cfg_offset) => {
                let _ = port.forward_cfg_read(&bus_number, cfg_offset & !3, &mut value);
                value = shift_read_value(cfg_offset, data.len(), value);
            }
        }

        data.copy_from_slice(&value.as_bytes()[..data.len()]);
        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        validate_ecam_intercept!(addr, data);

        let write_value = {
            let mut temp: u32 = 0;
            temp.as_mut_bytes()[..data.len()].copy_from_slice(data);
            temp
        };

        match self.decode_ecam_access(addr) {
            DecodedEcamAccess::UnexpectedIntercept() => {
                tracing::error!("unexpected intercept at address 0x{:16x}", addr);
            }
            DecodedEcamAccess::Unroutable() => {
                tracelimit::warn_ratelimited!("unroutable config space access");
            }
            DecodedEcamAccess::InternalBus(port, cfg_offset) => {
                let rounded_offset = cfg_offset & !3;
                let merged_value = if data.len() == 4 {
                    write_value
                } else {
                    let mut temp: u32 = 0;
                    let _ = port.pci_cfg_read(rounded_offset, &mut temp);
                    combine_old_new_values(cfg_offset, temp, write_value, data.len())
                };

                let _ = port.pci_cfg_write(rounded_offset, merged_value);
            }
            DecodedEcamAccess::DownstreamPort(port, bus_number, cfg_offset) => {
                let rounded_offset = cfg_offset & !3;
                let merged_value = if data.len() == 4 {
                    write_value
                } else {
                    let mut temp: u32 = 0;
                    let _ = port.forward_cfg_read(&bus_number, rounded_offset, &mut temp);
                    combine_old_new_values(cfg_offset, temp, write_value, data.len())
                };

                let _ = port.forward_cfg_write(&bus_number, rounded_offset, merged_value);
            }
        }

        IoResult::Ok
    }
}

#[derive(Inspect)]
struct RootPort {
    // Minimal type 1 configuration space emulation for
    // Linux and Windows to enumerate the port. This should
    // be refactored into a dedicated type 1 emulator.
    command_status_register: u32,
    bus_number_registers: u32,
    memory_limit_registers: u32,
    prefetch_limit_registers: u32,
    prefetch_base_upper_register: u32,
    prefetch_limit_upper_register: u32,

    #[inspect(skip)]
    link: Option<(Arc<str>, Box<dyn GenericPciBusDevice>)>,
}

impl RootPort {
    /// Constructs a new `RootPort` emulator.
    pub fn new() -> Self {
        Self {
            command_status_register: 0,
            bus_number_registers: 0,
            memory_limit_registers: 0,
            prefetch_limit_registers: 0,
            prefetch_base_upper_register: 0,
            prefetch_limit_upper_register: 0,
            link: None,
        }
    }

    /// Try to connect a PCIe device, returning (device, existing_device_name) if the
    /// port is already occupied.
    pub fn connect_device<D: GenericPciBusDevice>(
        &mut self,
        name: impl AsRef<str>,
        dev: D,
    ) -> Result<(), (D, Arc<str>)> {
        if let Some((name, _)) = &self.link {
            return Err((dev, name.clone()));
        }

        self.link = Some((name.as_ref().into(), Box::new(dev)));
        Ok(())
    }

    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        *value = match offset {
            0x00 => 0xF111_1414, // Device and Vendor IDs
            0x04 => self.command_status_register | 0x0010_0000,
            0x08 => 0x0604_0000, // Class code and revision
            0x0C => 0x0001_0000, // Header type 1
            0x10 => 0x0000_0000, // BAR0
            0x14 => 0x0000_0000, // BAR1
            0x18 => self.bus_number_registers,
            0x1C => 0x0000_0000, // Secondary status and I/O range
            0x20 => self.memory_limit_registers,
            0x24 => self.prefetch_limit_registers,
            0x28 => self.prefetch_base_upper_register,
            0x2C => self.prefetch_limit_upper_register,
            0x30 => 0x0000_0000, // I/O base and limit 16 bit
            0x34 => 0x0000_0040, // Reserved and Capability pointer
            0x38 => 0x0000_0000, // Expansion ROM
            0x3C => 0x0000_0000, // Bridge control, interrupt pin/line

            // PCI Express capability structure
            0x40 => 0x0142_0010, // Capability header and PCI Express capabilities register
            0x44 => 0x0000_0000, // Device capabilities register
            0x48 => 0x0000_0000, // Device control and status registers
            0x4C => 0x0000_0000, // Link capabilities register
            0x50 => 0x0011_0000, // Link control and status registers
            0x54 => 0x0000_0000, // Slot capabilities register
            0x58 => 0x0000_0000, // Slot status and control registers
            0x5C => 0x0000_0000, // Root capabilities and control registers
            0x60 => 0x0000_0000, // Root status register
            0x64 => 0x0000_0000, // Device capabilities 2 register
            0x68 => 0x0000_0000, // Device status 2 and control 2 registers
            0x6C => 0x0000_0000, // Link capabilities 2 register
            0x70 => 0x0000_0000, // Link status 2 and control 2 registers
            0x74 => 0x0000_0000, // Slot capabilities 2 register
            0x78 => 0x0000_0000, // Slot status 2 and control 2 registers

            _ => 0xFFFF,
        };

        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match offset {
            0x04 => self.command_status_register = value,
            0x18 => self.bus_number_registers = value,
            0x20 => self.memory_limit_registers = value,
            0x24 => self.prefetch_limit_registers = value,
            0x28 => self.prefetch_base_upper_register = value,
            0x2C => self.prefetch_limit_upper_register = value,
            _ => {}
        };

        IoResult::Ok
    }

    fn assigned_bus_number(&self, bus: u8) -> bool {
        let secondary_bus_number = ((self.bus_number_registers >> 8) & 0xFF) as u8;
        let suboordinate_bus_number = ((self.bus_number_registers >> 16) & 0xFF) as u8;

        bus >= secondary_bus_number && bus <= suboordinate_bus_number
    }

    fn forward_cfg_read(&mut self, bus: &u8, cfg_offset: u16, value: &mut u32) -> IoResult {
        let secondary_bus_number = ((self.bus_number_registers >> 8) & 0xFF) as u8;
        let suboordinate_bus_number = ((self.bus_number_registers >> 16) & 0xFF) as u8;

        if *bus == secondary_bus_number {
            if let Some((_, device)) = &mut self.link {
                let _ = device.pci_cfg_read(cfg_offset, value);
            }
        } else if *bus > secondary_bus_number && *bus <= suboordinate_bus_number {
            tracelimit::warn_ratelimited!("multi-level hierarchies not implemented yet");
        }

        IoResult::Ok
    }

    fn forward_cfg_write(&mut self, bus: &u8, cfg_offset: u16, value: u32) -> IoResult {
        let secondary_bus_number = ((self.bus_number_registers >> 8) & 0xFF) as u8;
        let suboordinate_bus_number = ((self.bus_number_registers >> 16) & 0xFF) as u8;

        if *bus == secondary_bus_number {
            if let Some((_, device)) = &mut self.link {
                let _ = device.pci_cfg_write(cfg_offset, value);
            }
        } else if *bus > secondary_bus_number && *bus <= suboordinate_bus_number {
            tracelimit::warn_ratelimited!("multi-level hierarchies not implemented yet");
        }

        IoResult::Ok
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateNotSupported;

    impl SaveRestore for GenericPcieRootComplex {
        type SavedState = SavedStateNotSupported;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Err(SaveError::NotSupported)
        }

        fn restore(
            &mut self,
            state: Self::SavedState,
        ) -> Result<(), vmcore::save_restore::RestoreError> {
            match state {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;

    /// Instantiate a root complex with the provided lowest bus number and port count.
    /// ECAM base address is assumed to be 0, and highest bus number is assumed to be 255.
    fn instantiate_root_complex(start_bus: u8, port_count: u8) -> GenericPcieRootComplex {
        let port_defs = (0..port_count)
            .map(|i| GenericPcieRootPortDefinition {
                name: format!("test-port-{}", i).into(),
            })
            .collect();

        let mut register_mmio = TestPcieMmioRegistration {};
        GenericPcieRootComplex::new(&mut register_mmio, start_bus, 255, 0, port_defs)
    }

    #[test]
    fn test_create() {
        let rc = instantiate_root_complex(0, 4);
        assert_eq!(rc.downstream_ports().len(), 4);
    }

    #[test]
    fn test_probe_ports_via_config_space() {
        let mut rc = instantiate_root_complex(0, 4);
        for device_number in 0..4 {
            let mut vendor_device: u32 = 0;
            rc.mmio_read((device_number << 3) * 4096, vendor_device.as_mut_bytes())
                .unwrap();
            assert_eq!(vendor_device, 0xF111_1414);

            let mut value_16: u16 = 0;
            rc.mmio_read((device_number << 3) * 4096, value_16.as_mut_bytes())
                .unwrap();
            assert_eq!(value_16, 0x1414);

            rc.mmio_read((device_number << 3) * 4096 + 2, value_16.as_mut_bytes())
                .unwrap();
            assert_eq!(value_16, 0xF111);
        }

        for device_number in 4..10 {
            let mut value_32: u32 = 0;
            rc.mmio_read((device_number << 3) * 4096, value_32.as_mut_bytes())
                .unwrap();
            assert_eq!(value_32, 0xFFFF_FFFF);

            let mut value_16: u16 = 0;
            rc.mmio_read((device_number << 3) * 4096, value_16.as_mut_bytes())
                .unwrap();
            assert_eq!(value_16, 0xFFFF);
            rc.mmio_read((device_number << 3) * 4096 + 2, value_16.as_mut_bytes())
                .unwrap();
            assert_eq!(value_16, 0xFFFF);
        }
    }

    #[test]
    fn test_root_port_cfg_forwarding() {
        const SECONDARY_BUS_NUMBER_ADDRESS: u64 = 0x19;
        const SUBOORDINATE_BUS_NUMBER_ADDRESS: u64 = 0x1A;

        let mut rc = instantiate_root_complex(0, 1);

        // Pre-bus number assignment, random accesses don't work.
        let mut value_32: u32 = 0;
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xFFFF_FFFF);

        // Secondary and suboordinate bus number registers are both
        // read / write, defaulting to 0.
        let mut bus_number: u8 = 0xFF;
        rc.mmio_read(SECONDARY_BUS_NUMBER_ADDRESS, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 0);
        rc.mmio_read(SUBOORDINATE_BUS_NUMBER_ADDRESS, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 0);

        rc.mmio_write(SECONDARY_BUS_NUMBER_ADDRESS, &[1]).unwrap();
        rc.mmio_read(SECONDARY_BUS_NUMBER_ADDRESS, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 1);

        rc.mmio_write(SUBOORDINATE_BUS_NUMBER_ADDRESS, &[2])
            .unwrap();
        rc.mmio_read(SUBOORDINATE_BUS_NUMBER_ADDRESS, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 2);

        // Bus numbers assigned, but no endpoint attached yet.
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xFFFF_FFFF);

        let endpoint = TestPcieEndpoint::new(
            |offset, value| match offset {
                0x0 => {
                    *value = 0xDEAD_BEEF;
                    Some(IoResult::Ok)
                }
                _ => Some(IoResult::Err(IoError::InvalidRegister)),
            },
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        let _ = rc.add_pcie_device(0, "test-ep", endpoint);

        // The secondary bus behind root port 0 has been assigned bus number
        // 1, so now the attached endpoint is accessible.
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xDEAD_BEEF);

        // Reassign the secondary bus number to 2.
        rc.mmio_write(SECONDARY_BUS_NUMBER_ADDRESS, &[2]).unwrap();
        rc.mmio_read(SECONDARY_BUS_NUMBER_ADDRESS, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 2);

        // The endpoint is no longer accessible at bus number 1, and is now
        // accessible at bus number 2.
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xFFFF_FFFF);
        rc.mmio_read(2 * 256 * 4096, value_32.as_mut_bytes())
            .unwrap();
        assert_eq!(value_32, 0xDEAD_BEEF);
    }
}
