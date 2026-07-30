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
use crate::PortDevfnError;
use crate::UPSTREAM_SWITCH_PORT_DEVICE_ID;
use crate::VENDOR_ID;
use crate::port::PcieDownstreamPort;
use crate::port::PciePortSettings;
use anyhow::Context;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigAccessType;
use chipset_device::pci::PciConfigAddress;
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use inspect::Inspect;
use inspect::InspectMut;
use pci_bus::GenericPciBusDevice;
use pci_core::bus_cfg::PciBusCfgAccessCallbacks;
use pci_core::bus_cfg::PciBusCfgAccessHandler;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
use pci_core::msi::MsiTarget;
use pci_core::spec::caps::pci_express::DevicePortType;
use pci_core::spec::caps::pci_express::MaxEndEndTlpPrefixes;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
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
    pub fn new(end_end_tlp_prefixing_supported: bool) -> Self {
        let mut pcie_cap = PciExpressCapability::new(DevicePortType::UpstreamSwitchPort, None);

        if end_end_tlp_prefixing_supported {
            pcie_cap = pcie_cap.with_tlp_prefixing_supported(MaxEndEndTlpPrefixes::Four);
        }

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
            vec![Box::new(pcie_cap)],
            vec![],
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
    /// * `msi_target` - MSI target for interrupt delivery
    /// * `settings` - Express-level port settings (ACS, etc.)
    pub fn new(
        name: impl Into<Arc<str>>,
        multi_function: Option<bool>,
        hotplug_slot_number: Option<u32>,
        msi_target: &MsiTarget,
        settings: PciePortSettings,
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
            msi_target,
            settings,
            None,
            None,
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
    /// The downstream ports to create.
    ///
    /// Each port is assigned a devfn the same way root-complex root ports are:
    /// an explicit [`devfn`](crate::GenericPciePortDefinition::devfn) is honored,
    /// and the rest are packed sequentially from device 0. CXL is not supported
    /// on switch downstream ports.
    /// TODO: implement physical slot number, link and slot stuff
    pub downstream_ports: Vec<crate::GenericPciePortDefinition>,
    /// MSI target from the parent connection. The switch re-derives
    /// per-port targets using the upstream port's bus range.
    pub msi_target: MsiTarget,
}

/// Error returned when a switch configuration is invalid.
#[derive(Debug, thiserror::Error)]
pub enum InvalidSwitchError {
    /// CXL was requested on a switch downstream port, which is not supported.
    #[error("downstream port '{name}': CXL is not supported on switch ports")]
    CxlUnsupported {
        /// Name of the offending downstream port.
        name: Arc<str>,
    },
    /// A downstream port's devfn could not be assigned.
    #[error(transparent)]
    Devfn(#[from] PortDevfnError),
    /// All ports within a switch have to expose the same value for TLP prefixing support.
    #[error("End-to-End TLP prefixing support is inconsistent within the switch")]
    InconsistentEndToEndTlpPrefixing,
    /// If the switch supports End-to-End TLP Prefixing, all downstream ports must support up to 4 prefixes.
    #[error(
        "downstream port '{name}' has invalid max end-to-end TLP prefixing support ({supported_prefixes} != 0)"
    )]
    InvalidMaxEndToEndTlpPrefixing {
        /// Name of the offending downstream port.
        name: Arc<str>,
        /// The MaxEndToEndTlpPrefixes value, encoded as defined by the PCIe specification.
        supported_prefixes: u32,
    },
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
    /// Downstream switch ports, sorted by devfn.
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|(k, _, v)| (k, v)))")]
    downstream_ports: Vec<(u8, Arc<str>, DownstreamSwitchPort)>,
    /// The configuration space access handler for the switch.
    bus_cfg_handler: PciBusCfgAccessHandler,
}

