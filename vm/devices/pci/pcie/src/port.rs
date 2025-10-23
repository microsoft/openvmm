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

/// A common PCIe downstream facing port implementation that handles device connections and configuration forwarding.
///
/// This struct contains the common functionality shared between RootPort and DownstreamSwitchPort,
/// including device connection management and configuration space forwarding logic.
#[derive(Inspect)]
pub struct PcieDownstreamPort {
    /// The name of this port.
    pub name: Arc<str>,

    /// The configuration space emulator for this port.
    pub cfg_space: ConfigSpaceType1Emulator,

    /// The connected device, if any.
    #[inspect(skip)]
    pub link: Option<(Arc<str>, Box<dyn GenericPciBusDevice>)>,
}

impl PcieDownstreamPort {
    /// Creates a new PCIe port with the specified hardware configuration.
    pub fn new(
        name: impl Into<Arc<str>>,
        hardware_ids: HardwareIds,
        port_type: DevicePortType,
    ) -> Self {
        Self::new_with_multi_function(name, hardware_ids, port_type, false)
    }

    /// Creates a new PCIe port with the specified hardware configuration and multi-function flag.
    pub fn new_with_multi_function(
        name: impl Into<Arc<str>>,
        hardware_ids: HardwareIds,
        port_type: DevicePortType,
        multi_function: bool,
    ) -> Self {
        let cfg_space = ConfigSpaceType1Emulator::new(
            hardware_ids,
            vec![Box::new(PciExpressCapability::new(port_type, None))],
        )
        .with_multi_function_bit(multi_function);
        Self {
            name: name.into(),
            cfg_space,
            link: None,
        }
    }

    /// Forward a configuration space access to the connected device with root port logic.
    ///
    /// This version supports routing components for multi-level hierarchies.
    pub fn forward_cfg_access_with_routing(
        &mut self,
        bus: &u8,
        device_function: &u8,
        is_read: bool,
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
                    let result = if is_read {
                        device.pci_cfg_read(cfg_offset, value)
                    } else {
                        device.pci_cfg_write(cfg_offset, *value)
                    };

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
                let result = if is_read {
                    device.pci_cfg_read_forward(*bus, *device_function, cfg_offset, value)
                } else {
                    device.pci_cfg_write_forward(*bus, *device_function, cfg_offset, *value)
                };

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
    ) -> Result<(), Arc<str>> {
        // Only connect if the name exactly matches this port's name
        if port_name == self.name.as_ref() {
            // Check if there's already a device connected
            if self.link.is_some() {
                return Err("Port is already occupied".into());
            }

            // Connect the device to this port
            self.link = Some((device_name.into(), device));
            return Ok(());
        }

        // If the name doesn't match, fail immediately (no forwarding)
        Err("Port name does not match".into())
    }
}
