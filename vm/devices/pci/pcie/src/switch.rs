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

    /// Forward a configuration space read - simplified since device connection is handled by Switch.
    pub fn forward_cfg_read(
        &mut self,
        bus: &u8,
        _device_function: &u8,
        _cfg_offset: u16,
        _value: &mut u32,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();
        if bus_range.contains(bus) {
            tracelimit::warn_ratelimited!("multi-level hierarchies not implemented yet");
        }

        IoResult::Ok
    }

    /// Forward a configuration space write - simplified since device connection is handled by Switch.
    pub fn forward_cfg_write(
        &mut self,
        bus: &u8,
        _device_function: &u8,
        _cfg_offset: u16,
        _value: u32,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();
        if bus_range.contains(bus) {
            tracelimit::warn_ratelimited!("multi-level hierarchies not implemented yet");
        }

        IoResult::Ok
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
    cfg_space: ConfigSpaceType1Emulator,

    #[inspect(skip)]
    link: Option<(Arc<str>, Box<dyn GenericPciBusDevice>)>,
}

impl DownstreamSwitchPort {
    /// Constructs a new [`DownstreamSwitchPort`] emulator.
    pub fn new() -> Self {
        let cfg_space = ConfigSpaceType1Emulator::new(
            HardwareIds {
                vendor_id: VENDOR_ID,
                device_id: DOWNSTREAM_SWITCH_PORT_DEVICE_ID,
                revision_id: 0,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::BRIDGE_PCI_TO_PCI,
                base_class: ClassCode::BRIDGE,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(PciExpressCapability::new(
                DevicePortType::DownstreamSwitchPort,
                None,
            ))],
        );
        Self {
            cfg_space,
            link: None,
        }
    }

    /// Try to connect a PCIe device, returning an existing device name if the
    /// port is already occupied.
    pub fn connect_device<D: GenericPciBusDevice>(
        &mut self,
        name: impl AsRef<str>,
        dev: D,
    ) -> Result<(), Arc<str>> {
        if let Some((name, _)) = &self.link {
            return Err(name.clone());
        }

        self.link = Some((name.as_ref().into(), Box::new(dev)));
        Ok(())
    }

    /// Forward a configuration space read to the connected device.
    pub fn forward_cfg_read(
        &mut self,
        bus: &u8,
        device_function: &u8,
        cfg_offset: u16,
        value: &mut u32,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();
        if *bus == *bus_range.start() && *device_function == 0 {
            if let Some((_, device)) = &mut self.link {
                if let Some(result) = device.pci_cfg_read(cfg_offset, value) {
                    return result;
                }
            }
        } else if bus_range.contains(bus) {
            tracelimit::warn_ratelimited!("multi-level hierarchies not implemented yet");
        }

        IoResult::Ok
    }

    /// Forward a configuration space write to the connected device.
    pub fn forward_cfg_write(
        &mut self,
        bus: &u8,
        device_function: &u8,
        cfg_offset: u16,
        value: u32,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();
        if *bus == *bus_range.start() && *device_function == 0 {
            if let Some((_, device)) = &mut self.link {
                if let Some(result) = device.pci_cfg_write(cfg_offset, value) {
                    return result;
                }
            }
        } else if bus_range.contains(bus) {
            tracelimit::warn_ratelimited!("multi-level hierarchies not implemented yet");
        }

        IoResult::Ok
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

impl Default for DownstreamSwitchPort {
    fn default() -> Self {
        Self::new()
    }
}

/// A PCI Express switch definition used for creating switch instances.
pub struct PcieSwitchDefinition {
    /// The name of the switch.
    pub name: Arc<str>,
    /// The number of downstream ports to create.
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
                let port = DownstreamSwitchPort::new();
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
    pub fn connect_downstream_device<D: GenericPciBusDevice>(
        &mut self,
        port: u8,
        name: impl AsRef<str>,
        dev: D,
    ) -> Result<(), Arc<str>> {
        let (_, downstream_port) = self
            .downstream_ports
            .get_mut(&port)
            .ok_or_else(|| -> Arc<str> { format!("Invalid downstream port {}", port).into() })?;
        downstream_port.connect_device(name, dev)
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
        // Check if the access is for the upstream port's bus range
        let upstream_bus_range = self.upstream_port.cfg_space().assigned_bus_range();
        if upstream_bus_range.contains(&bus) {
            if is_read {
                return Some(self.upstream_port.forward_cfg_read(
                    &bus,
                    &device_function,
                    cfg_offset,
                    value,
                ));
            } else {
                return Some(self.upstream_port.forward_cfg_write(
                    &bus,
                    &device_function,
                    cfg_offset,
                    *value,
                ));
            }
        }

        // Check downstream ports
        for (_, downstream_port) in self.downstream_ports.values_mut() {
            let downstream_bus_range = downstream_port.cfg_space().assigned_bus_range();
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

        // No matching port found
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
        let port = DownstreamSwitchPort::new();
        assert!(port.link.is_none());

        // Verify that we can read the vendor/device ID from config space
        let mut vendor_device_id: u32 = 0;
        port.cfg_space.read_u32(0x0, &mut vendor_device_id).unwrap();
        let expected = (DOWNSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);
    }

    #[test]
    fn test_downstream_switch_port_device_connection() {
        use crate::test_helpers::TestPcieEndpoint;
        use chipset_device::io::IoError;

        let mut port = DownstreamSwitchPort::new();
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
        assert!(port.connect_device("test-endpoint", test_device).is_ok());
        assert!(port.link.is_some());

        // Try to connect another device (should fail)
        let another_device = TestPcieEndpoint::new(
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );
        let result = port.connect_device("another-endpoint", another_device);
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
                .connect_downstream_device(0, "downstream-dev", downstream_device)
                .is_ok()
        );

        // Try to connect to invalid port
        let invalid_device = TestPcieEndpoint::new(
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );
        let result = switch.connect_downstream_device(99, "invalid-dev", invalid_device);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid downstream port 99"));
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
}
