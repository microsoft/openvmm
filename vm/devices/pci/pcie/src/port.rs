// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common PCIe port implementation shared between different port types.

use chipset_device::io::IoResult;
use inspect::Inspect;
use pci_bus::GenericPciBusDevice;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
use pci_core::spec::caps::pci_express::DevicePortType;
use pci_core::spec::hwid::HardwareIds;
use std::sync::Arc;

/// A common PCIe port implementation that handles device connections and configuration forwarding.
///
/// This struct contains the common functionality shared between RootPort and DownstreamSwitchPort,
/// including device connection management and configuration space forwarding logic.
#[derive(Inspect)]
pub struct PciePort {
    /// The configuration space emulator for this port.
    pub cfg_space: ConfigSpaceType1Emulator,

    /// The connected device, if any.
    #[inspect(skip)]
    pub link: Option<(Arc<str>, Box<dyn GenericPciBusDevice>)>,
}

impl PciePort {
    /// Creates a new PCIe port with the specified hardware configuration.
    pub fn new(hardware_ids: HardwareIds, port_type: DevicePortType) -> Self {
        let cfg_space = ConfigSpaceType1Emulator::new(
            hardware_ids,
            vec![Box::new(PciExpressCapability::new(port_type, None))],
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

    /// Forward a configuration space read to the connected device with root port logic.
    ///
    /// This version supports routing components for multi-level hierarchies.
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
                    if let Some(result) = device.pci_cfg_read(cfg_offset, value) {
                        match result {
                            IoResult::Ok => (),
                            res => return res,
                        }
                    }
                }
            } else {
                tracelimit::warn_ratelimited!("invalid access: multi-function device access not supported for now");
                return IoResult::Ok;
            }

        } else if bus_range.contains(bus) {
            if let Some((_, device)) = &mut self.link {
                // Forward access to the routing component.
                if let Some(routing_device) = device.as_routing_component() {
                    if let Some(result) = routing_device.pci_cfg_read_forward(
                        *bus,
                        *device_function,
                        cfg_offset,
                        value,
                    ) {
                        match result {
                            IoResult::Ok => (),
                            res => return res,
                        }
                    }
                } else {
                    tracelimit::warn_ratelimited!("invalid access: bus number to access not within port's bus number range");
                }
            }
        }

        IoResult::Ok
    }

    /// Forward a configuration space write to the connected device with root port logic.
    ///
    /// This version supports routing components for multi-level hierarchies.
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
            if *device_function == 0 {
                // Perform type-0 access to the child device's config space.
                if let Some((_, device)) = &mut self.link {
                    if let Some(result) = device.pci_cfg_write(cfg_offset, value) {
                        match result {
                            IoResult::Ok => (),
                            res => return res,
                        }
                    }
                }
            } else {
                tracelimit::warn_ratelimited!("invalid access: multi-function device access not supported for now");
                return IoResult::Ok;
            }

        } else if bus_range.contains(bus) {
            if let Some((_, device)) = &mut self.link {
                // Forward access to the routing component.
                if let Some(routing_device) = device.as_routing_component() {
                    if let Some(result) = routing_device.pci_cfg_write_forward(
                        *bus,
                        *device_function,
                        cfg_offset,
                        value,
                    ) {
                        match result {
                            IoResult::Ok => (),
                            res => return res,
                        }
                    }
                } else {
                    tracelimit::warn_ratelimited!("invalid access: bus number to access not within port's bus number range");
                }
            }
        }

        IoResult::Ok
    }
}