impl GenericPcieSwitch {
    /// Constructs a new [`GenericPcieSwitch`] emulator.
    pub fn new(definition: GenericPcieSwitchDefinition) -> Result<Self, InvalidSwitchError> {
        let any_port_supports_tlp_prefixing = definition
            .downstream_ports
            .iter()
            .any(|port_def| port_def.settings.tlp_prefixing_supported.is_some());

        for port_def in &definition.downstream_ports {
            // CXL is not supported on switch downstream ports (there is no CHBCR /
            // component-register infrastructure behind a switch).
            if port_def.settings.cxl_flex_bus_port_capability.is_some() {
                return Err(InvalidSwitchError::CxlUnsupported {
                    name: port_def.name.clone(),
                });
            }

            // If any port supports End-to-End TLP prefixing, all ports must support it.
            // See PCI Express Base Specification, Revision 7.0 Section 7.5.3.15
            if any_port_supports_tlp_prefixing
                != port_def.settings.tlp_prefixing_supported.is_some()
            {
                return Err(InvalidSwitchError::InconsistentEndToEndTlpPrefixing);
            }

            // If the switch supports End-to-End TLP prefixing, all downstream ports must support up to 4 prefixes.
            // See PCI Express Base Specification, Revision 7.0 Section 7.5.3.15
            if let Some(max_prefixes) = port_def.settings.tlp_prefixing_supported {
                if max_prefixes.into_bits() != MaxEndEndTlpPrefixes::Four.into_bits() {
                    return Err(InvalidSwitchError::InvalidMaxEndToEndTlpPrefixing {
                        name: port_def.name.clone(),
                        supported_prefixes: max_prefixes.into_bits(),
                    });
                }
            }
        }

        let upstream_port = UpstreamSwitchPort::new(any_port_supports_tlp_prefixing);

        // Assign each downstream port a devfn, shared with the root-complex
        // root-port assignment. Honors explicit devfns and packs the rest from
        // device 0.
        let placements = crate::assign_port_devfns(&definition.downstream_ports, 0)?;

        // Derive per-port MSI targets from the parent's target, using
        // the upstream port's bus range. When the guest programs the
        // upstream port's secondary bus, all downstream port MSI RIDs
        // automatically update.
        let switch_msi_target = definition
            .msi_target
            .with_bus_range(upstream_port.cfg_space().bus_range(), 0);

        let mut downstream_ports: Vec<(u8, Arc<str>, DownstreamSwitchPort)> = definition
            .downstream_ports
            .into_iter()
            .zip(placements)
            .enumerate()
            .map(|(i, (port_def, placement))| {
                let port_name = port_def.name;
                // Use the port index as the slot number for hotpluggable ports
                let hotplug_slot_number = if port_def.hotplug {
                    Some((i as u32) + 1)
                } else {
                    None
                };
                let port_msi_target = switch_msi_target.with_devfn(placement.devfn);
                let port = DownstreamSwitchPort::new(
                    port_name.clone(),
                    Some(placement.multi_function),
                    hotplug_slot_number,
                    &port_msi_target,
                    port_def.settings,
                );
                (placement.devfn, port_name, port)
            })
            .collect();

        // `downstream_ports` is searched by devfn, so keep it sorted; explicit
        // devfns may be assigned out of order.
        downstream_ports.sort_by_key(|(devfn, _, _)| *devfn);

        Ok(Self {
            name: definition.name,
            upstream_port,
            downstream_ports,
            bus_cfg_handler: PciBusCfgAccessHandler::new(),
        })
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
    pub fn downstream_ports(&self) -> Vec<crate::root::DownstreamPortInfo> {
        self.downstream_ports
            .iter()
            .map(|(devfn, name, dsp)| crate::root::DownstreamPortInfo {
                devfn: *devfn,
                name: name.clone(),
                bus_range: dsp.port.bus_range(),
            })
            .collect()
    }

    /// Handle direct configuration space read to downstream switch ports.
    fn handle_downstream_port_read(
        &mut self,
        addr: PciConfigAddress,
        value: ByteEnabledDwordRead<'_>,
    ) -> Option<IoResult> {
        let (_, _, downstream_port) = self
            .downstream_ports
            .iter_mut()
            .find(|(devfn, _, _)| *devfn == addr.devfn)?;
        Some(downstream_port.port.cfg_space.read(addr, value))
    }

    /// Handle direct configuration space write to downstream switch ports.
    fn handle_downstream_port_write(
        &mut self,
        addr: PciConfigAddress,
        value: ByteEnabledDwordWrite,
    ) -> Option<IoResult> {
        let (_, _, downstream_port) = self
            .downstream_ports
            .iter_mut()
            .find(|(devfn, _, _)| *devfn == addr.devfn)?;
        Some(downstream_port.port.cfg_space.write(addr, value))
    }

    /// Attach the provided `GenericPciBusDevice` to the port identified.
    pub fn add_pcie_device(
        &mut self,
        port_devfn: u8,
        name: &str,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> anyhow::Result<()> {
        let (_, port_name, downstream_port) = self
            .downstream_ports
            .iter_mut()
            .find(|(devfn, _, _)| *devfn == port_devfn)
            .ok_or_else(|| anyhow::anyhow!("port devfn {} not found", port_devfn))?;
        downstream_port
            .port
            .add_pcie_device(port_name.as_ref(), name, dev)
            .context("failed to add PCIe device to downstream port")?;
        Ok(())
    }
}

impl ChangeDeviceState for GenericPcieSwitch {
    /// No-op start hook: switch state is fully modeled in config-space state.
    fn start(&mut self) {}

    /// No-op stop hook: no background tasks or external resources to drain.
    async fn stop(&mut self) {}

    /// Resets upstream and downstream bridge config-space state to power-on defaults.
    async fn reset(&mut self) {
        // Reset the upstream port configuration space
        self.upstream_port.cfg_space.reset();

        // Reset all downstream port configuration spaces
        for (_, _, downstream_port) in self.downstream_ports.iter_mut() {
            downstream_port.port.cfg_space.reset();
        }
    }
}

impl ChipsetDevice for GenericPcieSwitch {
    /// Exposes this switch as a PCI config-space device to the chipset bus.
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    /// Exposes this switch as a device that can be polled for deferred completions.
    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PciConfigSpace for GenericPcieSwitch {
    /// Reads the switch's own upstream-port config space (Type 0 view).
    fn pci_cfg_read(&mut self, byte_offset: u16, value: ByteEnabledDwordRead<'_>) -> IoResult {
        self.upstream_port
            .cfg_space
            .read_byte_enabled(byte_offset, value)
    }

    /// Writes the switch's own upstream-port config space (Type 0 view).
    fn pci_cfg_write(&mut self, byte_offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        self.upstream_port
            .cfg_space
            .write_byte_enabled(byte_offset, value)
    }

    fn pci_cfg_read_with_routing(
        &mut self,
        access_type: PciConfigAccessType,
        address: PciConfigAddress,
        mut value: ByteEnabledDwordRead<'_>,
    ) -> IoResult {
        // Type 0 accesses are handled by the upstream port emulator, only devfn 0 is valid.
        if access_type == PciConfigAccessType::Type0 {
            if address.devfn == 0 {
                return self.upstream_port.cfg_space.read(address, value);
            } else {
                value.set(!0);
                return IoResult::Ok;
            }
        }

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration
        let upstream_bus_range = self.upstream_port.cfg_space.assigned_bus_range();
        if upstream_bus_range == (0..=0) {
            value.set(!0);
            return IoResult::Ok;
        }

        // If the target bus is not within the upstream bus range, we cannot route it. Return all-1s.
        if !upstream_bus_range.contains(&address.bus) {
            value.set(!0);
            return IoResult::Ok;
        }

        // If the target bus is the start of the upstream bus range, this access targets one of the
        // downstream ports on the internal bus of the switch.
        if address.bus == *upstream_bus_range.start() {
            return self
                .handle_downstream_port_read(address, value.reborrow())
                .unwrap_or_else(|| {
                    value.set(!0);
                    IoResult::Ok
                });
        }

        // The access must be routed somewhere downstream of a downstream port, invoke the
        // config space handler for dealing with deferrals and such.
        let mut callback = PciBusCfgAccessCallbackView::new(&mut self.downstream_ports);
        self.bus_cfg_handler.read(address, value, &mut callback)
    }

    fn pci_cfg_write_with_routing(
        &mut self,
        access_type: PciConfigAccessType,
        address: PciConfigAddress,
        value: ByteEnabledDwordWrite,
    ) -> IoResult {
        // Type 0 accesses are handled by the upstream port emulator, only devfn 0 is valid.
        if access_type == PciConfigAccessType::Type0 {
            if address.devfn == 0 {
                return self.upstream_port.cfg_space.write(address, value);
            } else {
                return IoResult::Ok;
            }
        }

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration.
        let upstream_bus_range = self.upstream_port.cfg_space.assigned_bus_range();
        if upstream_bus_range == (0..=0) {
            return IoResult::Ok;
        }

        // If the target bus is not within the upstream bus range, we cannot route it.
        if !upstream_bus_range.contains(&address.bus) {
            return IoResult::Ok;
        }

        // If the target bus is the start of the upstream bus range, this access targets one of the
        // downstream ports on the internal bus of the switch.
        if address.bus == *upstream_bus_range.start() {
            return self
                .handle_downstream_port_write(address, value)
                .unwrap_or(IoResult::Ok);
        }

        // The access must be routed somewhere downstream of a downstream port, invoke the
        // config space handler for dealing with deferrals and such.
        let mut callback = PciBusCfgAccessCallbackView::new(&mut self.downstream_ports);
        self.bus_cfg_handler.write(address, value, &mut callback)
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        // PCIe switches typically don't have a fixed BDF requirement
        None
    }
}

impl PollDevice for GenericPcieSwitch {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.bus_cfg_handler.poll(cx);
    }
}

struct PciBusCfgAccessCallbackView<'a> {
    downstream_ports: &'a mut Vec<(u8, Arc<str>, DownstreamSwitchPort)>,
}

impl<'a> PciBusCfgAccessCallbackView<'a> {
    fn new(downstream_ports: &'a mut Vec<(u8, Arc<str>, DownstreamSwitchPort)>) -> Self {
        Self { downstream_ports }
    }
}

impl<'a> PciBusCfgAccessCallbacks for PciBusCfgAccessCallbackView<'a> {
    fn read(&mut self, addr: PciConfigAddress, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        for (_, _, downstream_port) in self.downstream_ports.iter_mut() {
            let downstream_bus_range = downstream_port.cfg_space().assigned_bus_range();

            // Skip downstream ports with invalid/uninitialized bus configuration
            if downstream_bus_range == (0..=0) {
                continue;
            }

            if downstream_bus_range.contains(&addr.bus) {
                return downstream_port
                    .port
                    .forward_cfg_read_with_routing(addr, value);
            }
        }

        // No downstream port could handle this bus number
        value.set(!0);
        IoResult::Ok
    }

