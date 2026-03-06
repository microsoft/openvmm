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
use crate::port::PcieDownstreamPort;
use anyhow::{Context, bail};
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use inspect::Inspect;
use inspect::InspectMut;
use pci_bus::GenericPciBusDevice;
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

/// A PCI Express downstream switch port emulator.
///
/// A downstream switch port connects a switch to its children (e.g., endpoints or other switches).
/// It appears as a Type 1 PCI-to-PCI bridge with PCIe capability indicating it's a downstream switch port.
#[derive(Inspect)]
pub struct DownstreamSwitchPort {
    /// The common PCIe port implementation.
    #[inspect(flatten)]
    port: PcieDownstreamPort,
}

impl DownstreamSwitchPort {
    /// Constructs a new [`DownstreamSwitchPort`] emulator.
    ///
    /// # Arguments
    /// * `name` - The name for this downstream switch port
    /// * `multi_function` - Whether this port should have the multi-function flag set (default: false)
    /// * `hotplug_slot_number` - The slot number for hotplug support. `Some(slot_number)` enables hotplug, `None` disables it
    pub fn new(
        name: impl Into<Arc<str>>,
        multi_function: Option<bool>,
        hotplug_slot_number: Option<u32>,
    ) -> Self {
        let multi_function = multi_function.unwrap_or(false);
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

        let port = PcieDownstreamPort::new(
            name.into().to_string(),
            hardware_ids,
            DevicePortType::DownstreamSwitchPort,
            multi_function,
            hotplug_slot_number,
        );

        Self { port }
    }

    /// Get a reference to the configuration space emulator.
    pub fn cfg_space(&self) -> &ConfigSpaceType1Emulator {
        &self.port.cfg_space
    }

    /// Get a mutable reference to the configuration space emulator.
    pub fn cfg_space_mut(&mut self) -> &mut ConfigSpaceType1Emulator {
        &mut self.port.cfg_space
    }
}

/// A PCI Express switch definition used for creating switch instances.
pub struct GenericPcieSwitchDefinition {
    /// The name of the switch.
    pub name: Arc<str>,
    /// The number of downstream ports to create.
    /// TODO: implement physical slot number, link and slot stuff
    pub downstream_port_count: u8,
    /// Whether hotplug is enabled for this switch's downstream ports.
    pub hotplug: bool,
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
                // Use the port index as the slot number for hotpluggable ports
                let hotplug_slot_number = if definition.hotplug {
                    Some((i as u32) + 1)
                } else {
                    None
                };
                let port = DownstreamSwitchPort::new(
                    port_name.clone(),
                    Some(multi_function),
                    hotplug_slot_number,
                );
                (i, (port_name.into(), port))
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

    /// Route configuration space read to the appropriate port based on addressing.
    fn route_cfg_read(
        &mut self,
        bus: u8,
        device_function: u8,
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
            return self.handle_downstream_port_read(device_function, cfg_offset, value);
        }

