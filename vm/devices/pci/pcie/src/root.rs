// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express root complex and root port emulation.

use crate::BDF_BUS_SHIFT;
use crate::BDF_DEVICE_FUNCTION_MASK;
use crate::BDF_DEVICE_SHIFT;
use crate::MAX_FUNCTIONS_PER_BUS;
use crate::PAGE_OFFSET_MASK;
use crate::PAGE_SHIFT;
use crate::PAGE_SIZE64;
use crate::ROOT_PORT_DEVICE_ID;
use crate::VENDOR_ID;
use crate::port::PcieDownstreamPort;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use inspect::Inspect;
use inspect::InspectMut;
use pci_bus::GenericPciBusDevice;
use pci_core::spec::caps::pci_express::DevicePortType;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
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
    #[inspect(with = "|x| inspect::iter_by_key(x).map_value(|(_, v)| v)")]
    ports: HashMap<u8, (Arc<str>, RootPort)>,
}

/// A description of a generic PCIe root port.
pub struct GenericPcieRootPortDefinition {
    /// The name of the root port.
    pub name: Arc<str>,
}

/// A flat description of a PCIe switch without hierarchy.
pub struct GenericSwitchDefinition {
    /// The name of the switch.
    pub name: Arc<str>,
    /// Number of downstream ports.
    pub num_downstream_ports: u8,
    /// The parent port this switch is connected to.
    pub parent_port: Arc<str>,
}

impl GenericSwitchDefinition {
    /// Create a new switch definition.
    pub fn new(
        name: impl Into<Arc<str>>,
        num_downstream_ports: u8,
        parent_port: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            name: name.into(),
            num_downstream_ports,
            parent_port: parent_port.into(),
        }
    }
}