    /// Route configuration space write to downstream ports for further forwarding.
    fn write(&mut self, addr: PciConfigAddress, value: ByteEnabledDwordWrite) -> IoResult {
        for (_, _, downstream_port) in self.downstream_ports.iter_mut() {
            let downstream_bus_range = downstream_port.cfg_space().assigned_bus_range();

            // Skip downstream ports with invalid/uninitialized bus configuration
            if downstream_bus_range == (0..=0) {
                continue;
            }

            if downstream_bus_range.contains(&addr.bus) {
                return downstream_port
                    .port
                    .forward_cfg_write_with_routing(addr, value);
            }
        }

        // No downstream port could handle this bus number
        IoResult::Ok
    }
}

mod save_restore {
    use super::*;
    use std::collections::HashSet;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use super::ConfigSpaceType1Emulator;
        use super::SaveRestore;
        use cxl_spec::CxlComponentRegisters;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        type SwitchPortCfgSpaceSavedState = <ConfigSpaceType1Emulator as SaveRestore>::SavedState;
        type CxlComponentRegistersSavedState = <CxlComponentRegisters as SaveRestore>::SavedState;

        /// Saved state for one switch port config space.
        #[derive(Protobuf)]
        #[mesh(package = "pcie.switch")]
        pub struct DownstreamPortSavedState {
            /// The devfn of this downstream port.
            #[mesh(1)]
            pub devfn: u8,
            /// The downstream port Type 1 configuration space state.
            #[mesh(2)]
            pub cfg_space: SwitchPortCfgSpaceSavedState,
            /// Optional CXL component-register state for this downstream port.
            #[mesh(3)]
            pub cxl_component_registers: Option<CxlComponentRegistersSavedState>,
        }

        /// Saved state for the GenericPcieSwitch.
        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pcie.switch")]
        pub struct SavedState {
            /// The upstream port configuration space state.
            #[mesh(1)]
            pub upstream_cfg_space: SwitchPortCfgSpaceSavedState,
            /// Saved state for downstream ports.
            ///
            /// `devfn` identifies the target port for each entry.
            /// The vector ordering is not part of the saved-state contract.
            #[mesh(2)]
            pub downstream_ports: Vec<DownstreamPortSavedState>,
        }
    }

    impl SaveRestore for GenericPcieSwitch {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            // Save the upstream port configuration space
            let upstream_cfg_space = self.upstream_port.cfg_space.save()?;

            // Save all downstream ports (already sorted by devfn in the Vec).
            let mut downstream_ports = Vec::with_capacity(self.downstream_ports.len());
            for (devfn, _, downstream_port) in self.downstream_ports.iter_mut() {
                downstream_ports.push(state::DownstreamPortSavedState {
                    devfn: *devfn,
                    cfg_space: downstream_port.port.cfg_space.save()?,
                    cxl_component_registers: downstream_port
                        .port
                        .save_cxl_component_registers_state()?,
                });
            }

