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
use pci_bus::GenericPciBusDevice;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
use pci_core::spec::caps::pci_express::DevicePortType;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::sync::Arc;

/// A PCI Express upstream switch port emulator.
///
/// An upstream switch port connects a switch to its parent (e.g., root port or another switch).
/// It appears as a Type 1 PCI-to-PCI bridge with PCIe capability indicating it's an upstream switch port.
#[derive(Inspect)]
pub struct UpstreamSwitchPort {
    cfg_space: ConfigSpaceType1Emulator,

    #[inspect(skip)]
    link: Option<(Arc<str>, Box<dyn GenericPciBusDevice>)>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upstream_switch_port_creation() {
        let port = UpstreamSwitchPort::new();
        assert!(port.link.is_none());

        // Verify that we can read the vendor/device ID from config space
        let mut vendor_device_id: u32 = 0;
        port.cfg_space.read_u32(0x0, &mut vendor_device_id).unwrap();
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);
    }

    #[test]
    fn test_upstream_switch_port_device_connection() {
        use crate::test_helpers::TestPcieEndpoint;
        use chipset_device::io::IoError;

        let mut port = UpstreamSwitchPort::new();
        let test_device = TestPcieEndpoint::new(
            |offset, value| match offset {
                0x0 => {
                    *value = 0x1234_5678;
                    Some(IoResult::Ok)
                }
                _ => Some(IoResult::Err(IoError::InvalidRegister)),
            },
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        // Connect a device
        assert!(port.connect_device("test-device", test_device).is_ok());
        assert!(port.link.is_some());

        // Try to connect another device (should fail)
        let another_device = TestPcieEndpoint::new(
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );
        let result = port.connect_device("another-device", another_device);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().as_ref(), "test-device");
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
}