enum DecodedEcamAccess<'a> {
    UnexpectedIntercept,
    Unroutable,
    InternalBus(&'a mut RootPort, u16),
    DownstreamPort(&'a mut RootPort, u8, u8, u16),
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
        let ecam_size = ecam_size_from_bus_numbers(start_bus, end_bus);
        let mut ecam = register_mmio.new_io_region("ecam", ecam_size);
        ecam.map(ecam_base);

        let port_map: HashMap<u8, (Arc<str>, RootPort)> = ports
            .into_iter()
            .enumerate()
            .map(|(i, definition)| {
                let device_number: u8 = (i << BDF_DEVICE_SHIFT).try_into().expect("too many ports");
                let emulator = RootPort::new(definition.name.clone());
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
    pub fn add_pcie_device(
        &mut self,
        port: u8,
        name: impl AsRef<str>,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        let (_, root_port) = self
            .ports
            .get_mut(&port)
            .expect("caller must pass port number returned by downstream_ports()");
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

    /// Returns the size of the ECAM MMIO region this root complex is emulating.
    pub fn ecam_size(&self) -> u64 {
        ecam_size_from_bus_numbers(self.start_bus, self.end_bus)
    }

    fn decode_ecam_access<'a>(&'a mut self, addr: u64) -> DecodedEcamAccess<'a> {
        let ecam_offset = match self.ecam.offset_of(addr) {
            Some(offset) => offset,
            None => {
                return DecodedEcamAccess::UnexpectedIntercept;
            }
        };

        let ecam_based_bdf = (ecam_offset >> PAGE_SHIFT) as u16;
        let bus_number = ((ecam_based_bdf >> BDF_BUS_SHIFT) as u8) + self.start_bus;
        let device_function = (ecam_based_bdf & BDF_DEVICE_FUNCTION_MASK) as u8;
        let cfg_offset_within_function = (ecam_offset & PAGE_OFFSET_MASK) as u16;

        if bus_number == self.start_bus {
            match self.ports.get_mut(&device_function) {
                Some((_, port)) => {
                    return DecodedEcamAccess::InternalBus(port, cfg_offset_within_function);
                }
                None => return DecodedEcamAccess::Unroutable,
            }
        } else if bus_number > self.start_bus && bus_number <= self.end_bus {
            for (_, port) in self.ports.values_mut() {
                if port
                    .port
                    .cfg_space
                    .assigned_bus_range()
                    .contains(&bus_number)
                {
                    return DecodedEcamAccess::DownstreamPort(
                        port,
                        bus_number,
                        device_function,
                        cfg_offset_within_function,
                    );
                }
            }
            return DecodedEcamAccess::Unroutable;
        }

        DecodedEcamAccess::UnexpectedIntercept
    }
}

fn ecam_size_from_bus_numbers(start_bus: u8, end_bus: u8) -> u64 {
    assert!(end_bus >= start_bus);
    let bus_count = (end_bus as u16) - (start_bus as u16) + 1;
    (bus_count as u64) * (MAX_FUNCTIONS_PER_BUS as u64) * PAGE_SIZE64
}

impl ChangeDeviceState for GenericPcieRootComplex {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        for (_, (_, port)) in self.ports.iter_mut() {
            port.port.cfg_space.reset();
        }
    }
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

macro_rules! check_result {
    ($result:expr) => {
        match $result {
            IoResult::Ok => (),
            res => {
                return res;
            }
        }
    };
}

impl MmioIntercept for GenericPcieRootComplex {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        validate_ecam_intercept!(addr, data);

        // N.B. Emulators internally only support 4-byte aligned accesses to
        // 4-byte registers, but the guest can use 1-, 2-, or 4 byte memory
        // instructions to access ECAM. This function reads the 4-byte aligned
        // value then shifts it around as needed before copying the data into
        // the intercept completion bytes.

        let dword_aligned_addr = addr & !3;
        let mut dword_value = !0;
        match self.decode_ecam_access(dword_aligned_addr) {
            DecodedEcamAccess::UnexpectedIntercept => {
                tracing::error!("unexpected intercept at address 0x{:16x}", addr);
            }
            DecodedEcamAccess::Unroutable => {
                tracelimit::warn_ratelimited!("unroutable config space access");
            }
            DecodedEcamAccess::InternalBus(port, cfg_offset) => {
                check_result!(port.port.cfg_space.read_u32(cfg_offset, &mut dword_value));
            }
            DecodedEcamAccess::DownstreamPort(port, bus_number, device_function, cfg_offset) => {
                check_result!(port.forward_cfg_read(
                    &bus_number,
                    &device_function,
                    cfg_offset & !3,
                    &mut dword_value,
                ));
            }
        }

        let byte_offset_within_dword = (addr & 3) as usize;
        data.copy_from_slice(
            &dword_value.as_bytes()
                [byte_offset_within_dword..byte_offset_within_dword + data.len()],
        );
        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        validate_ecam_intercept!(addr, data);

        // N.B. Emulators internally only support 4-byte aligned accesses to
        // 4-byte registers, but the guest can use 1-, 2-, or 4-byte memory
        // instructions to access ECAM. If the guest is using a 1- or 2-byte
        // instruction, this function reads the 4-byte aligned configuration
        // register, masks in the new bytes being written by the guest, and
        // uses the resulting value for write emulation.

        let dword_aligned_addr = addr & !3;
        let write_dword = match data.len() {
            4 => {
                let mut temp: u32 = 0;
                temp.as_mut_bytes().copy_from_slice(data);
                temp
            }
            _ => {
                let mut temp_bytes: [u8; 4] = [0, 0, 0, 0];
                check_result!(self.mmio_read(dword_aligned_addr, &mut temp_bytes));

                let byte_offset_within_dword = (addr & 3) as usize;
                temp_bytes[byte_offset_within_dword..byte_offset_within_dword + data.len()]
                    .copy_from_slice(data);

                let mut temp: u32 = 0;
                temp.as_mut_bytes().copy_from_slice(&temp_bytes);
                temp
            }
        };

        match self.decode_ecam_access(dword_aligned_addr) {
            DecodedEcamAccess::UnexpectedIntercept => {
                tracing::error!("unexpected intercept at address 0x{:16x}", addr);
            }
            DecodedEcamAccess::Unroutable => {
                tracelimit::warn_ratelimited!("unroutable config space access");
            }
            DecodedEcamAccess::InternalBus(port, cfg_offset) => {
                check_result!(port.port.cfg_space.write_u32(cfg_offset, write_dword));
            }
            DecodedEcamAccess::DownstreamPort(port, bus_number, device_function, cfg_offset) => {
                check_result!(port.forward_cfg_write(
                    &bus_number,
                    &device_function,
                    cfg_offset,
                    write_dword,
                ));
            }
        }

        IoResult::Ok
    }
}

#[derive(Inspect)]
struct RootPort {
    /// The common PCIe port implementation.
    #[inspect(flatten)]
    port: PcieDownstreamPort,
}

impl RootPort {
    /// Constructs a new [`RootPort`] emulator.
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        let hardware_ids = HardwareIds {
            vendor_id: VENDOR_ID,
            device_id: ROOT_PORT_DEVICE_ID,
            revision_id: 0,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::BRIDGE_PCI_TO_PCI,
            base_class: ClassCode::BRIDGE,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };
        Self {
            port: PcieDownstreamPort::new(
                name.into().to_string(),
                hardware_ids,
                DevicePortType::RootPort,
                false,
            ),
        }
    }

    /// Try to connect a PCIe device, returning an existing device name if the
    /// port is already occupied.
    fn connect_device(
        &mut self,
        name: impl AsRef<str>,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        let port_name = self.port.name.clone();
        match self.port.add_pcie_device(&port_name, name.as_ref(), dev) {
            Ok(()) => Ok(()),
            Err(_error) => {
                // If the connection failed, it means the port is already occupied
                // We need to get the name of the existing device
                if let Some((existing_name, _)) = &self.port.link {
                    Err(existing_name.clone())
                } else {
                    // This shouldn't happen if add_pcie_device works correctly
                    panic!("Port connection failed but no existing device found")
                }
            }
        }
    }

    fn forward_cfg_read(
        &mut self,
        bus: &u8,
        device_function: &u8,
        cfg_offset: u16,
        value: &mut u32,
    ) -> IoResult {
        self.port
            .forward_cfg_read_with_routing(bus, device_function, cfg_offset, value)
    }

    fn forward_cfg_write(
        &mut self,
        bus: &u8,
        device_function: &u8,
        cfg_offset: u16,
        value: u32,
    ) -> IoResult {
        self.port
            .forward_cfg_write_with_routing(bus, device_function, cfg_offset, value)
    }

    /// Gets a reference to the MSI interrupt set for this root port.
    /// This can be used to connect the root port's MSI interrupts to an interrupt controller.
    pub fn msi_set(&self) -> &pci_core::msi::MsiInterruptSet {
        self.port.msi_set()
    }

    /// Gets a mutable reference to the MSI interrupt set for this root port.
    /// This can be used to connect the root port's MSI interrupts to an interrupt controller.
    pub fn msi_set_mut(&mut self) -> &mut pci_core::msi::MsiInterruptSet {
        self.port.msi_set_mut()
    }

    /// Gets a reference to the configuration space emulator for this root port.
    pub fn cfg_space(&self) -> &pci_core::cfg_space_emu::ConfigSpaceType1Emulator {
        &self.port.cfg_space
    }

    /// Gets a mutable reference to the configuration space emulator for this root port.
    pub fn cfg_space_mut(&mut self) -> &mut pci_core::cfg_space_emu::ConfigSpaceType1Emulator {
        &mut self.port.cfg_space
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
    use pal_async::async_test;

    fn instantiate_root_complex(
        start_bus: u8,
        end_bus: u8,
        port_count: u8,
    ) -> GenericPcieRootComplex {
        let port_defs = (0..port_count)
            .map(|i| GenericPcieRootPortDefinition {
                name: format!("test-port-{}", i).into(),
            })
            .collect();

        let mut register_mmio = TestPcieMmioRegistration {};
        GenericPcieRootComplex::new(&mut register_mmio, start_bus, end_bus, 0, port_defs)
    }

    #[test]
    fn test_create() {
        assert_eq!(
            instantiate_root_complex(0, 0, 1).downstream_ports().len(),
            1
        );
        assert_eq!(
            instantiate_root_complex(0, 1, 1).downstream_ports().len(),
            1
        );
        assert_eq!(
            instantiate_root_complex(1, 1, 1).downstream_ports().len(),
            1
        );
        assert_eq!(
            instantiate_root_complex(255, 255, 1)
                .downstream_ports()
                .len(),
            1
        );

        assert_eq!(
            instantiate_root_complex(0, 0, 4).downstream_ports().len(),
            4
        );

        assert_eq!(
            instantiate_root_complex(0, 255, 32)
                .downstream_ports()
                .len(),
            32
        );
        assert_eq!(
            instantiate_root_complex(32, 32, 32)
                .downstream_ports()
                .len(),
            32
        );
        assert_eq!(
            instantiate_root_complex(255, 255, 32)
                .downstream_ports()
                .len(),
            32
        );
    }

    #[test]
    fn test_ecam_size() {
        // Single bus
        assert_eq!(instantiate_root_complex(0, 0, 0).ecam_size(), 0x10_0000);
        assert_eq!(instantiate_root_complex(32, 32, 0).ecam_size(), 0x10_0000);
        assert_eq!(instantiate_root_complex(255, 255, 0).ecam_size(), 0x10_0000);

        // Two bus
        assert_eq!(instantiate_root_complex(0, 1, 0).ecam_size(), 0x20_0000);
        assert_eq!(instantiate_root_complex(32, 33, 0).ecam_size(), 0x20_0000);
        assert_eq!(instantiate_root_complex(254, 255, 0).ecam_size(), 0x20_0000);

        // Everything
        assert_eq!(instantiate_root_complex(0, 255, 0).ecam_size(), 0x1000_0000);
    }

    #[test]
    fn test_probe_ports_via_config_space() {
        let mut rc = instantiate_root_complex(0, 255, 4);
        for device_number in 0..4 {
            let mut vendor_device: u32 = 0;
            rc.mmio_read((device_number << 3) * 4096, vendor_device.as_mut_bytes())
                .unwrap();
            assert_eq!(vendor_device, 0xC030_1414);

            let mut value_16: u16 = 0;
            rc.mmio_read((device_number << 3) * 4096, value_16.as_mut_bytes())
                .unwrap();
            assert_eq!(value_16, 0x1414);

            rc.mmio_read((device_number << 3) * 4096 + 2, value_16.as_mut_bytes())
                .unwrap();
            assert_eq!(value_16, 0xC030);
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
    fn test_add_downstream_device_to_port() {
        let mut rc = instantiate_root_complex(0, 0, 1);

        let endpoint1 = TestPcieEndpoint::new(
            |offset, value| match offset {
                0x0 => {
                    *value = 0xAAAA_AAAA;
                    Some(IoResult::Ok)
                }
                _ => Some(IoResult::Err(IoError::InvalidRegister)),
            },
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        let endpoint2 = TestPcieEndpoint::new(
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        rc.add_pcie_device(0, "ep1", Box::new(endpoint1)).unwrap();

        match rc.add_pcie_device(0, "ep2", Box::new(endpoint2)) {
            Ok(()) => panic!("should have failed"),
            Err(name) => {
                assert_eq!(name, "ep1".into());
            }
        }
    }

    #[test]
    fn test_root_port_cfg_forwarding() {
        const SECONDARY_BUS_NUM_REG: u64 = 0x19;
        const SUBOORDINATE_BUS_NUM_REG: u64 = 0x1A;

        let mut rc = instantiate_root_complex(0, 255, 1);

        // Pre-bus number assignment, random accesses return 1s.
        let mut value_32: u32 = 0;
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xFFFF_FFFF);

        // Secondary and suboordinate bus number registers are both
        // read / write, defaulting to 0.
        let mut bus_number: u8 = 0xFF;
        rc.mmio_read(SECONDARY_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 0);
        rc.mmio_read(SUBOORDINATE_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 0);

        rc.mmio_write(SECONDARY_BUS_NUM_REG, &[1]).unwrap();
        rc.mmio_read(SECONDARY_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 1);

        rc.mmio_write(SUBOORDINATE_BUS_NUM_REG, &[2]).unwrap();
        rc.mmio_read(SUBOORDINATE_BUS_NUM_REG, bus_number.as_mut_bytes())
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

        rc.add_pcie_device(0, "test-ep", Box::new(endpoint))
            .unwrap();

        // The secondary bus behind root port 0 has been assigned bus number
        // 1, so now the attached endpoint is accessible.
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xDEAD_BEEF);

        // Reassign the secondary bus number to 2.
        rc.mmio_write(SECONDARY_BUS_NUM_REG, &[2]).unwrap();
        rc.mmio_read(SECONDARY_BUS_NUM_REG, bus_number.as_mut_bytes())
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

    #[async_test]
    async fn test_reset() {
        const COMMAND_REG: u64 = 0x4;
        const COMMAND_REG_VALUE: u16 = 0x0004;
        const PORT0_ECAM: u64 = 0;
        const PORT1_ECAM: u64 = (1 << 3) * 4096;

        let mut rc = instantiate_root_complex(0, 255, 2);
        let mut value_16: u16 = 0;

        // Write the command register of both ports with a reasonable value.
        rc.mmio_write(PORT0_ECAM + COMMAND_REG, COMMAND_REG_VALUE.as_bytes())
            .unwrap();
        rc.mmio_write(PORT1_ECAM + COMMAND_REG, COMMAND_REG_VALUE.as_bytes())
            .unwrap();
        rc.mmio_read(PORT0_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, COMMAND_REG_VALUE);
        rc.mmio_read(PORT1_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, COMMAND_REG_VALUE);

        // Reset the emulator, and ensure programming was cleared.
        rc.reset().await;
        rc.mmio_read(PORT0_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, 0);
        rc.mmio_read(PORT1_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, 0);

        // Re-write the command register of both ports after reset.
        rc.mmio_write(PORT0_ECAM + COMMAND_REG, COMMAND_REG_VALUE.as_bytes())
            .unwrap();
        rc.mmio_write(PORT1_ECAM + COMMAND_REG, COMMAND_REG_VALUE.as_bytes())
            .unwrap();
        rc.mmio_read(PORT0_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, COMMAND_REG_VALUE);
        rc.mmio_read(PORT1_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, COMMAND_REG_VALUE);
    }

    #[test]
    fn test_root_port_invalid_bus_range_handling() {
        let mut root_port = RootPort::new("test-port");

        // Don't configure bus numbers, so the range should be 0..=0 (invalid)
        let bus_range = root_port.port.cfg_space.assigned_bus_range();
        assert_eq!(bus_range, 0..=0);

        // Test that forwarding returns Ok but doesn't crash when bus range is invalid
        let mut value = 0u32;
        let result = root_port
            .port
            .forward_cfg_read_with_routing(&1, &0, 0x0, &mut value);
        assert!(matches!(result, IoResult::Ok));

        let result = root_port
            .port
            .forward_cfg_write_with_routing(&1, &0, 0x0, value);
        assert!(matches!(result, IoResult::Ok));
    }
}