            Ok(state::SavedState {
                upstream_cfg_space,
                downstream_ports,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                upstream_cfg_space,
                downstream_ports,
            } = state;

            // Reject snapshots from a different topology shape.
            if downstream_ports.len() != self.downstream_ports.len() {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "downstream port count mismatch: saved {}, current {}",
                    downstream_ports.len(),
                    self.downstream_ports.len()
                )));
            }

            // Restore the upstream port configuration space
            self.upstream_port.cfg_space.restore(upstream_cfg_space)?;

            let mut seen_ports = HashSet::with_capacity(downstream_ports.len());

            // Restore all downstream ports by devfn index.
            for port_state in downstream_ports {
                // Duplicate entries indicate corrupted or malformed saved state.
                if !seen_ports.insert(port_state.devfn) {
                    return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "duplicate downstream port devfn {} in saved state",
                        port_state.devfn
                    )));
                }

                if let Some((_, _, downstream_port)) = self
                    .downstream_ports
                    .iter_mut()
                    .find(|(devfn, _, _)| *devfn == port_state.devfn)
                {
                    downstream_port
                        .port
                        .cfg_space
                        .restore(port_state.cfg_space)?;
                    downstream_port.port.restore_cxl_component_registers_state(
                        port_state.cxl_component_registers,
                    )?;
                } else {
                    return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "downstream port devfn {} not found",
                        port_state.devfn
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
    use pci_core::msi::MsiConnection;
    use pci_core::test_helpers::TestCfgAccess;

    /// Builds a switch definition with `downstream_port_count` uniform
    /// downstream ports named `{name}-downstream-{i}`, mirroring the naming
    /// convention used by the rest of the topology.
    fn switch_def(
        name: &str,
        downstream_port_count: u8,
        hotplug: bool,
        dsp_settings: PciePortSettings,
        msi_target: MsiTarget,
    ) -> GenericPcieSwitchDefinition {
        GenericPcieSwitchDefinition {
            name: name.into(),
            downstream_ports: (0..downstream_port_count)
                .map(|i| crate::GenericPciePortDefinition {
                    name: format!("{name}-downstream-{i}").into(),
                    devfn: None,
                    hotplug,
                    settings: dsp_settings.clone(),
                })
                .collect(),
            msi_target,
        }
    }

    /// Builds a switch from a definition, unwrapping the (test-only) result.
    fn build(definition: GenericPcieSwitchDefinition) -> GenericPcieSwitch {
        GenericPcieSwitch::new(definition).unwrap()
    }

    fn tlp_prefixing_settings(max_prefixes: MaxEndEndTlpPrefixes) -> PciePortSettings {
        PciePortSettings {
            tlp_prefixing_supported: Some(max_prefixes),
            ..PciePortSettings::default()
        }
    }

    #[test]
    fn test_upstream_switch_port_creation() {
        // Verify that we can read the vendor/device ID from config space
        let port = UpstreamSwitchPort::new(false);
        let vendor_device_id = port.cfg_space.read_u32(0x0);
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);

        let port = UpstreamSwitchPort::new(true);
        let vendor_device_id = port.cfg_space.read_u32(0x0);
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);
    }

    #[test]
    fn test_switch_rejects_inconsistent_tlp_prefixing_support() {
        let mut definition = switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        );
        definition.downstream_ports[1].settings =
            tlp_prefixing_settings(MaxEndEndTlpPrefixes::Four);

        let Err(err) = GenericPcieSwitch::new(definition) else {
            panic!("switch with inconsistent TLP prefixing support should fail");
        };
        assert!(matches!(
            err,
            InvalidSwitchError::InconsistentEndToEndTlpPrefixing
        ));
    }

    #[test]
    fn test_switch_rejects_non_four_tlp_prefixing_support() {
        for max_prefixes in [
            MaxEndEndTlpPrefixes::One,
            MaxEndEndTlpPrefixes::Two,
            MaxEndEndTlpPrefixes::Three,
        ] {
            let definition = switch_def(
                "test-switch",
                2,
                false,
                tlp_prefixing_settings(max_prefixes),
                MsiTarget::disconnected(),
            );

            let Err(err) = GenericPcieSwitch::new(definition) else {
                panic!("switch with {max_prefixes:?} TLP prefixes should fail");
            };
            assert!(matches!(
                err,
                InvalidSwitchError::InvalidMaxEndToEndTlpPrefixing {
                    supported_prefixes,
                    ..
                } if supported_prefixes == max_prefixes.into_bits()
            ));
        }
    }

    #[test]
    fn test_switch_with_four_tlp_prefixes_reports_upstream_support() {
        let switch = build(switch_def(
            "test-switch",
            2,
            false,
            tlp_prefixing_settings(MaxEndEndTlpPrefixes::Four),
            MsiTarget::disconnected(),
        ));
        let device_caps_2 = pci_core::spec::caps::pci_express::DeviceCapabilities2::from_bits(
            switch.upstream_port.cfg_space().read_u32(0x64),
        );

        assert!(device_caps_2.extended_fmt_field_supported());
        assert!(device_caps_2.end_end_tlp_prefix_supported());
        assert_eq!(
            device_caps_2.max_end_end_tlp_prefixes().into_bits(),
            MaxEndEndTlpPrefixes::Four.into_bits()
        );
    }

    #[test]
    fn test_downstream_switch_port_creation() {
        let port = DownstreamSwitchPort::new(
            "test-downstream-port",
            None,
            None,
            &MsiTarget::disconnected(),
            PciePortSettings::default(),
        );
        assert!(port.port.link.is_none());

        // Verify that we can read the vendor/device ID from config space
        let vendor_device_id = port.port.cfg_space.read_u32(0x0);
        let expected = (DOWNSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);
    }

    #[test]
    fn test_downstream_switch_port_multi_function_options() {
        // Test with default multi_function (false)
        let port_default = DownstreamSwitchPort::new(
            "test-port-default",
            None,
            None,
            &MsiTarget::disconnected(),
            PciePortSettings::default(),
        );
        let header_type_value = port_default.cfg_space().read_u32(0x0C);
        let header_type_field = (header_type_value >> 16) & 0xFF;
        assert_eq!(
            header_type_field & 0x80,
            0x00,
            "Multi-function bit should NOT be set with None parameter"
        );

        // Test with explicit multi_function false
        let port_false = DownstreamSwitchPort::new(
            "test-port-false",
            Some(false),
            None,
            &MsiTarget::disconnected(),
            PciePortSettings::default(),
        );
        let header_type_value_false = port_false.cfg_space().read_u32(0x0C);
        let header_type_field_false = (header_type_value_false >> 16) & 0xFF;
        assert_eq!(
            header_type_field_false & 0x80,
            0x00,
            "Multi-function bit should NOT be set with Some(false)"
        );

        // Test with explicit multi_function true
        let port_true = DownstreamSwitchPort::new(
            "test-port-true",
            Some(true),
            None,
            &MsiTarget::disconnected(),
            PciePortSettings::default(),
        );
        let header_type_value_true = port_true.cfg_space().read_u32(0x0C);
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
        let port_no_hotplug = DownstreamSwitchPort::new(
            "test-port-no-hotplug",
            None,
            None,
            &MsiTarget::disconnected(),
            PciePortSettings::default(),
        );
        // We can't easily verify hotplug is disabled without accessing internal state,
        // but we can verify the port was created successfully
        let vendor_device_id = port_no_hotplug.cfg_space().read_u32(0x0);
        let expected = (DOWNSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);

        // Test with hotplug enabled (Some(slot_number))
        let port_with_hotplug = DownstreamSwitchPort::new(
            "test-port-hotplug",
            None,
            Some(42),
            &MsiTarget::disconnected(),
            PciePortSettings::default(),
        );
        let vendor_device_id_hotplug = port_with_hotplug.cfg_space().read_u32(0x0);
        assert_eq!(vendor_device_id_hotplug, expected);
        // The slot number and hotplug capability would be tested via PCIe capability registers
        // but that requires more complex setup
    }

    #[test]
    fn test_switch_creation() {
        let switch = build(switch_def(
            "test-switch",
            3,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        assert_eq!(switch.name().as_ref(), "test-switch");
        assert_eq!(switch.downstream_ports().len(), 3);

        // Verify downstream port names (HashMap doesn't guarantee order, so check each one exists)
        let ports = switch.downstream_ports();
        let port_names: std::collections::HashSet<_> =
            ports.iter().map(|p| p.name.as_ref()).collect();
        assert!(port_names.contains("test-switch-downstream-0"));
        assert!(port_names.contains("test-switch-downstream-1"));
        assert!(port_names.contains("test-switch-downstream-2"));

        // Verify port numbers
        let port_numbers: std::collections::HashSet<_> = ports.iter().map(|p| p.devfn).collect();
        assert!(port_numbers.contains(&0));
        assert!(port_numbers.contains(&1));
        assert!(port_numbers.contains(&2));
    }

    #[test]
    fn test_switch_device_connections() {
        use crate::test_helpers::TestPcieEndpoint;
        use chipset_device::io::IoError;

        let mut switch = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        let downstream_device = TestPcieEndpoint::new(
            |offset, mut value| match offset {
                0x0 => {
                    value.set(0xABCD_EF01);
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
                    Box::new(downstream_device),
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

        let mut switch = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Verify that Switch implements routing functionality by testing add_pcie_device method
        // This tests that the switch can accept device connections (routing capability)
        let test_device =
            TestPcieEndpoint::new(|_, _| Some(IoResult::Ok), |_, _| Some(IoResult::Ok));
        let add_result = switch.add_pcie_device(0, "test-device", Box::new(test_device));
        // Should succeed for port 0 (first downstream port)
        assert!(add_result.is_ok());

        // Test basic configuration space access through the PCI interface
        let value = switch.upstream_port.cfg_space_mut().read_u32(0x0);

        // Verify vendor/device ID is from the upstream port
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(value, expected);
    }

    #[test]
    fn test_switch_chipset_device() {
        use chipset_device::ChipsetDevice;
        use chipset_device::pci::PciConfigSpace;

        let mut switch = build(switch_def(
            "test-switch",
            4,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Test that it supports PCI but not other interfaces
        assert!(switch.supports_pci().is_some());
        assert!(switch.supports_poll_device().is_some());
        assert!(switch.supports_mmio().is_none());
        assert!(switch.supports_pio().is_none());

        // Test PciConfigSpace interface
        let mut value = 0u32;
        let result = PciConfigSpace::pci_cfg_read(
            &mut switch,
            0x0,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        );
        assert!(matches!(result, IoResult::Ok));

        // Verify we get the expected vendor/device ID
        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(value, expected);

        // Test write operation
        let result = PciConfigSpace::pci_cfg_write(
            &mut switch,
            0x4,
            ByteEnabledDwordWrite::with_all_bytes_enabled(0x12345678),
        );
        assert!(matches!(result, IoResult::Ok));
    }

    #[test]
    fn test_switch_default() {
        let switch = build(switch_def(
            "default-switch",
            4,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));
        assert_eq!(switch.name().as_ref(), "default-switch");
        assert_eq!(switch.downstream_ports().len(), 4);
    }

    #[test]
    fn test_switch_large_downstream_port_count() {
        let switch = build(switch_def(
            "test-switch",
            16,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));
        assert_eq!(switch.downstream_ports().len(), 16);
    }

    #[test]
    fn test_switch_downstream_port_direct_access() {
        let mut switch = build(switch_def(
            "test-switch",
            3,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        let secondary_bus = 1u8;
        let subordinate_bus = 10u8;
        // Set secondary bus number (offset 0x18) - bits 8-15 of the 32-bit value at 0x18
        let bus_config = (subordinate_bus as u32) << 16 | ((secondary_bus as u32) << 8);
        let result = switch.pci_cfg_write(
            0x18,
            ByteEnabledDwordWrite::with_all_bytes_enabled(bus_config),
        );
        assert!(matches!(result, IoResult::Ok));

        let bus_range = switch.upstream_port.cfg_space().assigned_bus_range();
        assert_eq!(bus_range, 1..=10);
        let switch_internal_bus = *bus_range.start(); // This is the secondary bus

        // Test direct access to downstream port 0 using function = 0
        let mut value = 0u32;
        let result = switch.pci_cfg_read_with_routing(
            PciConfigAccessType::Type1,
            PciConfigAddress::new(switch_internal_bus, 0, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        );
        assert!(matches!(result, IoResult::Ok));

        // Verify we got the downstream switch port's vendor/device ID
        let expected = (DOWNSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(value, expected);

        // Test direct access to downstream port 2 using function = 2
        let mut value2 = 0u32;
        let result2 = switch.pci_cfg_read_with_routing(
            PciConfigAccessType::Type1,
            PciConfigAddress::new(switch_internal_bus, 2, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value2),
        );
        assert!(matches!(result2, IoResult::Ok));
        assert_eq!(value2, expected);

        // Test access to non-existent downstream port using function = 5
        let mut value3 = 0u32;
        let result3 = switch.pci_cfg_read_with_routing(
            PciConfigAccessType::Type1,
            PciConfigAddress::new(switch_internal_bus, 5, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value3),
        );
        assert!(matches!(result3, IoResult::Ok));
        assert_eq!(value3, !0);
    }

    #[test]
    fn test_switch_invalid_bus_range_handling() {
        let mut switch = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Don't configure bus numbers, so the range should be 0..=0 (invalid)
        let bus_range = switch.upstream_port.cfg_space().assigned_bus_range();
        assert_eq!(bus_range, 0..=0);

        // Test that any access returns 1s when bus range is invalid
        let mut value = 0u32;
        let result = switch.pci_cfg_read_with_routing(
            PciConfigAccessType::Type1,
            PciConfigAddress::new(1, 0, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        );
        assert!(matches!(result, IoResult::Ok));
        assert_eq!(value, !0);

        let result2 = switch.pci_cfg_read_with_routing(
            PciConfigAccessType::Type1,
            PciConfigAddress::new(1, 0, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        );
        assert!(matches!(result2, IoResult::Ok));
        assert_eq!(value, !0);

        let result3 = switch.pci_cfg_read_with_routing(
            PciConfigAccessType::Type1,
            PciConfigAddress::new(2, 0, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        );
        assert!(matches!(result3, IoResult::Ok));
        assert_eq!(value, !0);
    }

    #[test]
    fn test_switch_downstream_port_invalid_bus_range_skipping() {
        let mut switch = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Configure the upstream port with a valid bus range
        let secondary_bus = 1u8;
        let subordinate_bus = 10u8;
        let primary_bus = 0u8;
        let bus_config =
            ((subordinate_bus as u32) << 16) | ((secondary_bus as u32) << 8) | (primary_bus as u32); // subordinate | secondary | primary
        switch
            .upstream_port
            .cfg_space_mut()
            .write_u32(0x18, bus_config);

        // Downstream ports still have invalid bus ranges (0..=0 by default)
        // so any access to buses beyond the secondary bus should return 1s.
        let mut value = 0u32;

        // Access to bus 2 should return 1s since no downstream port has a valid bus range
        let result = switch.pci_cfg_read_with_routing(
            PciConfigAccessType::Type1,
            PciConfigAddress::new(2, 0, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        );
        assert!(matches!(result, IoResult::Ok));
        assert_eq!(value, !0);

        // Access to bus 5 should also return 1s
        let result2 = switch.pci_cfg_read_with_routing(
            PciConfigAccessType::Type1,
            PciConfigAddress::new(5, 0, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        );
        assert!(matches!(result2, IoResult::Ok));
        assert_eq!(value, !0);

        // Access to the secondary bus (switch internal) should still work for downstream port config
        let result3 = switch.pci_cfg_read_with_routing(
            PciConfigAccessType::Type1,
            PciConfigAddress::new(secondary_bus, 0, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        );
        assert!(matches!(result3, IoResult::Ok));
        assert!(value != !0);
    }

    #[test]
    fn test_switch_multi_function_bit() {
        // Test that switches with multiple downstream ports set the multi-function bit
        let multi_port_switch = build(switch_def(
            "multi-port-switch",
            3,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Verify each downstream port has the multi-function bit set
        for p in multi_port_switch.downstream_ports() {
            let port_num = p.devfn;
            if let Some((_, _, downstream_port)) =
                multi_port_switch.downstream_ports.get(port_num as usize)
            {
                let header_type_value = downstream_port.cfg_space().read_u32(0x0C);

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
        let single_port_switch = build(switch_def(
            "single-port-switch",
            1,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Verify the single downstream port does NOT have the multi-function bit set
        for p in single_port_switch.downstream_ports() {
            let port_num = p.devfn;
            if let Some((_, _, downstream_port)) =
                single_port_switch.downstream_ports.get(port_num as usize)
            {
                let header_type_value = downstream_port.cfg_space().read_u32(0x0C);

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
        let switch_no_hotplug = build(switch_def(
            "test-switch-no-hotplug",
            1,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));
        assert_eq!(switch_no_hotplug.name().as_ref(), "test-switch-no-hotplug");

        // Test hotplug enabled
        let switch_with_hotplug = build(switch_def(
            "test-switch-with-hotplug",
            1,
            true,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));
        assert_eq!(
            switch_with_hotplug.name().as_ref(),
            "test-switch-with-hotplug"
        );
    }

    #[test]
    fn test_save_restore_port_mismatch_error() {
        use vmcore::save_restore::SaveRestore;

        // Create a switch with 3 downstream ports
        let mut switch = build(switch_def(
            "test-switch",
            3,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Save the state
        let saved_state = switch.save().expect("save should succeed");
        assert_eq!(saved_state.downstream_ports.len(), 3);

        // Create a new switch with only 2 downstream ports
        let mut switch2 = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Restore should fail because port 2 doesn't exist in the new switch
        let result = switch2.restore(saved_state);
        assert!(result.is_err());
    }

    #[test]
    fn test_save_restore_basic() {
        use vmcore::save_restore::SaveRestore;

        let mut switch = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Save the initial state
        let saved_state = switch.save().expect("save should succeed");

        // Verify the saved state has the correct number of downstream ports
        assert_eq!(saved_state.downstream_ports.len(), 2);

        // Restore the state to a new switch
        let mut switch2 = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));
        switch2
            .restore(saved_state)
            .expect("restore should succeed");
    }

    #[test]
    fn test_save_restore_with_bus_configuration() {
        use vmcore::save_restore::SaveRestore;

        let mut switch = build(switch_def(
            "test-switch",
            3,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Configure bus numbers on the upstream port
        let secondary_bus = 5u8;
        let subordinate_bus = 15u8;
        let primary_bus = 0u8;
        let bus_config =
            ((subordinate_bus as u32) << 16) | ((secondary_bus as u32) << 8) | (primary_bus as u32);
        switch
            .upstream_port
            .cfg_space_mut()
            .write_u32(0x18, bus_config);

        // Verify the bus range is set
        let bus_range = switch.upstream_port.cfg_space().assigned_bus_range();
        assert_eq!(*bus_range.start(), secondary_bus);
        assert_eq!(*bus_range.end(), subordinate_bus);

        // Save the state
        let saved_state = switch.save().expect("save should succeed");

        // Create a new switch and restore the state
        let mut switch2 = build(switch_def(
            "test-switch",
            3,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

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

        let mut switch = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Configure bus numbers on one of the downstream ports
        // First, we need to get access to the downstream port and configure it
        if let Some((_, _, downstream_port)) = switch.downstream_ports.get_mut(0) {
            let secondary_bus = 10u8;
            let subordinate_bus = 20u8;
            let primary_bus = 5u8;
            let bus_config = ((subordinate_bus as u32) << 16)
                | ((secondary_bus as u32) << 8)
                | (primary_bus as u32);
            downstream_port.port.cfg_space.write_u32(0x18, bus_config);
        }

        // Verify the downstream port bus range is set
        if let Some((_, _, downstream_port)) = switch.downstream_ports.first() {
            let bus_range = downstream_port.cfg_space().assigned_bus_range();
            assert_eq!(*bus_range.start(), 10);
            assert_eq!(*bus_range.end(), 20);
        }

        // Save the state
        let saved_state = switch.save().expect("save should succeed");

        // Create a new switch and restore the state
        let mut switch2 = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Verify the new switch has default bus range on downstream port before restore
        if let Some((_, _, downstream_port)) = switch2.downstream_ports.first() {
            let default_bus_range = downstream_port.cfg_space().assigned_bus_range();
            assert_eq!(default_bus_range, 0..=0);
        }

        // Restore the state
        switch2
            .restore(saved_state)
            .expect("restore should succeed");

        // Verify the downstream port bus range is restored
        if let Some((_, _, downstream_port)) = switch2.downstream_ports.first() {
            let restored_bus_range = downstream_port.cfg_space().assigned_bus_range();
            assert_eq!(*restored_bus_range.start(), 10);
            assert_eq!(*restored_bus_range.end(), 20);
        }
    }

    #[test]
    fn test_save_restore_preserves_upstream_and_downstream_cfg_space() {
        use vmcore::save_restore::SaveRestore;

        let mut switch = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        // Upstream primary/secondary/subordinate bus numbers.
        switch
            .upstream_port
            .cfg_space_mut()
            .write_u32(0x18, 0x0014_1200);

        // Downstream port 1 bus range.
        if let Some((_, _, downstream_port)) = switch.downstream_ports.get_mut(1) {
            downstream_port.port.cfg_space.write_u32(0x18, 0x0020_1f12);
        }

        let saved_state = switch.save().expect("save should succeed");

        let mut switch2 = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        switch2
            .restore(saved_state)
            .expect("restore should succeed");

        let upstream_bus_range = switch2.upstream_port.cfg_space().assigned_bus_range();
        assert_eq!(*upstream_bus_range.start(), 0x12);
        assert_eq!(*upstream_bus_range.end(), 0x14);

        if let Some((_, _, downstream_port)) = switch2.downstream_ports.get(1) {
            let downstream_bus_range = downstream_port.cfg_space().assigned_bus_range();
            assert_eq!(*downstream_bus_range.start(), 0x1f);
            assert_eq!(*downstream_bus_range.end(), 0x20);
        } else {
            panic!("missing downstream port 1");
        }
    }

    /// Adapts a `GenericPcieSwitch` to the `GenericPciBusDevice` trait so it
    /// can be attached to a downstream port as a linked device in tests.
    struct SwitchAdapter(GenericPcieSwitch);

    impl GenericPciBusDevice for SwitchAdapter {
        fn pci_cfg_read(
            &mut self,
            offset: u16,
            value: ByteEnabledDwordRead<'_>,
        ) -> Option<IoResult> {
            Some(PciConfigSpace::pci_cfg_read(&mut self.0, offset, value))
        }

        fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> Option<IoResult> {
            Some(PciConfigSpace::pci_cfg_write(&mut self.0, offset, value))
        }

        fn pci_cfg_read_with_routing(
            &mut self,
            access_type: PciConfigAccessType,
            address: PciConfigAddress,
            value: ByteEnabledDwordRead<'_>,
        ) -> Option<IoResult> {
            Some(PciConfigSpace::pci_cfg_read_with_routing(
                &mut self.0,
                access_type,
                address,
                value,
            ))
        }

        fn pci_cfg_write_with_routing(
            &mut self,
            access_type: PciConfigAccessType,
            address: PciConfigAddress,
            value: ByteEnabledDwordWrite,
        ) -> Option<IoResult> {
            Some(PciConfigSpace::pci_cfg_write_with_routing(
                &mut self.0,
                access_type,
                address,
                value,
            ))
        }
    }

    #[test]
    fn test_switch_enumeration_through_port() {
        use crate::port::PcieDownstreamPort;
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

        let msi_conn = MsiConnection::new();
        let mut port = PcieDownstreamPort::new(
            "root-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_conn.target(),
            PciePortSettings::default(),
            None,
            None,
        );

        // Configure the root port's bus range: secondary=1, subordinate=10
        port.cfg_space.write_u32(0x18, (10u32 << 16) | (1u32 << 8));

        // Create and attach a switch behind the port
        let switch = build(switch_def(
            "test-switch",
            2,
            false,
            PciePortSettings::default(),
            MsiTarget::disconnected(),
        ));

        port.link = Some(("switch".into(), Box::new(SwitchAdapter(switch))));

        // Type 0 config read to bus 1 (secondary), function 0 — this should
        // read the switch's upstream port config space and return its
        // vendor/device ID.
        let mut value = 0u32;
        let addr = PciConfigAddress::new(1, 0, 0x0).unwrap();
        let result = port.forward_cfg_read_with_routing(
            addr,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        );
        assert!(matches!(result, IoResult::Ok));

        let expected = (UPSTREAM_SWITCH_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(
            value, expected,
            "Type 0 access to bus 1 function 0 must read the switch's upstream port"
        );

        // Non-zero function on the same bus should return no device (switch
        // upstream port is single-function).
        let mut value2 = 0u32;
        let addr2 = PciConfigAddress::new(1, 1, 0x0).unwrap();
        let result2 = port.forward_cfg_read_with_routing(
            addr2,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value2),
        );
        assert!(matches!(result2, IoResult::Ok));
        assert_eq!(
            value2, !0,
            "Non-zero function should return all-1s (no device)"
        );
    }

    #[test]
    fn test_switch_acs_only_applies_to_dsp() {
        use pci_core::spec::caps::ExtendedCapabilityId;

        let switch = build(switch_def(
            "acs-switch",
            1,
            false,
            PciePortSettings {
                acs_capabilities_supported: 0x0001,
                ..Default::default()
            },
            MsiTarget::disconnected(),
        ));

        // Upstream switch ports do not expose ACS in this model.
        let upstream_header = switch.upstream_port().cfg_space().read_u32(0x100);
        assert_eq!(upstream_header, 0);

        let mut switch = switch;
        let (_, _, downstream_port) = switch
            .downstream_ports
            .iter_mut()
            .next()
            .expect("expected downstream port");

        let downstream_header = downstream_port.cfg_space_mut().read_u32(0x100);
        assert_eq!(
            downstream_header & 0xffff,
            ExtendedCapabilityId::ACS.0 as u32
        );

        let caps_control = downstream_port.cfg_space_mut().read_u32(0x104);
        assert_eq!(caps_control as u16, 0x0001);
    }

    #[test]
    fn test_switch_explicit_devfn() {
        // A downstream port with an explicit devfn is placed there; the
        // remaining ports fill the lowest free devfns from 0.
        let definition = GenericPcieSwitchDefinition {
            name: "sw".into(),
            downstream_ports: vec![
                crate::GenericPciePortDefinition {
                    name: "a".into(),
                    devfn: Some(16), // device 2, function 0
                    hotplug: false,
                    settings: PciePortSettings::default(),
                },
                crate::GenericPciePortDefinition {
                    name: "b".into(),
                    devfn: None,
                    hotplug: false,
                    settings: PciePortSettings::default(),
                },
            ],
            msi_target: MsiTarget::disconnected(),
        };
        let switch = GenericPcieSwitch::new(definition).unwrap();
        let mut ports: Vec<_> = switch
            .downstream_ports()
            .into_iter()
            .map(|p| (p.name.to_string(), p.devfn))
            .collect();
        ports.sort_by_key(|(_, devfn)| *devfn);
        assert_eq!(ports, vec![("b".into(), 0), ("a".into(), 16)]);
    }

    #[test]
    fn test_switch_cxl_rejected() {
        // CXL is not supported on switch downstream ports.
        let definition = GenericPcieSwitchDefinition {
            name: "sw".into(),
            downstream_ports: vec![crate::GenericPciePortDefinition {
                name: "a".into(),
                devfn: None,
                hotplug: false,
                settings: PciePortSettings {
                    cxl_flex_bus_port_capability: Some(Default::default()),
                    ..Default::default()
                },
            }],
            msi_target: MsiTarget::disconnected(),
        };
        assert!(matches!(
            GenericPcieSwitch::new(definition),
            Err(InvalidSwitchError::CxlUnsupported { .. })
        ));
    }

    #[test]
    fn test_switch_duplicate_devfn_rejected() {
        let definition = GenericPcieSwitchDefinition {
            name: "sw".into(),
            downstream_ports: vec![
                crate::GenericPciePortDefinition {
                    name: "a".into(),
                    devfn: Some(0),
                    hotplug: false,
                    settings: PciePortSettings::default(),
                },
                crate::GenericPciePortDefinition {
                    name: "b".into(),
                    devfn: Some(0),
                    hotplug: false,
                    settings: PciePortSettings::default(),
                },
            ],
            msi_target: MsiTarget::disconnected(),
        };
        assert!(matches!(
            GenericPcieSwitch::new(definition),
            Err(InvalidSwitchError::Devfn(PortDevfnError::DevfnInUse {
                devfn: 0,
                ..
            }))
        ));
    }
}
