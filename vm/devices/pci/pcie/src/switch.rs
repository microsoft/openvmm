// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express switch port emulation.
//!
//! This module provides emulation for PCIe switch ports:
//! - [`UpstreamSwitchPort`]: Connects a switch to its parent (root port or another switch)
//! - [`DownstreamSwitchPort`]: Connects a switch to its children (endpoints or other switches)
//!
//! Both port types implement Type 1 PCI-to-PCI bridge functionality with appropriate
//! PCIe capabilities indicating their port type.

use crate::DOWNSTREAM_SWITCH_PORT_DEVICE_ID;
use crate::UPSTREAM_SWITCH_PORT_DEVICE_ID;
use crate::VENDOR_ID;
use crate::port::PciePort;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use inspect::Inspect;
use inspect::InspectMut;
use pci_bus::{GenericPciBusDevice, GenericPciRoutingComponent};
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
use pci_core::spec::caps::pci_express::DevicePortType;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::collections::HashMap;
use std::sync::Arc;
use vmcore::device_state::ChangeDeviceState;

/// A PCI Express upstream switch port emulator.
///
/// An upstream switch port connects a switch to its parent (e.g., root port or another switch).
/// It appears as a Type 1 PCI-to-PCI bridge with PCIe capability indicating it's an upstream switch port.
#[derive(Inspect)]
pub struct UpstreamSwitchPort {
    cfg_space: ConfigSpaceType1Emulator,
}

impl UpstreamSwitchPort {
    /// Constructs a new [`UpstreamSwitchPort`] emulator.
    pub fn new() -> Self {
        let cfg_space = ConfigSpaceType1Emulator::new(
            HardwareIds {
                vendor_id: VENDOR_ID,
                device_id: UPSTREAM_SWITCH_PORT_DEVICE_ID,
                revision_id: 0,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::BRIDGE_PCI_TO_PCI,
                base_class: ClassCode::BRIDGE,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(PciExpressCapability::new(
                DevicePortType::UpstreamSwitchPort,
                None,
            ))],
        );
        Self { cfg_space }
    }

    /// Get a reference to the configuration space emulator.
    pub fn cfg_space(&self) -> &ConfigSpaceType1Emulator {
        &self.cfg_space
    }

    /// Get a mutable reference to the configuration space emulator.
    pub fn cfg_space_mut(&mut self) -> &mut ConfigSpaceType1Emulator {
        &mut self.cfg_space
    }
}

impl Default for UpstreamSwitchPort {
    fn default() -> Self {
        Self::new()
    }
}

/// A PCI Express downstream switch port emulator.
///
/// A downstream switch port connects a switch to its children (e.g., endpoints or other switches).
/// It appears as a Type 1 PCI-to-PCI bridge with PCIe capability indicating it's a downstream switch port.
#[derive(Inspect)]
pub struct DownstreamSwitchPort {
    /// The common PCIe port implementation.
    #[inspect(flatten)]
    port: PciePort,
}

impl DownstreamSwitchPort {
    /// Constructs a new [`DownstreamSwitchPort`] emulator.
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self::new_with_multi_function(name, false)
    }

    /// Constructs a new [`DownstreamSwitchPort`] emulator with multi-function flag.
    pub fn new_with_multi_function(name: impl Into<Arc<str>>, multi_function: bool) -> Self {
        let hardware_ids = HardwareIds {
            vendor_id: VENDOR_ID,
            device_id: DOWNSTREAM_SWITCH_PORT_DEVICE_ID,
            revision_id: 0,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::BRIDGE_PCI_TO_PCI,
            base_class: ClassCode::BRIDGE,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };
        Self {
            port: PciePort::new_with_multi_function(
                name,
                hardware_ids,
                DevicePortType::DownstreamSwitchPort,
                multi_function,
            ),
        }
    }

    /// Get a reference to the configuration space emulator.
    pub fn cfg_space(&self) -> &ConfigSpaceType1Emulator {
        &self.port.cfg_space
    }

    /// Get a mutable reference to the configuration space emulator.
    pub fn cfg_space_mut(&mut self) -> &mut ConfigSpaceType1Emulator {
        &mut self.port.cfg_space
    }

    /// Get a mutable reference to the underlying PCIe port.
    pub fn port_mut(&mut self) -> &mut PciePort {
        &mut self.port
    }
}

impl Default for DownstreamSwitchPort {
    fn default() -> Self {
        Self::new("default-downstream-port")
    }
}

