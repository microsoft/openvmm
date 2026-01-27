// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common PCIe port implementation shared between different port types.

use anyhow::bail;
use chipset_device::io::IoResult;
use inspect::Inspect;
use pci_bus::GenericPciBusDevice;
use pci_core::capabilities::msi_cap::MsiCapability;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
use pci_core::msi::MsiConnection;
use pci_core::spec::caps::pci_express::DevicePortType;
use pci_core::spec::hwid::HardwareIds;
use std::sync::Arc;

/// A common PCIe downstream facing port implementation that handles device connections and configuration forwarding.
///
/// This struct contains the common functionality shared between RootPort and DownstreamSwitchPort,
/// including device connection management and configuration space forwarding logic.
#[derive(Inspect)]
pub struct PcieDownstreamPort {
    /// The name of this port.
    pub name: String,

    /// The configuration space emulator for this port.
    pub cfg_space: ConfigSpaceType1Emulator,

    /// The connected device, if any.
    #[inspect(skip)]
    pub link: Option<(Arc<str>, Box<dyn GenericPciBusDevice>)>,
}

impl PcieDownstreamPort {
    /// Creates a new PCIe port with the specified hardware configuration and optional multi-function flag.
    ///
    /// # Arguments
    /// * `name` - The name for this port
    /// * `hardware_ids` - Hardware identifiers for the port
    /// * `port_type` - The PCIe port type (root port, downstream switch port, etc.)
    /// * `multi_function` - Whether this port should have the multi-function flag set
    /// * `hotplug_slot_number` - The slot number for hotplug support. `Some(slot_number)` enables hotplug, `None` disables it
    pub fn new(
        name: impl Into<String>,
        hardware_ids: HardwareIds,
        port_type: DevicePortType,
        multi_function: bool,
        hotplug_slot_number: Option<u32>,
    ) -> Self {
        let port_name = name.into();

        let (hotplug, slot_number) = match hotplug_slot_number {
            Some(slot) => (true, Some(slot)),
            None => (false, None),
        };

        let msi_conn = MsiConnection::new();
        // Create MSI capability with 1 message (multiple_message_capable=0), 64-bit addressing, no per-vector masking
        let msi_capability = MsiCapability::new(0, true, false, msi_conn.target());

        let pcie_cap = if hotplug {
            let slot_num = slot_number.unwrap_or(0);
            PciExpressCapability::new(port_type, None).with_hotplug_support(slot_num)
        } else {
            PciExpressCapability::new(port_type, None)
        };

        let cfg_space = ConfigSpaceType1Emulator::new(
            hardware_ids,
            vec![Box::new(pcie_cap), Box::new(msi_capability)],
        )
        .with_multi_function_bit(multi_function);

        Self {
            name: port_name,
            cfg_space,
            link: None,
        }
    }

    /// Forward a configuration space read to the connected device.
    /// Supports routing components for multi-level hierarchies.
    pub fn forward_cfg_read_with_routing(
        &mut self,
        bus: &u8,
        device_function: &u8,
        cfg_offset: u16,
        value: &mut u32,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration
        if bus_range == (0..=0) {
            tracelimit::warn_ratelimited!("invalid access: port bus number range not configured");
            return IoResult::Ok;
        }

        if *bus == *bus_range.start() {
            // Perform type-0 access to the child device's config space.
            if *device_function == 0 {
                if let Some((_, device)) = &mut self.link {
                    let result = device.pci_cfg_read(cfg_offset, value);

                    if let Some(result) = result {
                        match result {
                            IoResult::Ok => (),
                            res => return res,
                        }
                    }
                }
            } else {
                tracelimit::warn_ratelimited!(
                    "invalid access: multi-function device access not supported for now"
                );
                return IoResult::Ok;
            }
        } else if bus_range.contains(bus) {
            if let Some((_, device)) = &mut self.link {
                // Forward access to the linked device.
                let result = device.pci_cfg_read_forward(*bus, *device_function, cfg_offset, value);

                if let Some(result) = result {
                    match result {
                        IoResult::Ok => (),
                        res => return res,
                    }
                }
            } else {
                tracelimit::warn_ratelimited!(
                    "invalid access: bus number to access not within port's bus number range"
                );
            }
        }

        IoResult::Ok
    }