        // Route to downstream ports for further forwarding
        self.route_read_to_downstream_ports(bus, device_function, cfg_offset, value)
    }

    /// Route configuration space write to the appropriate port based on addressing.
    fn route_cfg_write(
        &mut self,
        bus: u8,
        device_function: u8,
        cfg_offset: u16,
        value: u32,
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
            return self.handle_downstream_port_write(device_function, cfg_offset, value);
        }

        // Route to downstream ports for further forwarding
        self.route_write_to_downstream_ports(bus, device_function, cfg_offset, value)
    }

    /// Handle direct configuration space read to downstream switch ports.
    fn handle_downstream_port_read(
        &mut self,
        device_function: u8,
        cfg_offset: u16,
        value: &mut u32,
    ) -> Option<IoResult> {
        if let Some((_, downstream_port)) = self.downstream_ports.get_mut(&device_function) {
            Some(downstream_port.port.cfg_space.read_u32(cfg_offset, value))
        } else {
            // No downstream switch port found for this device function
            None
        }
    }

    /// Handle direct configuration space write to downstream switch ports.
    fn handle_downstream_port_write(
        &mut self,
        device_function: u8,
        cfg_offset: u16,
        value: u32,
    ) -> Option<IoResult> {
        if let Some((_, downstream_port)) = self.downstream_ports.get_mut(&device_function) {
            Some(downstream_port.port.cfg_space.write_u32(cfg_offset, value))
        } else {
            // No downstream switch port found for this device function
            None
        }
    }

    /// Route configuration space read to downstream ports for further forwarding.
    fn route_read_to_downstream_ports(
        &mut self,
        bus: u8,
        device_function: u8,
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
                return Some(downstream_port.port.forward_cfg_read_with_routing(
                    &bus,
                    &device_function,
                    cfg_offset,
                    value,
                ));
            }
        }

        // No downstream port could handle this bus number
        None
    }

    /// Route configuration space write to downstream ports for further forwarding.
    fn route_write_to_downstream_ports(
        &mut self,
        bus: u8,
        device_function: u8,
        cfg_offset: u16,
        value: u32,
    ) -> Option<IoResult> {
        for (_, downstream_port) in self.downstream_ports.values_mut() {
            let downstream_bus_range = downstream_port.cfg_space().assigned_bus_range();

            // Skip downstream ports with invalid/uninitialized bus configuration
            if downstream_bus_range == (0..=0) {
                continue;
            }

            if downstream_bus_range.contains(&bus) {
                return Some(downstream_port.port.forward_cfg_write_with_routing(
                    &bus,
                    &device_function,
                    cfg_offset,
                    value,
                ));
            }
        }

        // No downstream port could handle this bus number
        None
    }

    /// Attach the provided `GenericPciBusDevice` to the port identified.
    pub fn add_pcie_device(
        &mut self,
        port: u8,
        name: &str,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> anyhow::Result<()> {
        // Find the specific downstream port that matches the port number
        if let Some((port_name, downstream_port)) = self.downstream_ports.get_mut(&port) {
            // Found the matching port, try to connect to it using the port's name
            downstream_port
                .port
                .add_pcie_device(port_name.as_ref(), name, dev)
                .context("failed to add PCIe device to downstream port")?;
            Ok(())
        } else {
            // No downstream port found with matching port number
            bail!("port {} not found", port);
        }
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
        // Forward to the upstream port's configuration space (the switch presents as the upstream port)
        self.upstream_port.cfg_space.read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        // Forward to the upstream port's configuration space (the switch presents as the upstream port)
        self.upstream_port.cfg_space.write_u32(offset, value)
    }

    fn pci_cfg_read_forward(
        &mut self,
        bus: u8,
        device_function: u8,
        offset: u16,
        value: &mut u32,
    ) -> Option<IoResult> {
        self.route_cfg_read(bus, device_function, offset, value)
    }

    fn pci_cfg_write_forward(
        &mut self,
        bus: u8,
        device_function: u8,
        offset: u16,
        value: u32,
    ) -> Option<IoResult> {
        self.route_cfg_write(bus, device_function, offset, value)
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        // PCIe switches typically don't have a fixed BDF requirement
        None
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

        /// Saved state for the GenericPcieSwitch.
        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pcie.switch")]
        pub struct SavedState {
            /// The upstream port configuration space state.
            #[mesh(1)]
            pub upstream_port_cfg_space: <ConfigSpaceType1Emulator as SaveRestore>::SavedState,
            /// The downstream port states, ordered by port number (index = port number).
            #[mesh(2)]
            pub downstream_ports: Vec<<ConfigSpaceType1Emulator as SaveRestore>::SavedState>,
        }
    }

    impl SaveRestore for GenericPcieSwitch {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            // Save the upstream port configuration space
            let upstream_port_cfg_space = self.upstream_port.cfg_space.save()?;

            // Save all downstream ports in order by port number
            let port_count = self.downstream_ports.len();
            let mut downstream_ports = Vec::with_capacity(port_count);
            for port_number in 0..port_count as u8 {
                if let Some((_, downstream_port)) = self.downstream_ports.get_mut(&port_number) {
                    downstream_ports.push(downstream_port.port.cfg_space.save()?);
                } else {
                    return Err(SaveError::NotFound(format!(
                        "downstream port {}",
                        port_number
                    )));
                }
            }

            Ok(state::SavedState {
                upstream_port_cfg_space,
                downstream_ports,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                upstream_port_cfg_space,
                downstream_ports,
            } = state;

            // Validate that the number of downstream ports matches
            if downstream_ports.len() != self.downstream_ports.len() {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "downstream port count mismatch: saved {}, current {}",
                    downstream_ports.len(),
                    self.downstream_ports.len()
                )));
            }

            // Restore the upstream port configuration space
            self.upstream_port
                .cfg_space
                .restore(upstream_port_cfg_space)?;

            // Restore all downstream ports (index = port number)
            for (port_number, cfg_space) in downstream_ports.into_iter().enumerate() {
                let port_number = port_number as u8;
                if let Some((_, downstream_port)) = self.downstream_ports.get_mut(&port_number) {
                    downstream_port.port.cfg_space.restore(cfg_space)?;
                } else {
                    return Err(RestoreError::NotFound(format!(
                        "downstream port {}",
                        port_number
                    )));
                }
            }

            Ok(())
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
        let port = DownstreamSwitchPort::new("test-downstream-port", None, None);
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
    fn test_downstream_switch_port_multi_function_options() {
        // Test with default multi_function (false)
        let port_default = DownstreamSwitchPort::new("test-port-default", None, None);
        let mut header_type_value: u32 = 0;
        port_default
            .cfg_space()
            .read_u32(0x0C, &mut header_type_value)
            .unwrap();
        let header_type_field = (header_type_value >> 16) & 0xFF;
        assert_eq!(
            header_type_field & 0x80,
            0x00,
            "Multi-function bit should NOT be set with None parameter"
        );

        // Test with explicit multi_function false
        let port_false = DownstreamSwitchPort::new("test-port-false", Some(false), None);
        let mut header_type_value_false: u32 = 0;
        port_false
            .cfg_space()
            .read_u32(0x0C, &mut header_type_value_false)
            .unwrap();
        let header_type_field_false = (header_type_value_false >> 16) & 0xFF;
        assert_eq!(
            header_type_field_false & 0x80,
            0x00,
            "Multi-function bit should NOT be set with Some(false)"
        );

        // Test with explicit multi_function true
        let port_true = DownstreamSwitchPort::new("test-port-true", Some(true), None);
        let mut header_type_value_true: u32 = 0;
        port_true
            .cfg_space()
            .read_u32(0x0C, &mut header_type_value_true)
            .unwrap();
        let header_type_field_true = (header_type_value_true >> 16) & 0xFF;
        assert_eq!(
            header_type_field_true & 0x80,
            0x80,
            "Multi-function bit should be set with Some(true)"
        );
    }

    #[test]
    fn test_downstream_switch_port_hotplug_options() {
        // Test with hotplug disabled (None)
        let port_no_hotplug = DownstreamSwitchPort::new("test-port-no-hotplug", None, None);
        // We can't easily verify hotplug is disabled without accessing internal state,
        // but we can verify the port was created successfully
        let mut vendor_device_id: u32 = 0;
        port_no_hotplug
            .cfg_space()
            .read_u32(0x0, &mut vendor_device_id)
            .unwrap();
        let expected = (DOWNSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);

        // Test with hotplug enabled (Some(slot_number))
        let port_with_hotplug = DownstreamSwitchPort::new("test-port-hotplug", None, Some(42));
        let mut vendor_device_id_hotplug: u32 = 0;
        port_with_hotplug
            .cfg_space()
            .read_u32(0x0, &mut vendor_device_id_hotplug)
            .unwrap();
        assert_eq!(vendor_device_id_hotplug, expected);
        // The slot number and hotplug capability would be tested via PCIe capability registers
        // but that requires more complex setup
    }

    #[test]
    fn test_switch_creation() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 3,
            hotplug: false,
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
            hotplug: false,
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
        // add_pcie_device returns an anyhow::Error on failure,
        // so we just verify that the connection failed
        assert!(result.is_err());
    }

    #[test]
    fn test_switch_routing_functionality() {
        use crate::test_helpers::TestPcieEndpoint;
        use chipset_device::io::IoResult;

        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
            hotplug: false,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Verify that Switch implements routing functionality by testing add_pcie_device method
        // This tests that the switch can accept device connections (routing capability)
        let test_device =
            TestPcieEndpoint::new(|_, _| Some(IoResult::Ok), |_, _| Some(IoResult::Ok));
        let add_result = switch.add_pcie_device(0, "test-device", Box::new(test_device));
        // Should succeed for port 0 (first downstream port)
        assert!(add_result.is_ok());

        // Test basic configuration space access through the PCI interface
        let mut value = 0u32;
        let result = switch
            .upstream_port
            .cfg_space_mut()
            .read_u32(0x0, &mut value);
        assert!(matches!(result, IoResult::Ok));

        // Verify vendor/device ID is from the upstream port
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(value, expected);
    }

    #[test]
    fn test_switch_chipset_device() {
        use chipset_device::ChipsetDevice;
        use chipset_device::pci::PciConfigSpace;

        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 4,
            hotplug: false,
        };
        let mut switch = GenericPcieSwitch::new(definition);

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
        let definition = GenericPcieSwitchDefinition {
            name: "default-switch".into(),
            downstream_port_count: 4,
            hotplug: false,
        };
        let switch = GenericPcieSwitch::new(definition);
        assert_eq!(switch.name().as_ref(), "default-switch");
        assert_eq!(switch.downstream_ports().len(), 4);
    }

    #[test]
    fn test_switch_large_downstream_port_count() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 16,
            hotplug: false,
        };
        let switch = GenericPcieSwitch::new(definition);
        assert_eq!(switch.downstream_ports().len(), 16);
    }

    #[test]
    fn test_switch_downstream_port_direct_access() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 3,
            hotplug: false,
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
        let result = switch.route_cfg_read(switch_internal_bus, 0, 0x0, &mut value);
        assert!(result.is_some());

        // Verify we got the downstream switch port's vendor/device ID
        let expected = (DOWNSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(value, expected);

        // Test direct access to downstream port 2 using device_function = 2
        let mut value2 = 0u32;
        let result2 = switch.route_cfg_read(switch_internal_bus, 2, 0x0, &mut value2);
        assert!(result2.is_some());
        assert_eq!(value2, expected);

        // Test access to non-existent downstream port using device_function = 5
        let mut value3 = 0u32;
        let result3 = switch.route_cfg_read(switch_internal_bus, 5, 0x0, &mut value3);
        assert!(result3.is_none());
    }

    #[test]
    fn test_switch_invalid_bus_range_handling() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
            hotplug: false,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Don't configure bus numbers, so the range should be 0..=0 (invalid)
        let bus_range = switch.upstream_port.cfg_space().assigned_bus_range();
        assert_eq!(bus_range, 0..=0);

        // Test that any access returns None when bus range is invalid
        let mut value = 0u32;
        let result = switch.route_cfg_read(0, 0, 0x0, &mut value);
        assert!(result.is_none());

        let result2 = switch.route_cfg_read(1, 0, 0x0, &mut value);
        assert!(result2.is_none());

        let result3 = switch.route_cfg_write(0, 0, 0x0, value);
        assert!(result3.is_none());
    }

    #[test]
    fn test_switch_downstream_port_invalid_bus_range_skipping() {
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
            hotplug: false,
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
        let result = switch.route_cfg_read(2, 0, 0x0, &mut value);
        assert!(result.is_none());

        // Access to bus 5 should also return None
        let result2 = switch.route_cfg_read(5, 0, 0x0, &mut value);
        assert!(result2.is_none());

        // Access to the secondary bus (switch internal) should still work for downstream port config
        let result3 = switch.route_cfg_read(secondary_bus, 0, 0x0, &mut value);
        assert!(result3.is_some());
    }

    #[test]
    fn test_switch_multi_function_bit() {
        // Test that switches with multiple downstream ports set the multi-function bit
        let multi_port_definition = GenericPcieSwitchDefinition {
            name: "multi-port-switch".into(),
            downstream_port_count: 3,
            hotplug: false,
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
            hotplug: false,
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

    #[test]
    fn test_hotplug_support() {
        // Test hotplug disabled
        let definition_no_hotplug = GenericPcieSwitchDefinition {
            name: "test-switch-no-hotplug".into(),
            downstream_port_count: 1,
            hotplug: false,
        };
        let switch_no_hotplug = GenericPcieSwitch::new(definition_no_hotplug);
        assert_eq!(switch_no_hotplug.name().as_ref(), "test-switch-no-hotplug");

        // Test hotplug enabled
        let definition_with_hotplug = GenericPcieSwitchDefinition {
            name: "test-switch-with-hotplug".into(),
            downstream_port_count: 1,
            hotplug: true,
        };
        let switch_with_hotplug = GenericPcieSwitch::new(definition_with_hotplug);
        assert_eq!(
            switch_with_hotplug.name().as_ref(),
            "test-switch-with-hotplug"
        );
    }

    #[test]
    fn test_save_restore_basic() {
        use vmcore::save_restore::SaveRestore;

        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
            hotplug: false,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Save the initial state
        let saved_state = switch.save().expect("save should succeed");

        // Verify the saved state has the correct number of downstream ports
        assert_eq!(saved_state.downstream_ports.len(), 2);

        // Restore the state to a new switch
        let definition2 = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
            hotplug: false,
        };
        let mut switch2 = GenericPcieSwitch::new(definition2);
        switch2
            .restore(saved_state)
            .expect("restore should succeed");
    }

    #[test]
    fn test_save_restore_with_bus_configuration() {
        use vmcore::save_restore::SaveRestore;

        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 3,
            hotplug: false,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Configure bus numbers on the upstream port
        let secondary_bus = 5u8;
        let subordinate_bus = 15u8;
        let primary_bus = 0u8;
        let bus_config =
            ((subordinate_bus as u32) << 16) | ((secondary_bus as u32) << 8) | (primary_bus as u32);
        switch
            .upstream_port
            .cfg_space_mut()
            .write_u32(0x18, bus_config)
            .unwrap();

        // Verify the bus range is set
        let bus_range = switch.upstream_port.cfg_space().assigned_bus_range();
        assert_eq!(*bus_range.start(), secondary_bus);
        assert_eq!(*bus_range.end(), subordinate_bus);

        // Save the state
        let saved_state = switch.save().expect("save should succeed");

        // Create a new switch and restore the state
        let definition2 = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 3,
            hotplug: false,
        };
        let mut switch2 = GenericPcieSwitch::new(definition2);

        // Verify the new switch has default bus range before restore
        let default_bus_range = switch2.upstream_port.cfg_space().assigned_bus_range();
        assert_eq!(default_bus_range, 0..=0);

        // Restore the state
        switch2
            .restore(saved_state)
            .expect("restore should succeed");

        // Verify the bus range is restored
        let restored_bus_range = switch2.upstream_port.cfg_space().assigned_bus_range();
        assert_eq!(*restored_bus_range.start(), secondary_bus);
        assert_eq!(*restored_bus_range.end(), subordinate_bus);
    }

    #[test]
    fn test_save_restore_downstream_port_state() {
        use vmcore::save_restore::SaveRestore;

        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
            hotplug: false,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Configure bus numbers on one of the downstream ports
        // First, we need to get access to the downstream port and configure it
        if let Some((_, downstream_port)) = switch.downstream_ports.get_mut(&0) {
            let secondary_bus = 10u8;
            let subordinate_bus = 20u8;
            let primary_bus = 5u8;
            let bus_config = ((subordinate_bus as u32) << 16)
                | ((secondary_bus as u32) << 8)
                | (primary_bus as u32);
            downstream_port
                .port
                .cfg_space
                .write_u32(0x18, bus_config)
                .unwrap();
        }

        // Verify the downstream port bus range is set
        if let Some((_, downstream_port)) = switch.downstream_ports.get(&0) {
            let bus_range = downstream_port.cfg_space().assigned_bus_range();
            assert_eq!(*bus_range.start(), 10);
            assert_eq!(*bus_range.end(), 20);
        }

        // Save the state
        let saved_state = switch.save().expect("save should succeed");

        // Create a new switch and restore the state
        let definition2 = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
            hotplug: false,
        };
        let mut switch2 = GenericPcieSwitch::new(definition2);

        // Verify the new switch has default bus range on downstream port before restore
        if let Some((_, downstream_port)) = switch2.downstream_ports.get(&0) {
            let default_bus_range = downstream_port.cfg_space().assigned_bus_range();
            assert_eq!(default_bus_range, 0..=0);
        }

        // Restore the state
        switch2
            .restore(saved_state)
            .expect("restore should succeed");

        // Verify the downstream port bus range is restored
        if let Some((_, downstream_port)) = switch2.downstream_ports.get(&0) {
            let restored_bus_range = downstream_port.cfg_space().assigned_bus_range();
            assert_eq!(*restored_bus_range.start(), 10);
            assert_eq!(*restored_bus_range.end(), 20);
        }
    }

    #[test]
    fn test_save_restore_port_mismatch_error() {
        use vmcore::save_restore::SaveRestore;

        // Create a switch with 3 downstream ports
        let definition = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 3,
            hotplug: false,
        };
        let mut switch = GenericPcieSwitch::new(definition);

        // Save the state
        let saved_state = switch.save().expect("save should succeed");
        assert_eq!(saved_state.downstream_ports.len(), 3);

        // Create a new switch with only 2 downstream ports
        let definition2 = GenericPcieSwitchDefinition {
            name: "test-switch".into(),
            downstream_port_count: 2,
            hotplug: false,
        };
        let mut switch2 = GenericPcieSwitch::new(definition2);

        // Restore should fail because port 2 doesn't exist in the new switch
        let result = switch2.restore(saved_state);
        assert!(result.is_err());
    }
}
