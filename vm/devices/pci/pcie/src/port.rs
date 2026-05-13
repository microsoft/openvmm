// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common PCIe port implementation shared between different port types.

use anyhow::bail;
use chipset_device::io::IoResult;
use inspect::Inspect;
use pci_bus::GenericPciBusDevice;
use pci_core::bus_range::AssignedBusRange;
use pci_core::capabilities::msi_cap::MsiCapability;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
use pci_core::msi::MsiTarget;
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
    /// * `msi_target` - MSI target for interrupt delivery
    pub fn new(
        name: impl Into<String>,
        hardware_ids: HardwareIds,
        port_type: DevicePortType,
        multi_function: bool,
        hotplug_slot_number: Option<u32>,
        msi_target: &MsiTarget,
    ) -> Self {
        let port_name = name.into();

        let (hotplug, slot_number) = match hotplug_slot_number {
            Some(slot) => (true, Some(slot)),
            None => (false, None),
        };

        let msi_capability = MsiCapability::new(0, true, false, msi_target);

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

    /// Returns a clone of the config space emulator's shared bus range.
    ///
    /// The returned handle shares the same underlying atomic as the
    /// emulator — writes, resets, and restores are reflected automatically.
    pub fn bus_range(&self) -> AssignedBusRange {
        self.cfg_space.bus_range()
    }

    /// Notify the guest of a hotplug event via MSI.
    ///
    /// Fires MSI if the guest has enabled hot_plug_interrupt_enable in
    /// Slot Control. The caller must have already set the appropriate
    /// status bits (via set_hotplug_state) before calling this.
    fn fire_hotplug_msi(&self) {
        let hotplug_enabled = self
            .cfg_space
            .capabilities()
            .iter()
            .find_map(|cap| cap.as_pci_express())
            .is_some_and(|pcie| pcie.hot_plug_interrupt_enabled());

        if hotplug_enabled {
            if let Some(interrupt) = self
                .cfg_space
                .capabilities()
                .iter()
                .find_map(|cap| cap.as_msi_cap())
                .and_then(|msi| msi.interrupt())
            {
                interrupt.deliver();
            }
        }
    }

    /// Forward a configuration space read to the connected device.
    /// Supports routing components for multi-level hierarchies.
    pub fn forward_cfg_read_with_routing(
        &mut self,
        bus: &u8,
        function: &u8,
        cfg_offset: u16,
        value: &mut u32,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration
        if bus_range == (0..=0) {
            tracelimit::warn_ratelimited!("invalid access: port bus number range not configured");
            return IoResult::Ok;
        }

        if bus_range.contains(bus) {
            if let Some((_, device)) = &mut self.link {
                let secondary_bus = *bus_range.start();
                let result = device.pci_cfg_read_with_routing(
                    secondary_bus,
                    *bus,
                    *function,
                    cfg_offset,
                    value,
                );

                if let Some(result) = result {
                    match result {
                        IoResult::Ok => (),
                        res => return res,
                    }
                }
            } else if *bus != *bus_range.start() {
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
        function: &u8,
        cfg_offset: u16,
        value: u32,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration
        if bus_range == (0..=0) {
            tracelimit::warn_ratelimited!("invalid access: port bus number range not configured");
            return IoResult::Ok;
        }

        if bus_range.contains(bus) {
            if let Some((_, device)) = &mut self.link {
                let secondary_bus = *bus_range.start();
                let result = device.pci_cfg_write_with_routing(
                    secondary_bus,
                    *bus,
                    *function,
                    cfg_offset,
                    value,
                );

                if let Some(result) = result {
                    match result {
                        IoResult::Ok => (),
                        res => return res,
                    }
                }
            } else if *bus != *bus_range.start() {
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

    /// Hot-add a device to this port at runtime.
    ///
    /// Unlike `add_pcie_device`, this method verifies the port is hotplug-capable
    /// and fires MSI to notify the guest's pciehp driver.
    pub fn hotplug_add_device(
        &mut self,
        device_name: &str,
        device: Box<dyn GenericPciBusDevice>,
    ) -> anyhow::Result<()> {
        let is_hotplug_capable = self
            .cfg_space
            .capabilities()
            .iter()
            .find_map(|cap| cap.as_pci_express())
            .is_some_and(|pcie| pcie.slot_capabilities().hot_plug_capable());

        if !is_hotplug_capable {
            bail!("port '{}' is not hotplug capable", self.name);
        }
        if self.link.is_some() {
            bail!("port '{}' is already occupied", self.name);
        }

        self.link = Some((device_name.into(), device));

        // Atomically set presence + link active + changed bits, then fire MSI
        for cap in self.cfg_space.capabilities().iter() {
            if let Some(pcie) = cap.as_pci_express() {
                pcie.set_hotplug_state(true);
            }
        }
        self.fire_hotplug_msi();
        Ok(())
    }

    /// Hot-remove the device from this port at runtime.
    pub fn hotplug_remove_device(&mut self) -> anyhow::Result<()> {
        let is_hotplug_capable = self
            .cfg_space
            .capabilities()
            .iter()
            .find_map(|cap| cap.as_pci_express())
            .is_some_and(|pcie| pcie.slot_capabilities().hot_plug_capable());

        if !is_hotplug_capable {
            bail!("port '{}' is not hotplug capable", self.name);
        }
        if self.link.is_none() {
            bail!("port '{}' is empty", self.name);
        }

        self.link = None;

        // Atomically clear presence + link active + set changed bits, then fire MSI
        for cap in self.cfg_space.capabilities().iter() {
            if let Some(pcie) = cap.as_pci_express() {
                pcie.set_hotplug_state(false);
            }
        }
        self.fire_hotplug_msi();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::io::IoResult;
    use parking_lot::Mutex;
    use pci_bus::GenericPciBusDevice;
    use pci_core::spec::hwid::HardwareIds;
    use std::sync::Arc;

    // Mock device for testing
    struct MockDevice;

    impl GenericPciBusDevice for MockDevice {
        fn pci_cfg_read(&mut self, _offset: u16, _value: &mut u32) -> Option<IoResult> {
            None
        }

        fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> Option<IoResult> {
            None
        }
    }

    #[derive(Default, Debug, Clone, PartialEq, Eq)]
    struct RoutingStats {
        direct_reads: usize,
        forward_reads: Vec<(u8, u8, u16)>,
        direct_writes: usize,
        forward_writes: Vec<(u8, u8, u16, u32)>,
    }

    struct MultiFunctionMockDevice {
        stats: Arc<Mutex<RoutingStats>>,
    }

    impl GenericPciBusDevice for MultiFunctionMockDevice {
        fn pci_cfg_read(&mut self, _offset: u16, _value: &mut u32) -> Option<IoResult> {
            self.stats.lock().direct_reads += 1;
            Some(IoResult::Ok)
        }

        fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> Option<IoResult> {
            self.stats.lock().direct_writes += 1;
            Some(IoResult::Ok)
        }

        fn pci_cfg_read_with_routing(
            &mut self,
            _secondary_bus: u8,
            target_bus: u8,
            function: u8,
            offset: u16,
            value: &mut u32,
        ) -> Option<IoResult> {
            self.stats
                .lock()
                .forward_reads
                .push((target_bus, function, offset));
            *value = 0x1234_5678;
            Some(IoResult::Ok)
        }

        fn pci_cfg_write_with_routing(
            &mut self,
            _secondary_bus: u8,
            target_bus: u8,
            function: u8,
            offset: u16,
            value: u32,
        ) -> Option<IoResult> {
            self.stats
                .lock()
                .forward_writes
                .push((target_bus, function, offset, value));
            Some(IoResult::Ok)
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

        let msi_conn = pci_core::msi::MsiConnection::new();
        let mut port = PcieDownstreamPort::new(
            "test-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            Some(1), // Enable hotplug with slot number 1
            msi_conn.target(),
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

        let msi_conn = pci_core::msi::MsiConnection::new();
        let mut port = PcieDownstreamPort::new(
            "test-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None, // No hotplug
            msi_conn.target(),
        );

        // Add a device to the port (should not panic even without hotplug support)
        let mock_device = Box::new(MockDevice);
        let result = port.add_pcie_device("test-port", "mock-device", mock_device);
        assert!(
            result.is_ok(),
            "Adding device should succeed even without hotplug support"
        );
    }

    #[test]
    fn test_direct_child_bus_reads_use_forward_for_multifunction_devices() {
        use pci_core::spec::hwid::{ClassCode, ProgrammingInterface, Subclass};

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

        let msi_conn = pci_core::msi::MsiConnection::new();
        let mut port = PcieDownstreamPort::new(
            "test-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            msi_conn.target(),
        );

        port.cfg_space
            .write_u32(0x18, (1u32 << 16) | (1u32 << 8))
            .unwrap();

        let stats = Arc::new(Mutex::new(RoutingStats::default()));
        port.link = Some((
            "mf-device".into(),
            Box::new(MultiFunctionMockDevice {
                stats: Arc::clone(&stats),
            }),
        ));

        let mut value = 0;
        // All accesses on the secondary bus go through
        // pci_cfg_read_with_routing — the linked device is responsible
        // for dispatching function 0 to its own config space.
        assert!(matches!(
            port.forward_cfg_read_with_routing(&1, &0, 0x10, &mut value),
            IoResult::Ok
        ));
        assert!(matches!(
            port.forward_cfg_read_with_routing(&1, &3, 0x14, &mut value),
            IoResult::Ok
        ));

        let stats = stats.lock().clone();
        assert_eq!(stats.direct_reads, 0);
        assert_eq!(stats.forward_reads, vec![(1, 0, 0x10), (1, 3, 0x14)]);
    }

    #[test]
    fn test_direct_child_bus_writes_use_forward_for_multifunction_devices() {
        use pci_core::spec::hwid::{ClassCode, ProgrammingInterface, Subclass};

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

        let msi_conn = pci_core::msi::MsiConnection::new();
        let mut port = PcieDownstreamPort::new(
            "test-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            msi_conn.target(),
        );

        port.cfg_space
            .write_u32(0x18, (1u32 << 16) | (1u32 << 8))
            .unwrap();

        let stats = Arc::new(Mutex::new(RoutingStats::default()));
        port.link = Some((
            "mf-device".into(),
            Box::new(MultiFunctionMockDevice {
                stats: Arc::clone(&stats),
            }),
        ));

        // All accesses on the secondary bus go through
        // pci_cfg_write_with_routing — the linked device is responsible
        // for dispatching function 0 to its own config space.
        assert!(matches!(
            port.forward_cfg_write_with_routing(&1, &0, 0x10, 0xAAAA_0000),
            IoResult::Ok
        ));
        assert!(matches!(
            port.forward_cfg_write_with_routing(&1, &2, 0x14, 0xBBBB_0000),
            IoResult::Ok
        ));

        let stats = stats.lock().clone();
        assert_eq!(stats.direct_writes, 0);
        assert_eq!(
            stats.forward_writes,
            vec![(1, 0, 0x10, 0xAAAA_0000), (1, 2, 0x14, 0xBBBB_0000)]
        );
    }
}
