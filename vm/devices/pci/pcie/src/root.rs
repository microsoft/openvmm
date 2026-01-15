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
    /// Whether hotplug is enabled for this root port.
    pub hotplug: bool,
}

/// A flat description of a PCIe switch without hierarchy.
pub struct GenericSwitchDefinition {
    /// The name of the switch.
    pub name: Arc<str>,
    /// Number of downstream ports.
    pub num_downstream_ports: u8,
    /// The parent port this switch is connected to.
    pub parent_port: Arc<str>,
    /// Whether hotplug is enabled for this switch.
    pub hotplug: bool,
}

impl GenericSwitchDefinition {
    /// Create a new switch definition.
    pub fn new(
        name: impl Into<Arc<str>>,
        num_downstream_ports: u8,
        parent_port: impl Into<Arc<str>>,
        hotplug: bool,
    ) -> Self {
        Self {
            name: name.into(),
            num_downstream_ports,
            parent_port: parent_port.into(),
            hotplug,
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
                // Use the device number as the slot number for hotpluggable ports
                let hotplug_slot_number = if definition.hotplug {
                    Some((device_number as u32) + 1)
                } else {
                    None
                };
                let root_port = RootPort::new(definition.name.clone(), hotplug_slot_number);
                (device_number, (definition.name, root_port))
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
        let (_port_name, root_port) = self.ports.get_mut(&port).ok_or_else(|| -> Arc<str> {
            tracing::error!(
                "GenericPcieRootComplex: port {:#x} not found for device '{}'",
                port,
                name.as_ref()
            );
            format!("Port {:#x} not found", port).into()
        })?;

        match root_port.connect_device(name, dev) {
            Ok(()) => Ok(()),
            Err(existing_device) => {
                tracing::warn!(
                    "GenericPcieRootComplex: failed to connect device to port {:#x}, existing device: '{}'",
                    port,
                    existing_device
                );
                Err(existing_device)
            }
        }
    }

    /// Enumerate the downstream ports of the root complex.
    pub fn downstream_ports(&self) -> Vec<(u8, Arc<str>)> {
        let ports: Vec<(u8, Arc<str>)> = self
            .ports
            .iter()
            .map(|(port, (name, _))| (*port, name.clone()))
            .collect();

        ports
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
    ///
    /// # Arguments
    /// * `name` - The name for this root port
    /// * `hotplug_slot_number` - The slot number for hotplug support. `Some(slot_number)` enables hotplug, `None` disables it
    pub fn new(name: impl Into<Arc<str>>, hotplug_slot_number: Option<u32>) -> Self {
        let name_str = name.into();
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

        let port = PcieDownstreamPort::new(
            name_str.to_string(),
            hardware_ids,
            DevicePortType::RootPort,
            false,
            hotplug_slot_number,
        );

        Self { port }
    }

    /// Try to connect a PCIe device, returning an existing device name if the
    /// port is already occupied.
    fn connect_device(
        &mut self,
        name: impl AsRef<str>,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        let device_name = name.as_ref();
        let port_name = self.port.name.clone();

        match self.port.add_pcie_device(&port_name, device_name, dev) {
            Ok(()) => Ok(()),
            Err(_error) => {
                // If the connection failed, it means the port is already occupied
                // We need to get the name of the existing device
                if let Some((existing_name, _)) = &self.port.link {
                    tracing::warn!(
                        "RootPort: '{}' failed to connect device '{}', port already occupied by '{}'",
                        port_name,
                        device_name,
                        existing_name
                    );
                    Err(existing_name.clone())
                } else {
                    // This shouldn't happen if add_pcie_device works correctly
                    tracing::error!(
                        "RootPort: '{}' connection failed for device '{}' but no existing device found",
                        port_name,
                        device_name
                    );
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
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh_protobuf::Protobuf;
        use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        /// Saved state for the GenericPcieRootComplex.
        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pcie.root")]
        pub struct SavedState {
            /// The lowest valid bus number under the root complex.
            #[mesh(1)]
            pub start_bus: u8,
            /// The highest valid bus number under the root complex.
            #[mesh(2)]
            pub end_bus: u8,
            /// The root port configuration space states.
            ///
            /// Port numbers correspond to PCIe device numbers that are multiples of 8
            /// (0, 8, 16, ...). The index into this vector is the zero-based port
            /// index (0, 1, 2, ...). The actual device/port number for a given entry
            /// can be obtained by `port_device_number = index * 8`.
            #[mesh(3)]
            pub root_ports: Vec<<ConfigSpaceType1Emulator as SaveRestore>::SavedState>,
        }
    }

    impl SaveRestore for GenericPcieRootComplex {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            // Save all root ports in order by port number
            let port_count = self.ports.len();
            let mut root_ports = Vec::with_capacity(port_count);

            // Get all port numbers and sort them for consistent ordering
            let mut port_numbers: Vec<u8> = self.ports.keys().copied().collect();
            port_numbers.sort();

            for port_number in port_numbers {
                if let Some((_, root_port)) = self.ports.get_mut(&port_number) {
                    root_ports.push(root_port.port.cfg_space.save()?);
                } else {
                    return Err(SaveError::NotFound(format!("root port {}", port_number)));
                }
            }

            Ok(state::SavedState {
                start_bus: self.start_bus,
                end_bus: self.end_bus,
                root_ports,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                start_bus,
                end_bus,
                root_ports,
            } = state;

            // Validate that bus numbers match
            if start_bus != self.start_bus || end_bus != self.end_bus {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "bus number mismatch: saved ({}-{}), current ({}-{})",
                    start_bus,
                    end_bus,
                    self.start_bus,
                    self.end_bus
                )));
            }

            // Get all port numbers and sort them for consistent ordering
            let mut port_numbers: Vec<u8> = self.ports.keys().copied().collect();
            port_numbers.sort();

            // Validate port count matches
            if root_ports.len() != port_numbers.len() {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "root port count mismatch: saved {}, current {}",
                    root_ports.len(),
                    port_numbers.len()
                )));
            }

            // Restore all root ports (index corresponds to sorted port numbers)
            for (cfg_space, port_number) in root_ports.into_iter().zip(port_numbers.iter()) {
                if let Some((_, root_port)) = self.ports.get_mut(port_number) {
                    root_port.port.cfg_space.restore(cfg_space)?;
                } else {
                    return Err(RestoreError::NotFound(format!("root port {}", port_number)));
                }
            }

            Ok(())
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
                hotplug: false,
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
    fn test_root_port_hotplug_options() {
        // Test with hotplug disabled (None)
        let root_port_no_hotplug = RootPort::new("test-port-no-hotplug", None);
        // We can't easily verify hotplug is disabled without accessing internal state,
        // but we can verify the port was created successfully
        let mut vendor_device_id: u32 = 0;
        root_port_no_hotplug
            .port
            .cfg_space
            .read_u32(0x0, &mut vendor_device_id)
            .unwrap();
        let expected = (ROOT_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);

        // Test with hotplug enabled (Some(slot_number))
        let root_port_with_hotplug = RootPort::new("test-port-hotplug", Some(5));
        let mut vendor_device_id_hotplug: u32 = 0;
        root_port_with_hotplug
            .port
            .cfg_space
            .read_u32(0x0, &mut vendor_device_id_hotplug)
            .unwrap();
        assert_eq!(vendor_device_id_hotplug, expected);
        // The slot number and hotplug capability would be tested via PCIe capability registers
        // but that requires more complex setup
    }

    #[test]
    fn test_root_port_invalid_bus_range_handling() {
        let mut root_port = RootPort::new("test-port", None);

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

    #[test]
    fn test_save_restore_basic() {
        use vmcore::save_restore::SaveRestore;

        let mut rc = instantiate_root_complex(0, 255, 4);

        // Save the initial state
        let saved_state = rc.save().expect("save should succeed");

        // Verify the saved state has the correct values
        assert_eq!(saved_state.start_bus, 0);
        assert_eq!(saved_state.end_bus, 255);
        assert_eq!(saved_state.root_ports.len(), 4);

        // Restore the state to a new root complex
        let mut rc2 = instantiate_root_complex(0, 255, 4);
        rc2.restore(saved_state).expect("restore should succeed");
    }

    #[test]
    fn test_save_restore_with_bus_configuration() {
        use vmcore::save_restore::SaveRestore;
        use zerocopy::IntoBytes;

        const SECONDARY_BUS_NUM_REG: u64 = 0x19;
        const SUBORDINATE_BUS_NUM_REG: u64 = 0x1A;

        let mut rc = instantiate_root_complex(0, 255, 2);

        // Configure bus numbers on port 0
        rc.mmio_write(SECONDARY_BUS_NUM_REG, &[1]).unwrap();
        rc.mmio_write(SUBORDINATE_BUS_NUM_REG, &[10]).unwrap();

        // Configure bus numbers on port 1 (at device 1, so offset by 8*4096)
        const PORT1_ECAM: u64 = (1 << 3) * 4096;
        rc.mmio_write(PORT1_ECAM + SECONDARY_BUS_NUM_REG, &[11])
            .unwrap();
        rc.mmio_write(PORT1_ECAM + SUBORDINATE_BUS_NUM_REG, &[20])
            .unwrap();

        // Verify the bus numbers are set
        let mut bus_number: u8 = 0;
        rc.mmio_read(SECONDARY_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 1);
        rc.mmio_read(SUBORDINATE_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 10);

        rc.mmio_read(
            PORT1_ECAM + SECONDARY_BUS_NUM_REG,
            bus_number.as_mut_bytes(),
        )
        .unwrap();
        assert_eq!(bus_number, 11);
        rc.mmio_read(
            PORT1_ECAM + SUBORDINATE_BUS_NUM_REG,
            bus_number.as_mut_bytes(),
        )
        .unwrap();
        assert_eq!(bus_number, 20);

        // Save the state
        let saved_state = rc.save().expect("save should succeed");

        // Create a new root complex and restore the state
        let mut rc2 = instantiate_root_complex(0, 255, 2);

        // Verify the new root complex has default bus numbers before restore
        let mut default_bus: u8 = 0xFF;
        rc2.mmio_read(SECONDARY_BUS_NUM_REG, default_bus.as_mut_bytes())
            .unwrap();
        assert_eq!(default_bus, 0);

        // Restore the state
        rc2.restore(saved_state).expect("restore should succeed");

        // Verify the bus numbers are restored
        let mut restored_bus: u8 = 0;
        rc2.mmio_read(SECONDARY_BUS_NUM_REG, restored_bus.as_mut_bytes())
            .unwrap();
        assert_eq!(restored_bus, 1);
        rc2.mmio_read(SUBORDINATE_BUS_NUM_REG, restored_bus.as_mut_bytes())
            .unwrap();
        assert_eq!(restored_bus, 10);

        rc2.mmio_read(
            PORT1_ECAM + SECONDARY_BUS_NUM_REG,
            restored_bus.as_mut_bytes(),
        )
        .unwrap();
        assert_eq!(restored_bus, 11);
        rc2.mmio_read(
            PORT1_ECAM + SUBORDINATE_BUS_NUM_REG,
            restored_bus.as_mut_bytes(),
        )
        .unwrap();
        assert_eq!(restored_bus, 20);
    }

    #[test]
    fn test_save_restore_bus_number_mismatch_error() {
        use vmcore::save_restore::SaveRestore;

        // Create a root complex with specific bus range
        let mut rc = instantiate_root_complex(0, 255, 2);

        // Save the state
        let saved_state = rc.save().expect("save should succeed");

        // Create a new root complex with different bus range
        let mut rc2 = instantiate_root_complex(0, 127, 2);

        // Restore should fail because bus numbers don't match
        let result = rc2.restore(saved_state);
        assert!(result.is_err());
    }

    #[test]
    fn test_save_restore_port_count_mismatch_error() {
        use vmcore::save_restore::SaveRestore;

        // Create a root complex with 4 ports
        let mut rc = instantiate_root_complex(0, 255, 4);

        // Save the state
        let saved_state = rc.save().expect("save should succeed");
        assert_eq!(saved_state.root_ports.len(), 4);

        // Create a new root complex with only 2 ports
        let mut rc2 = instantiate_root_complex(0, 255, 2);

        // Restore should fail because port counts don't match
        let result = rc2.restore(saved_state);
        assert!(result.is_err());
    }
}