/// A PCI Express switch definition used for creating switch instances.
pub struct GenericPcieSwitchDefinition {
    /// The name of the switch.
    pub name: Arc<str>,
    /// The number of downstream ports to create.
    /// TODO: implement physical slot number, link and slot stuff
    pub downstream_port_count: usize,
}

/// A PCI Express switch emulator that implements a complete switch with upstream and downstream ports.
///
/// A PCIe switch consists of:
/// - One upstream switch port that connects to the parent (root port or another switch)
/// - Multiple downstream switch ports that connect to children (endpoints or other switches)
///
/// The switch implements routing functionality to forward configuration space accesses
/// between the upstream and downstream ports based on bus number assignments.
#[derive(InspectMut)]
pub struct GenericPcieSwitch {
    /// The name of this switch instance.
    name: Arc<str>,
    /// The upstream switch port that connects to the parent.
    upstream_port: UpstreamSwitchPort,
    /// Map of downstream switch ports, indexed by port number.
    #[inspect(with = "|x| inspect::iter_by_key(x).map_value(|(_, v)| v)")]
    downstream_ports: HashMap<u8, (Arc<str>, DownstreamSwitchPort)>,
}

impl GenericPcieSwitch {
    /// Constructs a new [`GenericPcieSwitch`] emulator.
    pub fn new(definition: GenericPcieSwitchDefinition) -> Self {
        let upstream_port = UpstreamSwitchPort::new();

        // If there are multiple downstream ports, they need the multi-function flag set
        let multi_function = definition.downstream_port_count > 1;

        let downstream_ports = (0..definition.downstream_port_count)
            .map(|i| {
                let port_name = format!("{}-downstream-{}", definition.name, i);
                let port = DownstreamSwitchPort::new_with_multi_function(
                    port_name.clone(),
                    multi_function,
                );
                (i as u8, (port_name.into(), port))
            })
            .collect();

        Self {
            name: definition.name,
            upstream_port,
            downstream_ports,
        }
    }

    /// Get the name of this switch.
    pub fn name(&self) -> &Arc<str> {
        &self.name
    }

    /// Get a reference to the upstream switch port.
    pub fn upstream_port(&self) -> &UpstreamSwitchPort {
        &self.upstream_port
    }

    /// Enumerate the downstream ports of the switch.
    pub fn downstream_ports(&self) -> Vec<(u8, Arc<str>)> {
        self.downstream_ports
            .iter()
            .map(|(port, (name, _))| (*port, name.clone()))
            .collect()
    }

    /// Route configuration space access to the appropriate port based on addressing.
    fn route_cfg_access(
        &mut self,
        bus: u8,
        device_function: u8,
        is_read: bool,
        cfg_offset: u16,
        value: &mut u32,
    ) -> Option<IoResult> {
        let upstream_bus_range = self.upstream_port.cfg_space().assigned_bus_range();

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration
        if upstream_bus_range == (0..=0) {
            return None;
        }

        // Only handle accesses within our decoded bus range
        if !upstream_bus_range.contains(&bus) {
            return None;
        }

        let secondary_bus = *upstream_bus_range.start();

        // Direct access to downstream switch ports on the secondary bus
        if bus == secondary_bus {
            return self.handle_downstream_port_access(device_function, is_read, cfg_offset, value);
        }

        // Route to downstream ports for further forwarding
        self.route_to_downstream_ports(bus, device_function, is_read, cfg_offset, value)
    }

    /// Handle direct configuration space access to downstream switch ports.
    fn handle_downstream_port_access(
        &mut self,
        device_function: u8,
        is_read: bool,
        cfg_offset: u16,
        value: &mut u32,
    ) -> Option<IoResult> {
        if let Some((_, downstream_port)) = self.downstream_ports.get_mut(&device_function) {
            Some(if is_read {
                downstream_port.port.cfg_space.read_u32(cfg_offset, value)
            } else {
                downstream_port.port.cfg_space.write_u32(cfg_offset, *value)
            })
        } else {
            // No downstream switch port found for this device function
            None
        }
    }

    /// Route configuration space access to downstream ports for further forwarding.
    fn route_to_downstream_ports(
        &mut self,
        bus: u8,
        device_function: u8,
        is_read: bool,
        cfg_offset: u16,
        value: &mut u32,
    ) -> Option<IoResult> {
        for (_, downstream_port) in self.downstream_ports.values_mut() {
            let downstream_bus_range = downstream_port.cfg_space().assigned_bus_range();

            // Skip downstream ports with invalid/uninitialized bus configuration
            if downstream_bus_range == (0..=0) {
                continue;
            }

            if downstream_bus_range.contains(&bus) {
                return Some(downstream_port.port.forward_cfg_access_with_routing(
                    &bus,
                    &device_function,
                    is_read,
                    cfg_offset,
                    value,
                ));
            }
        }

        // No downstream port could handle this bus number
        None
    }
}