    /// Forward a configuration space write to the connected device.
    /// Supports routing components for multi-level hierarchies.
    pub fn forward_cfg_write_with_routing(
        &mut self,
        bus: &u8,
        device_function: &u8,
        cfg_offset: u16,
        value: u32,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration
        if bus_range == (0..=0) {
            tracelimit::warn_ratelimited!("invalid access: port bus number range not configured");
            return IoResult::Ok;
        }

        if *bus == *bus_range.start() {
            // Perform type-0 access to the child device's config space.
            if *device_function == 0 {
                if let Some((_, device)) = &mut self.link {
                    let result = device.pci_cfg_write(cfg_offset, value);

                    if let Some(result) = result {
                        match result {
                            IoResult::Ok => (),
                            res => return res,
                        }
                    }
                }
            } else {
                tracelimit::warn_ratelimited!(
                    "invalid access: multi-function device access not supported for now"
                );
                return IoResult::Ok;
            }
        } else if bus_range.contains(bus) {
            if let Some((_, device)) = &mut self.link {
                // Forward access to the linked device.
                let result =
                    device.pci_cfg_write_forward(*bus, *device_function, cfg_offset, value);

                if let Some(result) = result {
                    match result {
                        IoResult::Ok => (),
                        res => return res,
                    }
                }
            } else {
                tracelimit::warn_ratelimited!(
                    "invalid access: bus number to access not within port's bus number range"
                );
            }
        }

        IoResult::Ok
    }

    /// Connect a device to this specific port by exact name match.
    pub fn add_pcie_device(
        &mut self,
        port_name: &str,
        device_name: &str,
        device: Box<dyn GenericPciBusDevice>,
    ) -> anyhow::Result<()> {
        // Only connect if the name exactly matches this port's name
        if port_name == self.name.as_str() {
            // Check if there's already a device connected
            if self.link.is_some() {
                bail!("port is already occupied");
            }

            // Connect the device to this port
            self.link = Some((device_name.into(), device));

            // Set presence detect state to true when a device is connected
            self.cfg_space.set_presence_detect_state(true);

            return Ok(());
        }

        // If the name doesn't match, fail immediately (no forwarding)
        bail!("port name does not match")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::io::IoResult;
    use pci_bus::GenericPciBusDevice;
    use pci_core::spec::hwid::HardwareIds;

    // Mock device for testing
    struct MockDevice;

    impl GenericPciBusDevice for MockDevice {
        fn pci_cfg_read(&mut self, _offset: u16, _value: &mut u32) -> Option<IoResult> {
            None
        }

        fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> Option<IoResult> {
            None
        }

        fn pci_cfg_read_forward(
            &mut self,
            _bus: u8,
            _device_function: u8,
            _offset: u16,
            _value: &mut u32,
        ) -> Option<IoResult> {
            None
        }

        fn pci_cfg_write_forward(
            &mut self,
            _bus: u8,
            _device_function: u8,
            _offset: u16,
            _value: u32,
        ) -> Option<IoResult> {
            None
        }
    }

    #[test]
    fn test_add_pcie_device_sets_presence_detect_state() {
        use pci_core::spec::hwid::{ClassCode, ProgrammingInterface, Subclass};

        // Create a port with hotplug support
        let hardware_ids = HardwareIds {
            vendor_id: 0x1234,
            device_id: 0x5678,
            revision_id: 0,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::BRIDGE_PCI_TO_PCI,
            base_class: ClassCode::BRIDGE,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        let mut port = PcieDownstreamPort::new(
            "test-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            Some(1), // Enable hotplug with slot number 1
        );

        // Initially, presence detect state should be 0
        let mut slot_status_val = 0u32;
        let result = port.cfg_space.read_u32(0x58, &mut slot_status_val); // 0x40 (cap start) + 0x18 (slot control/status)
        assert!(matches!(result, IoResult::Ok));
        let initial_presence_detect = (slot_status_val >> 22) & 0x1; // presence_detect_state is bit 6 of slot status
        assert_eq!(
            initial_presence_detect, 0,
            "Initial presence detect state should be 0"
        );

        // Add a device to the port
        let mock_device = Box::new(MockDevice);
        let result = port.add_pcie_device("test-port", "mock-device", mock_device);
        assert!(result.is_ok(), "Adding device should succeed");

        // Check that presence detect state is now 1
        let result = port.cfg_space.read_u32(0x58, &mut slot_status_val);
        assert!(matches!(result, IoResult::Ok));
        let present_presence_detect = (slot_status_val >> 22) & 0x1;
        assert_eq!(
            present_presence_detect, 1,
            "Presence detect state should be 1 after adding device"
        );
    }

    #[test]
    fn test_add_pcie_device_without_hotplug() {
        use pci_core::spec::hwid::{ClassCode, ProgrammingInterface, Subclass};

        // Create a port without hotplug support
        let hardware_ids = HardwareIds {
            vendor_id: 0x1234,
            device_id: 0x5678,
            revision_id: 0,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::BRIDGE_PCI_TO_PCI,
            base_class: ClassCode::BRIDGE,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        let mut port = PcieDownstreamPort::new(
            "test-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None, // No hotplug
        );

        // Add a device to the port (should not panic even without hotplug support)
        let mock_device = Box::new(MockDevice);
        let result = port.add_pcie_device("test-port", "mock-device", mock_device);
        assert!(
            result.is_ok(),
            "Adding device should succeed even without hotplug support"
        );
    }
}
