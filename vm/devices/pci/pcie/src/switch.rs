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
use chipset_device::io::IoResult;
use inspect::Inspect;
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
            port: PciePort::new(name, hardware_ids, DevicePortType::DownstreamSwitchPort),
        }
    }

    /// Try to connect a PCIe device, returning an existing device name if the
    /// port is already occupied.
    pub fn connect_device(
        &mut self,
        name: impl AsRef<str>,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        let port_name = self.port.name.clone();
        let device_name: Arc<str> = name.as_ref().into();
        match self
            .port
            .try_connect_under(port_name.as_ref(), device_name, dev)
        {
            Ok(()) => Ok(()),
            Err(_returned_device) => {
                // If the connection failed, it means the port is already occupied
                // We need to get the name of the existing device
                if let Some((existing_name, _)) = &self.port.link {
                    Err(existing_name.clone())
                } else {
                    // This shouldn't happen if try_connect_under works correctly
                    Err("unknown".into())
                }
            }
        }
    }

    /// Forward a configuration space read to the connected device.
    pub fn forward_cfg_read(
        &mut self,
        bus: &u8,
        device_function: &u8,
        cfg_offset: u16,
        value: &mut u32,
    ) -> IoResult {
        self.port
            .forward_cfg_read_with_routing(bus, device_function, cfg_offset, value)
    }

    /// Forward a configuration space write to the connected device.
    pub fn forward_cfg_write(
        &mut self,
        bus: &u8,
        device_function: &u8,
        cfg_offset: u16,
        value: u32,
    ) -> IoResult {
        self.port
            .forward_cfg_write_with_routing(bus, device_function, cfg_offset, value)
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
pub struct PcieSwitchDefinition {
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
#[derive(Inspect)]
pub struct Switch {
    /// The name of this switch instance.
    name: Arc<str>,
    /// The upstream switch port that connects to the parent.
    upstream_port: UpstreamSwitchPort,
    /// Map of downstream switch ports, indexed by port number.
    #[inspect(with = "|x| inspect::iter_by_key(x).map_value(|(_, v)| v)")]
    downstream_ports: HashMap<u8, (Arc<str>, DownstreamSwitchPort)>,
}

impl Switch {
    /// Constructs a new [`Switch`] emulator.
    pub fn new(definition: PcieSwitchDefinition) -> Self {
        let upstream_port = UpstreamSwitchPort::new();

        let downstream_ports = (0..definition.downstream_port_count)
            .map(|i| {
                let port_name = format!("{}-downstream-{}", definition.name, i);
                let port = DownstreamSwitchPort::new(port_name.clone());
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

    /// Get a mutable reference to the upstream switch port.
    pub fn upstream_port_mut(&mut self) -> &mut UpstreamSwitchPort {
        &mut self.upstream_port
    }

    /// Enumerate the downstream ports of the switch.
    pub fn downstream_ports(&self) -> Vec<(u8, Arc<str>)> {
        self.downstream_ports
            .iter()
            .map(|(port, (name, _))| (*port, name.clone()))
            .collect()
    }

    /// Connect a device to a specific downstream port.
    pub fn connect_downstream_device(
        &mut self,
        port_name: impl AsRef<str>,
        device_name: impl AsRef<str>,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        // Find the downstream port with the matching name
        let port_name_ref = port_name.as_ref();
        let (_, downstream_port) = self
            .downstream_ports
            .values_mut()
            .find(|(name, _)| name.as_ref() == port_name_ref)
            .ok_or_else(|| -> Arc<str> {
                format!("Downstream port '{}' not found", port_name_ref).into()
            })?;
        downstream_port.connect_device(device_name, dev)
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
        // Check if the access is for the upstream port's decoded bus range
        let upstream_bus_range = self.upstream_port.cfg_space().assigned_bus_range();

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration
        if upstream_bus_range == (0..=0) {
            return None;
        }

        if upstream_bus_range.contains(&bus) {
            // If the access goes to the secondary bus number of the upstream switch port, this means the
            // access should target one of the downstream switch ports. Look for the matching one and
            // return the config space access result from it if found.
            if bus == *upstream_bus_range.start() {
                if let Some((_, downstream_port)) = self.downstream_ports.get_mut(&device_function)
                {
                    if is_read {
                        return Some(downstream_port.port.cfg_space.read_u32(cfg_offset, value));
                    } else {
                        return Some(downstream_port.port.cfg_space.write_u32(cfg_offset, *value));
                    }
                }

                // No downstream switch port found for the access targeting the secondary bus number,
                // this means no valid device to handle the access.
                return None;
            }

            // Otherwise, since the access is within the decoded bus range of the switch, this means the
            // access should be routed downstream of one of the downstream switch ports.
            for (_, downstream_port) in self.downstream_ports.values_mut() {
                let downstream_bus_range = downstream_port.cfg_space().assigned_bus_range();

                // Skip downstream ports with invalid/uninitialized bus configuration
                if downstream_bus_range == (0..=0) {
                    continue;
                }

                if downstream_bus_range.contains(&bus) {
                    if is_read {
                        return Some(downstream_port.forward_cfg_read(
                            &bus,
                            &device_function,
                            cfg_offset,
                            value,
                        ));
                    } else {
                        return Some(downstream_port.forward_cfg_write(
                            &bus,
                            &device_function,
                            cfg_offset,
                            *value,
                        ));
                    }
                }
            }
        }

        // The access is not within the upstream switch port's decoded bus range,
        // return None to indicate no handling.
        None
    }
}

impl GenericPciBusDevice for Switch {
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

impl GenericPciRoutingComponent for Switch {
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

    fn try_connect_under(
        &mut self,
        port_name: &str,
        device_name: Arc<str>,
        device: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Box<dyn GenericPciBusDevice>> {
        // Try to connect to each downstream port - any of them might be able to handle
        // the connection either directly (if name matches) or by routing it further down
        let mut current_device = device;

        for (_, (_, downstream_port)) in self.downstream_ports.iter_mut() {
            match downstream_port.port.try_connect_under(
                port_name,
                device_name.clone(),
                current_device,
            ) {
                Ok(()) => return Ok(()),
                Err(returned_device) => {
                    current_device = returned_device;
                    // Continue to next downstream port
                }
            }
        }

        // None of our downstream ports could handle the connection
        Err(current_device)
    }
}

impl Default for Switch {
    fn default() -> Self {
        Self::new(PcieSwitchDefinition {
            name: "default-switch".into(),
            downstream_port_count: 4,
        })
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
    fn test_downstream_switch_port_device_connection() {
        use crate::test_helpers::TestPcieEndpoint;
        use chipset_device::io::IoError;

        let mut port = DownstreamSwitchPort::new("test-port");
        let test_device = TestPcieEndpoint::new(
            |offset, value| match offset {
                0x0 => {
                    *value = 0xABCD_EF01;
                    Some(IoResult::Ok)
                }
                _ => Some(IoResult::Err(IoError::InvalidRegister)),
            },
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        // Connect a device
        assert!(
            port.connect_device("test-endpoint", Box::new(test_device))
                .is_ok()
        );
        assert!(port.port.link.is_some());

        // Try to connect another device (should fail)
        let another_device = TestPcieEndpoint::new(
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );
        let result = port.connect_device("another-endpoint", Box::new(another_device));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().as_ref(), "test-endpoint");
    }

    #[test]
    fn test_switch_creation() {
        let definition = PcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 3,
        };
        let switch = Switch::new(definition);

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

        let definition = PcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
        };
        let mut switch = Switch::new(definition);

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
                .connect_downstream_device(
                    "test-switch-downstream-0",
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
        let result = switch.connect_downstream_device(
            "invalid-port-name",
            "invalid-dev",
            Box::new(invalid_device),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Downstream port 'invalid-port-name' not found")
        );
    }

    #[test]
    fn test_switch_as_routing_component() {
        let definition = PcieSwitchDefinition {
            name: "routing-switch".into(),
            downstream_port_count: 1,
        };
        let mut switch = Switch::new(definition);

        // Verify that Switch implements GenericPciRoutingComponent
        assert!(switch.as_routing_component().is_some());

        // Test basic configuration space access
        let mut value = 0u32;
        let result = switch.pci_cfg_read(0x0, &mut value);
        assert!(result.is_some());

        // Verify vendor/device ID is from the upstream port
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(value, expected);
    }

    #[test]
    fn test_switch_default() {
        let switch = Switch::default();
        assert_eq!(switch.name().as_ref(), "default-switch");
        assert_eq!(switch.downstream_ports().len(), 4);
    }

    #[test]
    fn test_switch_large_downstream_port_count() {
        let definition = PcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 16,
        };
        let switch = Switch::new(definition);
        assert_eq!(switch.downstream_ports().len(), 16);
    }

    #[test]
    fn test_switch_downstream_port_direct_access() {
        let definition = PcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 3,
        };
        let mut switch = Switch::new(definition);

        // Simulate the switch's internal bus being assigned as bus 1
        let secondary_bus = 1u8;
        // Set secondary bus number (offset 0x18) - bits 8-15 of the 32-bit value at 0x18
        let bus_config = (10u32 << 24) | ((secondary_bus as u32) << 16) | (0u32 << 8) | 0u32; // subordinate | secondary | reserved | primary
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
        let definition = PcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
        };
        let mut switch = Switch::new(definition);

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
        let definition = PcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
        };
        let mut switch = Switch::new(definition);

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
}