impl GenericPciBusDevice for GenericPcieSwitch {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> Option<IoResult> {
        // Forward to the upstream port's configuration space (the switch presents as the upstream port)
        self.upstream_port.cfg_space.read_u32(offset, value).into()
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> Option<IoResult> {
        // Forward to the upstream port's configuration space (the switch presents as the upstream port)
        self.upstream_port.cfg_space.write_u32(offset, value).into()
    }

    fn as_routing_component(&mut self) -> Option<&mut dyn GenericPciRoutingComponent> {
        Some(self)
    }
}

impl GenericPciRoutingComponent for GenericPcieSwitch {
    fn pci_cfg_read_forward(
        &mut self,
        bus: u8,
        device_function: u8,
        offset: u16,
        value: &mut u32,
    ) -> Option<IoResult> {
        self.route_cfg_access(bus, device_function, true, offset, value)
    }

    fn pci_cfg_write_forward(
        &mut self,
        bus: u8,
        device_function: u8,
        offset: u16,
        value: u32,
    ) -> Option<IoResult> {
        let mut temp_value = value;
        self.route_cfg_access(bus, device_function, false, offset, &mut temp_value)
    }

    fn add_pcie_device(
        &mut self,
        port: u8,
        name: &str,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        // Find the specific downstream port that matches the port number
        if let Some((port_name, downstream_port)) = self.downstream_ports.get_mut(&port) {
            // Found the matching port, try to connect to it using the port's name
            downstream_port
                .port
                .add_pcie_device(port_name.as_ref(), name, dev)
        } else {
            // No downstream port found with matching port number
            Err(format!("Port {} not found", port).into())
        }
    }
}

impl Default for GenericPcieSwitch {
    fn default() -> Self {
        Self::new(GenericPcieSwitchDefinition {
            name: "default-switch".into(),
            downstream_port_count: 4,
        })
    }
}

impl ChangeDeviceState for GenericPcieSwitch {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        // Reset the upstream port configuration space
        self.upstream_port.cfg_space.reset();

        // Reset all downstream port configuration spaces
        for (_, downstream_port) in self.downstream_ports.values_mut() {
            downstream_port.port.cfg_space.reset();
        }
    }
}

impl ChipsetDevice for GenericPcieSwitch {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for GenericPcieSwitch {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        // Delegate to the GenericPciBusDevice implementation
        GenericPciBusDevice::pci_cfg_read(self, offset, value)
            .unwrap_or(IoResult::Err(chipset_device::io::IoError::InvalidRegister))
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        // Delegate to the GenericPciBusDevice implementation
        GenericPciBusDevice::pci_cfg_write(self, offset, value)
            .unwrap_or(IoResult::Err(chipset_device::io::IoError::InvalidRegister))
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        // PCIe switches typically don't have a fixed BDF requirement
        None
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateNotSupported;

    impl SaveRestore for GenericPcieSwitch {
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

    #[test]
    fn test_upstream_switch_port_creation() {
        let port = UpstreamSwitchPort::new();

        // Verify that we can read the vendor/device ID from config space
        let mut vendor_device_id: u32 = 0;
        port.cfg_space.read_u32(0x0, &mut vendor_device_id).unwrap();
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);
    }

    #[test]
    fn test_downstream_switch_port_creation() {
        let port = DownstreamSwitchPort::new("test-downstream-port");
        assert!(port.port.link.is_none());

        // Verify that we can read the vendor/device ID from config space
        let mut vendor_device_id: u32 = 0;
        port.port
            .cfg_space
            .read_u32(0x0, &mut vendor_device_id)
            .unwrap();
        let expected = (DOWNSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);
    }

    #[test]
    fn test_switch_creation() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 3,
        };
        let switch = GenericPcieSwitch::new(definition);

        assert_eq!(switch.name().as_ref(), "test-switch");
        assert_eq!(switch.downstream_ports().len(), 3);

        // Verify downstream port names (HashMap doesn't guarantee order, so check each one exists)
        let ports = switch.downstream_ports();
        let port_names: std::collections::HashSet<_> =
            ports.iter().map(|(_, name)| name.as_ref()).collect();
        assert!(port_names.contains("test-switch-downstream-0"));
        assert!(port_names.contains("test-switch-downstream-1"));
        assert!(port_names.contains("test-switch-downstream-2"));

        // Verify port numbers
        let port_numbers: std::collections::HashSet<_> =
            ports.iter().map(|(num, _)| *num).collect();
        assert!(port_numbers.contains(&0));
        assert!(port_numbers.contains(&1));
        assert!(port_numbers.contains(&2));
    }

    #[test]
    fn test_switch_device_connections() {
        use crate::test_helpers::TestPcieEndpoint;
        use chipset_device::io::IoError;

        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        let downstream_device = TestPcieEndpoint::new(
            |offset, value| match offset {
                0x0 => {
                    *value = 0xABCD_EF01;
                    Some(IoResult::Ok)
                }
                _ => Some(IoResult::Err(IoError::InvalidRegister)),
            },
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        // Connect downstream device to port 0
        assert!(
            switch
                .add_pcie_device(
                    0, // Port number instead of port name
                    "downstream-dev",
                    Box::new(downstream_device)
                )
                .is_ok()
        );

        // Try to connect to invalid port
        let invalid_device = TestPcieEndpoint::new(
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );
        let result = switch.add_pcie_device(99, "invalid-dev", Box::new(invalid_device)); // Use invalid port number
        assert!(result.is_err());
        // add_pcie_device returns an error string on failure,
        // so we just verify that the connection failed
        assert!(result.is_err());
    }

    #[test]
    fn test_switch_as_routing_component() {
        let definition = GenericPcieSwitchDefinition {
            name: "routing-switch".into(),
            downstream_port_count: 1,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Verify that Switch implements GenericPciRoutingComponent
        assert!(switch.as_routing_component().is_some());

        // Test basic configuration space access
        let mut value = 0u32;
        let result = GenericPciBusDevice::pci_cfg_read(&mut switch, 0x0, &mut value);
        assert!(result.is_some());

        // Verify vendor/device ID is from the upstream port
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(value, expected);
    }

    #[test]
    fn test_switch_chipset_device() {
        use chipset_device::ChipsetDevice;
        use chipset_device::pci::PciConfigSpace;

        let mut switch = GenericPcieSwitch::default();

        // Test that it supports PCI but not other interfaces
        assert!(switch.supports_pci().is_some());
        assert!(switch.supports_mmio().is_none());
        assert!(switch.supports_pio().is_none());
        assert!(switch.supports_poll_device().is_none());

        // Test PciConfigSpace interface
        let mut value = 0u32;
        let result = PciConfigSpace::pci_cfg_read(&mut switch, 0x0, &mut value);
        assert!(matches!(result, IoResult::Ok));

        // Verify we get the expected vendor/device ID
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(value, expected);

        // Test write operation
        let result = PciConfigSpace::pci_cfg_write(&mut switch, 0x4, 0x12345678);
        assert!(matches!(result, IoResult::Ok));
    }

    #[test]
    fn test_switch_default() {
        let switch = GenericPcieSwitch::default();
        assert_eq!(switch.name().as_ref(), "default-switch");
        assert_eq!(switch.downstream_ports().len(), 4);
    }

    #[test]
    fn test_switch_large_downstream_port_count() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 16,
        };
        let switch = GenericPcieSwitch::new(definition);
        assert_eq!(switch.downstream_ports().len(), 16);
    }

    #[test]
    fn test_switch_downstream_port_direct_access() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 3,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Simulate the switch's internal bus being assigned as bus 1
        let secondary_bus = 1u8;
        // Set secondary bus number (offset 0x18) - bits 8-15 of the 32-bit value at 0x18
        let bus_config = (10u32 << 24) | ((secondary_bus as u32) << 16); // subordinate | secondary
        switch
            .upstream_port
            .cfg_space_mut()
            .write_u32(0x18, bus_config)
            .unwrap();

        let bus_range = switch.upstream_port.cfg_space().assigned_bus_range();
        let switch_internal_bus = *bus_range.start(); // This is the secondary bus

        // Test direct access to downstream port 0 using device_function = 0
        let mut value = 0u32;
        let result = switch.route_cfg_access(switch_internal_bus, 0, true, 0x0, &mut value);
        assert!(result.is_some());

        // Verify we got the downstream switch port's vendor/device ID
        let expected = (DOWNSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(value, expected);

        // Test direct access to downstream port 2 using device_function = 2
        let mut value2 = 0u32;
        let result2 = switch.route_cfg_access(switch_internal_bus, 2, true, 0x0, &mut value2);
        assert!(result2.is_some());
        assert_eq!(value2, expected);

        // Test access to non-existent downstream port using device_function = 5
        let mut value3 = 0u32;
        let result3 = switch.route_cfg_access(switch_internal_bus, 5, true, 0x0, &mut value3);
        assert!(result3.is_none());
    }

    #[test]
    fn test_switch_invalid_bus_range_handling() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Don't configure bus numbers, so the range should be 0..=0 (invalid)
        let bus_range = switch.upstream_port.cfg_space().assigned_bus_range();
        assert_eq!(bus_range, 0..=0);

        // Test that any access returns None when bus range is invalid
        let mut value = 0u32;
        let result = switch.route_cfg_access(0, 0, true, 0x0, &mut value);
        assert!(result.is_none());

        let result2 = switch.route_cfg_access(1, 0, true, 0x0, &mut value);
        assert!(result2.is_none());

        let result3 = switch.route_cfg_access(0, 0, false, 0x0, &mut value);
        assert!(result3.is_none());
    }

    #[test]
    fn test_switch_downstream_port_invalid_bus_range_skipping() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Configure the upstream port with a valid bus range
        let secondary_bus = 1u8;
        let subordinate_bus = 10u8;
        let primary_bus = 0u8;
        let bus_config =
            ((subordinate_bus as u32) << 16) | ((secondary_bus as u32) << 8) | (primary_bus as u32); // subordinate | secondary | primary
        switch
            .upstream_port
            .cfg_space_mut()
            .write_u32(0x18, bus_config)
            .unwrap();

        // Downstream ports still have invalid bus ranges (0..=0 by default)
        // so any access to buses beyond the secondary bus should return None
        let mut value = 0u32;

        // Access to bus 2 should return None since no downstream port has a valid bus range
        let result = switch.route_cfg_access(2, 0, true, 0x0, &mut value);
        assert!(result.is_none());

        // Access to bus 5 should also return None
        let result2 = switch.route_cfg_access(5, 0, true, 0x0, &mut value);
        assert!(result2.is_none());

        // Access to the secondary bus (switch internal) should still work for downstream port config
        let result3 = switch.route_cfg_access(secondary_bus, 0, true, 0x0, &mut value);
        assert!(result3.is_some());
    }

    #[test]
    fn test_switch_multi_function_bit() {
        // Test that switches with multiple downstream ports set the multi-function bit
        let multi_port_definition = GenericPcieSwitchDefinition {
            name: "multi-port-switch".into(),
            downstream_port_count: 3,
        };
        let multi_port_switch = GenericPcieSwitch::new(multi_port_definition);

        // Verify each downstream port has the multi-function bit set
        for (port_num, _) in multi_port_switch.downstream_ports() {
            if let Some((_, downstream_port)) = multi_port_switch.downstream_ports.get(&port_num) {
                let mut header_type_value: u32 = 0;
                downstream_port
                    .cfg_space()
                    .read_u32(0x0C, &mut header_type_value)
                    .unwrap();

                // Extract the header type field (bits 16-23, with multi-function bit at bit 23)
                let header_type_field = (header_type_value >> 16) & 0xFF;

                // Multi-function bit should be set (bit 7 of header type field = bit 23 of dword)
                assert_eq!(
                    header_type_field & 0x80,
                    0x80,
                    "Multi-function bit should be set for downstream port {} in multi-port switch",
                    port_num
                );

                // Base header type should still be 01 (bridge)
                assert_eq!(
                    header_type_field & 0x7F,
                    0x01,
                    "Header type should be 01 (bridge) for downstream port {}",
                    port_num
                );
            }
        }

        // Test that switches with single downstream port do NOT set the multi-function bit
        let single_port_definition = GenericPcieSwitchDefinition {
            name: "single-port-switch".into(),
            downstream_port_count: 1,
        };
        let single_port_switch = GenericPcieSwitch::new(single_port_definition);

        // Verify the single downstream port does NOT have the multi-function bit set
        for (port_num, _) in single_port_switch.downstream_ports() {
            if let Some((_, downstream_port)) = single_port_switch.downstream_ports.get(&port_num) {
                let mut header_type_value: u32 = 0;
                downstream_port
                    .cfg_space()
                    .read_u32(0x0C, &mut header_type_value)
                    .unwrap();

                // Extract the header type field (bits 16-23)
                let header_type_field = (header_type_value >> 16) & 0xFF;

                // Multi-function bit should NOT be set
                assert_eq!(
                    header_type_field & 0x80,
                    0x00,
                    "Multi-function bit should NOT be set for downstream port {} in single-port switch",
                    port_num
                );

                // Base header type should still be 01 (bridge)
                assert_eq!(
                    header_type_field & 0x7F,
                    0x01,
                    "Header type should be 01 (bridge) for downstream port {}",
                    port_num
                );
            }
        }
    }
}
