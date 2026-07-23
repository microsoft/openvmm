// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common PCIe port implementation shared between different port types.

use anyhow::bail;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigAccessType;
use chipset_device::pci::PciConfigAddress;
use cxl_spec::CxlComponentRegisters;
use cxl_spec::CxlFlexBusPortDvsecExtendedCapability;
use cxl_spec::CxlPortDvsecExtendedCapability;
use cxl_spec::CxlRegisterLocatorDvsecExtendedCapability;
use cxl_spec::pci_registers::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecCapability;
use cxl_spec::pci_registers::spec::register_locator_dvsec::CxlRegisterLocatorRegisterBir;
use cxl_spec::pci_registers::spec::register_locator_dvsec::CxlRegisterLocatorRegisterBlockIdentifier;
use cxl_spec::spec::CXL_COMPONENT_REGISTERS_SIZE_BYTES;
use inspect::Inspect;
use pci_bus::GenericPciBusDevice;
use pci_core::bus_range::AssignedBusRange;
use pci_core::capabilities::extended::PciExtendedCapability;
use pci_core::capabilities::extended::acs::AcsExtendedCapability;
use pci_core::capabilities::msi_cap::MsiCapability;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::msi::MsiTarget;
use pci_core::spec::caps::pci_express::DevicePortType;
use pci_core::spec::caps::pci_express::MaxEndEndTlpPrefixes;
use pci_core::spec::hwid::HardwareIds;
use std::sync::Arc;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

const ACS_CAPABILITY_IMPLEMENTED_MASK: u16 = 0x00df;
const ACS_CAPABILITY_ALLOWED_ROOT_OR_DSP_MASK: u16 = 0x00ff;
const ACS_CAPABILITY_ALLOWED_USP_MASK: u16 = 0x0000;

type CxlComponentRegistersSavedState = <CxlComponentRegisters as SaveRestore>::SavedState;
const CXL_COMPONENT_ALLOWED_ACCESS_SIZES: [usize; 2] = [4, 8];

fn validate_cxl_component_register_access(offset: u64, len: usize) -> Result<(), IoError> {
    if !CXL_COMPONENT_ALLOWED_ACCESS_SIZES.contains(&len) {
        return Err(IoError::InvalidAccessSize);
    }

    if !offset.is_multiple_of(len as u64) {
        return Err(IoError::UnalignedAccess);
    }

    Ok(())
}

/// Express-level settings for a PCIe port.
///
/// Collects feature flags that apply uniformly to a port so that adding new
/// capabilities does not require changing every constructor signature.
#[derive(Debug, Default, Clone)]
pub struct PciePortSettings {
    /// ACS capability bits to advertise on this port. `0` means the ACS
    /// extended capability is not present.
    pub acs_capabilities_supported: u16,

    /// Flex Bus Port capability bits used to advertise CXL support on ports.
    ///
    /// CXL DVSECs are added only when this is `Some` and either `cache_capable`
    /// or `mem_capable` is set.
    pub cxl_flex_bus_port_capability: Option<CxlFlexBusPortDvsecCapability>,

    /// Whether End-to-End TLP prefixing is supported for this port.
    ///
    /// If supported, the max number of TLP prefixes to advertise.
    pub tlp_prefixing_supported: Option<MaxEndEndTlpPrefixes>,
}

/// A description of a generic PCIe port (a root-complex root port or a switch
/// downstream port).
pub struct GenericPciePortDefinition {
    /// The name of the port.
    pub name: Arc<str>,
    /// The device/function (`device << 3 | function`) to place this port at.
    ///
    /// When `None`, the port is assigned the lowest available devfn at or
    /// above the builder's first-port device number. Ports are assigned in
    /// order; an explicit devfn that collides with an already-assigned port
    /// is an error. Honored for both root-complex root ports and switch
    /// downstream ports.
    pub devfn: Option<u8>,
    /// Whether hotplug is enabled for this port.
    pub hotplug: bool,
    /// Express-level port settings (ACS, etc.).
    pub settings: PciePortSettings,
}

/// Generic PCIe port BAR definition.
#[derive(Clone)]
pub struct PortBarDefinition {
    /// BAR index (Type-1 headers currently support BAR0/BAR1 only).
    pub index: u8,
    /// Total BAR size in bytes.
    pub size_bytes: u64,
    /// BAR subregions used to dispatch MMIO accesses.
    pub subregions: Vec<PortBarSubregionDefinition>,
}

/// Generic PCIe port BAR subregion definition.
#[derive(Clone)]
pub struct PortBarSubregionDefinition {
    /// Subregion semantic kind.
    pub kind: PortBarSubregionKind,
    /// Subregion offset within BAR aperture.
    pub offset: u64,
    /// Subregion length in bytes.
    pub size_bytes: u64,
}

/// Generic PCIe port BAR subregion kind.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PortBarSubregionKind {
    /// CXL component register space.
    CxlComponentRegisters,
    /// MSI-X table subregion.
    MsiXTable,
    /// MSI-X pending bit array subregion.
    MsiXPba,
}

pub(crate) fn build_cxl_register_locator_extended_capability(
    register_bir: CxlRegisterLocatorRegisterBir,
    register_block_offset: u64,
) -> Option<Box<dyn PciExtendedCapability>> {
    // Build the single-block CXL Register Locator DVSEC and drop invalid layouts.
    let locator = CxlRegisterLocatorDvsecExtendedCapability::new()
        .with_register_block(
            register_bir,
            CxlRegisterLocatorRegisterBlockIdentifier::COMPONENT_REGISTERS,
            register_block_offset,
        )
        .ok()?;

    Some(Box::new(locator))
}

/// Maps a Type-1 BAR index to the corresponding CXL Register Locator BIR enum.
///
/// Returns `None` for unsupported BAR numbers.
fn register_bir_from_bar_index(index: u8) -> Option<CxlRegisterLocatorRegisterBir> {
    match index {
        0 => Some(CxlRegisterLocatorRegisterBir::BAR_10H),
        1 => Some(CxlRegisterLocatorRegisterBir::BAR_14H),
        2 => Some(CxlRegisterLocatorRegisterBir::BAR_18H),
        3 => Some(CxlRegisterLocatorRegisterBir::BAR_1CH),
        4 => Some(CxlRegisterLocatorRegisterBir::BAR_20H),
        5 => Some(CxlRegisterLocatorRegisterBir::BAR_24H),
        _ => None,
    }
}

/// Extracts CXL Register Locator settings from the configured BAR layout.
///
/// The locator is only emitted when a CXL component-register subregion exists and
/// the BAR index can be represented in CXL BIR encoding.
fn cxl_register_locator_from_bar(
    bar: Option<&PortBarDefinition>,
) -> Option<(CxlRegisterLocatorRegisterBir, u64)> {
    let bar_cfg = bar?;
    let component_subregion = bar_cfg
        .subregions
        .iter()
        .find(|region| region.kind == PortBarSubregionKind::CxlComponentRegisters)?;

    let Some(register_bir) = register_bir_from_bar_index(bar_cfg.index) else {
        tracelimit::warn_ratelimited!(
            bar_index = bar_cfg.index,
            "unsupported BAR index for CXL Register Locator"
        );
        return None;
    };

    Some((register_bir, component_subregion.offset))
}

fn has_cxl_component_register_subregion(bar: Option<&PortBarDefinition>) -> bool {
    bar.is_some_and(|bar_cfg| {
        bar_cfg
            .subregions
            .iter()
            .any(|region| region.kind == PortBarSubregionKind::CxlComponentRegisters)
    })
}

fn drop_cxl_component_register_subregions(bar: &mut Option<PortBarDefinition>) {
    if let Some(bar_cfg) = bar {
        bar_cfg
            .subregions
            .retain(|region| region.kind != PortBarSubregionKind::CxlComponentRegisters);

        if bar_cfg.subregions.is_empty() {
            *bar = None;
        }
    }
}

fn default_bar_from_settings(settings: &PciePortSettings) -> Option<PortBarDefinition> {
    let cxl_requested = settings
        .cxl_flex_bus_port_capability
        .is_some_and(|capability| capability.cache_capable() || capability.mem_capable());

    cxl_requested.then_some(PortBarDefinition {
        index: 0,
        size_bytes: CXL_COMPONENT_REGISTERS_SIZE_BYTES,
        subregions: vec![PortBarSubregionDefinition {
            kind: PortBarSubregionKind::CxlComponentRegisters,
            offset: 0,
            size_bytes: CXL_COMPONENT_REGISTERS_SIZE_BYTES,
        }],
    })
}

/// Filters requested ACS bits by both implementation support and port-type policy.
pub(crate) fn filter_acs_capabilities_for_bridge(
    port_type: &DevicePortType,
    requested: u16,
) -> u16 {
    let type_mask = match port_type {
        DevicePortType::RootPort | DevicePortType::DownstreamSwitchPort => {
            ACS_CAPABILITY_ALLOWED_ROOT_OR_DSP_MASK
        }
        DevicePortType::UpstreamSwitchPort => ACS_CAPABILITY_ALLOWED_USP_MASK,
        DevicePortType::Endpoint => 0,
    };

    requested & ACS_CAPABILITY_IMPLEMENTED_MASK & type_mask
}

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

    /// Optional BAR layout for this port.
    #[inspect(skip)]
    bar: Option<PortBarDefinition>,

    /// Optional CXL component-register backing for CXL BAR subregion emulation.
    #[inspect(skip)]
    cxl_component_registers: Option<CxlComponentRegisters>,
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
    /// * `settings` - Express-level port settings (ACS, etc.)
    pub fn new(
        name: impl Into<String>,
        hardware_ids: HardwareIds,
        port_type: DevicePortType,
        multi_function: bool,
        hotplug_slot_number: Option<u32>,
        msi_target: &MsiTarget,
        settings: PciePortSettings,
        register_mmio: Option<&mut dyn RegisterMmioIntercept>,
        mut bar: Option<PortBarDefinition>,
    ) -> Self {
        let port_name = name.into();
        let mut bars = DeviceBars::new();
        let mut cxl_component_registers = None;
        let mut cxl_locator_capability = None;
        if bar.is_none() {
            bar = default_bar_from_settings(&settings);
        }

        // CXL DVSECs are exposed only when the port advertises cache/mem capability.
        let mut cxl_enabled = settings
            .cxl_flex_bus_port_capability
            .is_some_and(|capability| capability.cache_capable() || capability.mem_capable());

        let requested_cxl_component_subregion = has_cxl_component_register_subregion(bar.as_ref());

        if cxl_enabled && requested_cxl_component_subregion {
            if let Some((register_bir, register_block_offset)) =
                cxl_register_locator_from_bar(bar.as_ref())
            {
                if let Some(locator_capability) = build_cxl_register_locator_extended_capability(
                    register_bir,
                    register_block_offset,
                ) {
                    cxl_locator_capability = Some(locator_capability);
                    cxl_component_registers = Some(CxlComponentRegisters::new());
                } else {
                    tracelimit::warn_ratelimited!(
                        offset = register_block_offset,
                        "invalid CXL Register Locator settings; disabling CXL BAR subregion"
                    );
                    drop_cxl_component_register_subregions(&mut bar);
                    cxl_enabled = false;
                }
            } else {
                tracelimit::warn_ratelimited!(
                    "invalid CXL Register Locator BAR configuration; disabling CXL BAR subregion"
                );
                drop_cxl_component_register_subregions(&mut bar);
                cxl_enabled = false;
            }
        }

        // Materialize BAR intercept plumbing only when the caller provides BAR metadata.
        if let Some(bar_cfg) = &bar {
            if bar_cfg.index != 0 {
                tracelimit::warn_ratelimited!(
                    bar_index = bar_cfg.index,
                    "ignoring unsupported BAR index; only BAR0 is supported"
                );
                bar = None;
            } else if let Some(register_mmio) = register_mmio {
                let region_name = format!("{}-bar{}", port_name, bar_cfg.index);
                bars = bars.bar0(
                    bar_cfg.size_bytes,
                    BarMemoryKind::Intercept(
                        register_mmio.new_io_region(&region_name, bar_cfg.size_bytes),
                    ),
                );
            } else {
                tracelimit::warn_ratelimited!(
                    "ignoring BAR configuration because MMIO register context is missing"
                );
                bar = None;
            }
        }

        // If CXL component-register emulation was requested but BAR MMIO interception could
        // not be materialized, disable CXL DVSECs so config space matches emulation behavior.
        if cxl_enabled && requested_cxl_component_subregion && bar.is_none() {
            tracelimit::warn_ratelimited!(
                "dropping CXL DVSECs because BAR MMIO interception is unavailable"
            );
            cxl_enabled = false;
            cxl_locator_capability = None;
            cxl_component_registers = None;
        }

        let (hotplug, slot_number) = match hotplug_slot_number {
            Some(slot) => (true, Some(slot)),
            None => (false, None),
        };

        let msi_capability = MsiCapability::new(0, true, false, msi_target);
        let acs_supported =
            filter_acs_capabilities_for_bridge(&port_type, settings.acs_capabilities_supported);

        let mut pcie_cap = PciExpressCapability::new(port_type, None);
        if hotplug {
            let slot_num = slot_number.unwrap_or(0);
            pcie_cap = pcie_cap.with_hotplug_support(slot_num);
        }

        if let Some(max_prefixes) = settings.tlp_prefixing_supported {
            pcie_cap = pcie_cap.with_tlp_prefixing_supported(max_prefixes);
        }

        let extended_capabilities = if acs_supported != 0 {
            vec![
                Box::new(AcsExtendedCapability::with_capabilities(acs_supported))
                    as Box<dyn PciExtendedCapability>,
            ]
        } else {
            vec![]
        };

        let mut extended_capabilities = extended_capabilities;

        if cxl_enabled {
            // CXL Spec mandates that a CXL root port or downstream switch port must have CXL Port DVSEC
            // and CXL Flex Bus Port DVSEC.
            extended_capabilities.push(Box::new(CxlPortDvsecExtendedCapability::new()));
            let mut flex_bus_dvsec = CxlFlexBusPortDvsecExtendedCapability::new();
            if let Some(capability) = settings.cxl_flex_bus_port_capability {
                flex_bus_dvsec = flex_bus_dvsec
                    .with_cache_capable(capability.cache_capable())
                    .with_mem_capable(capability.mem_capable())
                    .with_optional_capabilities(
                        capability.cache_capable(),
                        capability.cxl_68b_flit_and_vh_capable(),
                        capability.cxl_multi_logical_device_capable(),
                        capability.cxl_latency_optimized_256b_flit_capable(),
                        capability.cxl_pbr_flit_capable(),
                    );
            }
            extended_capabilities.push(Box::new(flex_bus_dvsec));

            if let Some(locator_capability) = cxl_locator_capability {
                extended_capabilities.push(locator_capability);
            }
        }

        let cfg_space = ConfigSpaceType1Emulator::new_with_bars(
            hardware_ids,
            vec![Box::new(pcie_cap), Box::new(msi_capability)],
            extended_capabilities,
            bars,
        )
        .with_multi_function_bit(multi_function);

        Self {
            name: port_name,
            cfg_space,
            link: None,
            bar,
            cxl_component_registers,
        }
    }

    /// Resolves a guest physical address to a BAR index + BAR-relative offset.
    pub(crate) fn find_bar(&self, addr: u64) -> Option<(u8, u64)> {
        self.cfg_space.find_bar(addr)
    }

    /// Saves optional per-port CXL component-register state.
    ///
    /// Ports without CXL component-register backing return `None`.
    pub(crate) fn save_cxl_component_registers_state(
        &mut self,
    ) -> Result<Option<CxlComponentRegistersSavedState>, SaveError> {
        self.cxl_component_registers
            .as_mut()
            .map(|regs| regs.save())
            .transpose()
    }

    /// Restores optional per-port CXL component-register state.
    ///
    /// State presence must match the current port topology, otherwise restore fails
    /// with an invalid-saved-state error.
    pub(crate) fn restore_cxl_component_registers_state(
        &mut self,
        state: Option<CxlComponentRegistersSavedState>,
    ) -> Result<(), RestoreError> {
        match (&mut self.cxl_component_registers, state) {
            (Some(current), Some(saved)) => current.restore(saved)?,
            (None, None) => {}
            (Some(_), None) => {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "missing CXL component-register state"
                )));
            }
            (None, Some(_)) => {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "unexpected CXL component-register state"
                )));
            }
        }

        Ok(())
    }

    /// Handles BAR reads for this port using subregion semantics.
    ///
    /// CXL component-register accesses are redirected into `CxlComponentRegisters`;
    /// unsupported or out-of-window accesses are handled as reserved reads.
    pub(crate) fn bar_mmio_read(&mut self, bar: u8, bar_offset: u64, data: &mut [u8]) -> IoResult {
        if bar != 0 {
            tracelimit::warn_ratelimited!(bar, "unsupported port BAR read");
            data.fill(0xff);
            return IoResult::Ok;
        }

        let Some(bar_cfg) = &self.bar else {
            tracelimit::warn_ratelimited!(bar, "BAR read without BAR configuration");
            data.fill(0xff);
            return IoResult::Ok;
        };

        let access_end = match bar_offset.checked_add(data.len() as u64) {
            Some(end) => end,
            None => {
                data.fill(0xff);
                return IoResult::Ok;
            }
        };

        let Some(subregion) = bar_cfg.subregions.iter().find(|subregion| {
            let Some(subregion_end) = subregion.offset.checked_add(subregion.size_bytes) else {
                return false;
            };

            bar_offset >= subregion.offset && access_end <= subregion_end
        }) else {
            tracelimit::warn_ratelimited!(
                offset = bar_offset,
                size = data.len(),
                "BAR read outside configured subregions"
            );
            data.fill(0);
            return IoResult::Ok;
        };

        match subregion.kind {
            PortBarSubregionKind::CxlComponentRegisters => {
                let Some(component_registers) = self.cxl_component_registers.as_ref() else {
                    tracelimit::warn_ratelimited!(
                        "CXL component register BAR read without component-register backing"
                    );
                    data.fill(0);
                    return IoResult::Ok;
                };

                let Some(relative_offset) = bar_offset.checked_sub(subregion.offset) else {
                    data.fill(0);
                    return IoResult::Ok;
                };

                if let Err(err) =
                    validate_cxl_component_register_access(relative_offset, data.len())
                {
                    return IoResult::Err(err);
                }

                let Ok(relative_offset) = u16::try_from(relative_offset) else {
                    data.fill(0);
                    return IoResult::Ok;
                };

                match component_registers.read(relative_offset, data) {
                    IoResult::Err(IoError::InvalidRegister) => {
                        data.fill(0);
                        IoResult::Ok
                    }
                    res => res,
                }
            }
            PortBarSubregionKind::MsiXTable | PortBarSubregionKind::MsiXPba => {
                // MSI-X BAR subregions are not emulated by this port yet.
                tracelimit::warn_ratelimited!(
                    "read BAR subregion of kind {:?}, offset 0x{:x}, size 0x{:x}",
                    subregion.kind,
                    subregion.offset,
                    subregion.size_bytes
                );
                data.fill(0);
                IoResult::Ok
            }
        }
    }

    /// Handles BAR writes for this port using subregion semantics.
    ///
    /// CXL component-register accesses are redirected into `CxlComponentRegisters`;
    /// unsupported or out-of-window accesses are treated as handled writes.
    pub(crate) fn bar_mmio_write(&mut self, bar: u8, bar_offset: u64, data: &[u8]) -> IoResult {
        if bar != 0 {
            tracelimit::warn_ratelimited!(bar, "unsupported port BAR write");
            return IoResult::Ok;
        }

        let Some(bar_cfg) = &self.bar else {
            tracelimit::warn_ratelimited!(bar, "BAR write without BAR configuration");
            return IoResult::Ok;
        };

        let access_end = match bar_offset.checked_add(data.len() as u64) {
            Some(end) => end,
            None => {
                return IoResult::Ok;
            }
        };

        let Some(subregion) = bar_cfg.subregions.iter().find(|subregion| {
            let Some(subregion_end) = subregion.offset.checked_add(subregion.size_bytes) else {
                return false;
            };

            bar_offset >= subregion.offset && access_end <= subregion_end
        }) else {
            tracelimit::warn_ratelimited!(
                offset = bar_offset,
                size = data.len(),
                "BAR write outside configured subregions"
            );
            return IoResult::Ok;
        };

        match subregion.kind {
            PortBarSubregionKind::CxlComponentRegisters => {
                let Some(component_registers) = self.cxl_component_registers.as_mut() else {
                    tracelimit::warn_ratelimited!(
                        "CXL component register BAR write without component-register backing"
                    );
                    return IoResult::Ok;
                };

                let Some(relative_offset) = bar_offset.checked_sub(subregion.offset) else {
                    return IoResult::Ok;
                };

                if let Err(err) =
                    validate_cxl_component_register_access(relative_offset, data.len())
                {
                    return IoResult::Err(err);
                }

                let Ok(relative_offset) = u16::try_from(relative_offset) else {
                    return IoResult::Ok;
                };

                match component_registers.write(relative_offset, data) {
                    IoResult::Err(IoError::InvalidRegister) => IoResult::Ok,
                    res => res,
                }
            }
            PortBarSubregionKind::MsiXTable | PortBarSubregionKind::MsiXPba => {
                // MSI-X BAR subregions are not emulated by this port yet.
                tracelimit::warn_ratelimited!(
                    "write BAR subregion of kind {:?}, offset 0x{:x}, size 0x{:x}",
                    subregion.kind,
                    subregion.offset,
                    subregion.size_bytes
                );
                IoResult::Ok
            }
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
        addr: PciConfigAddress,
        mut value: ByteEnabledDwordRead<'_>,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration,
        // and the access should not have been routed to this port.
        if bus_range == (0..=0) {
            tracelimit::warn_ratelimited!("invalid access: port bus number range not configured");
            value.set(!0);
            return IoResult::Ok;
        }

        // If the bus number is not within the port's bus range, the access should not have been
        // routed to this port.
        if !bus_range.contains(&addr.bus) {
            tracelimit::warn_ratelimited!(
                "bus number to access not within port's bus number range"
            );
            value.set(!0);
            return IoResult::Ok;
        }

        // Read whether ARI Forwarding is enabled on this port before borrowing the
        // connected device, to avoid overlapping borrows of `self`.
        let ari_forwarding_enabled = self
            .cfg_space
            .capabilities()
            .iter()
            .find_map(|cap| cap.as_pci_express())
            .is_some_and(|pcie| pcie.ari_forwarding_enable());

        // If there is no device connected to the port, then we should return all-1s.
        let Some((_, device)) = &mut self.link else {
            tracing::trace!("no device connected to port");
            value.set(!0);
            return IoResult::Ok;
        };

        // Determine how to route the request. If the target bus is this port's secondary
        // bus, the Type 1 access must be turned into a Type 0 access to the connected
        // device. Per PCIe Base Spec 7.0 §7.3.3, a downstream port only decodes Device
        // Number 0, so a request to a non-zero device number is an Unsupported Request --
        // unless ARI Forwarding is enabled (§6.13), in which case the full 8-bit devfn
        // carries an ARI function number and must be forwarded. Without this gate, an
        // ARI endpoint's functions numbered > 7 are aliased under phantom device numbers.
        let new_access_type = if addr.bus == *bus_range.start() {
            if addr.device() == 0 || ari_forwarding_enabled {
                PciConfigAccessType::Type0
            } else {
                tracing::trace!(
                    device = addr.device(),
                    "config read to non-zero device below downstream port without ARI forwarding; unsupported request"
                );
                value.set(!0);
                return IoResult::Ok;
            }
        } else {
            PciConfigAccessType::Type1
        };

        device
            .pci_cfg_read_with_routing(new_access_type, addr, value.reborrow())
            .unwrap_or_else(|| {
                tracelimit::warn_ratelimited!("failed to read from connected device");
                value.set(!0);
                IoResult::Ok
            })
    }

    /// Forward a configuration space write to the connected device.
    /// Supports routing components for multi-level hierarchies.
    pub fn forward_cfg_write_with_routing(
        &mut self,
        addr: PciConfigAddress,
        value: ByteEnabledDwordWrite,
    ) -> IoResult {
        let bus_range = self.cfg_space.assigned_bus_range();

        // If the bus range is 0..=0, this indicates invalid/uninitialized bus configuration,
        // and the access should not have been routed to this port.
        if bus_range == (0..=0) {
            tracelimit::warn_ratelimited!("invalid access: port bus number range not configured");
            return IoResult::Ok;
        }

        // If the bus number is not within the port's bus range, the access should not have been
        // routed to this port.
        if !bus_range.contains(&addr.bus) {
            tracelimit::warn_ratelimited!(
                "bus number to access not within port's bus number range"
            );
            return IoResult::Ok;
        }

        // Read whether ARI Forwarding is enabled on this port before borrowing the
        // connected device, to avoid overlapping borrows of `self`.
        let ari_forwarding_enabled = self
            .cfg_space
            .capabilities()
            .iter()
            .find_map(|cap| cap.as_pci_express())
            .is_some_and(|pcie| pcie.ari_forwarding_enable());

        // If there is no device connected to the port, then we should just drop the access.
        let Some((_, device)) = &mut self.link else {
            tracelimit::warn_ratelimited!("no device connected to port");
            return IoResult::Ok;
        };

        // Determine how to route the request. If the target bus is this port's secondary
        // bus, the Type 1 access must be turned into a Type 0 access to the connected
        // device. Per PCIe Base Spec 7.0 §7.3.3, a downstream port only decodes Device
        // Number 0, so a request to a non-zero device number is an Unsupported Request --
        // unless ARI Forwarding is enabled (§6.13), in which case the full 8-bit devfn
        // carries an ARI function number and must be forwarded. Without this gate, an
        // ARI endpoint's functions numbered > 7 are aliased under phantom device numbers.
        let new_access_type = if addr.bus == *bus_range.start() {
            if addr.device() == 0 || ari_forwarding_enabled {
                PciConfigAccessType::Type0
            } else {
                tracing::trace!(
                    device = addr.device(),
                    "config write to non-zero device below downstream port without ARI forwarding; unsupported request"
                );
                return IoResult::Ok;
            }
        } else {
            PciConfigAccessType::Type1
        };

        device
            .pci_cfg_write_with_routing(new_access_type, addr, value)
            .unwrap_or_else(|| {
                tracelimit::warn_ratelimited!("failed to write to connected device");
                IoResult::Ok
            })
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
    use crate::test_helpers::TestPcieMmioRegistration;
    use chipset_device::io::IoResult;
    use cxl_spec::pci_registers::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecCapability;
    use parking_lot::Mutex;
    use pci_bus::GenericPciBusDevice;
    use pci_core::spec::hwid::HardwareIds;
    use pci_core::test_helpers::TestCfgAccess;
    use std::sync::Arc;

    fn make_cxl_bar_port() -> PcieDownstreamPort {
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

        let mut mmio = TestPcieMmioRegistration {};
        let msi_target = MsiTarget::disconnected();
        PcieDownstreamPort::new(
            "cxl-bar-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                acs_capabilities_supported: 0,
                cxl_flex_bus_port_capability: Some(
                    CxlFlexBusPortDvsecCapability::new().with_mem_capable(true),
                ),
                tlp_prefixing_supported: None,
            },
            Some(&mut mmio),
            Some(PortBarDefinition {
                index: 0,
                size_bytes: 0x1000,
                subregions: vec![PortBarSubregionDefinition {
                    kind: PortBarSubregionKind::CxlComponentRegisters,
                    offset: 0,
                    size_bytes: 0x1000,
                }],
            }),
        )
    }

    // Mock device for testing
    struct MockDevice;

    impl GenericPciBusDevice for MockDevice {
        fn pci_cfg_read(
            &mut self,
            _offset: u16,
            _value: ByteEnabledDwordRead<'_>,
        ) -> Option<IoResult> {
            None
        }

        fn pci_cfg_write(
            &mut self,
            _offset: u16,
            _value: ByteEnabledDwordWrite,
        ) -> Option<IoResult> {
            None
        }
    }

    #[derive(Default, Debug, Clone, PartialEq, Eq)]
    struct RoutingStats {
        direct_reads: usize,
        direct_writes: usize,
        type0_reads: Vec<PciConfigAddress>,
        type0_writes: Vec<PciConfigAddress>,
        type1_reads: Vec<PciConfigAddress>,
        type1_writes: Vec<PciConfigAddress>,
    }

    struct MultiFunctionMockDevice {
        stats: Arc<Mutex<RoutingStats>>,
    }

    impl GenericPciBusDevice for MultiFunctionMockDevice {
        fn pci_cfg_read(
            &mut self,
            _offset: u16,
            _value: ByteEnabledDwordRead<'_>,
        ) -> Option<IoResult> {
            self.stats.lock().direct_reads += 1;
            Some(IoResult::Ok)
        }

        fn pci_cfg_write(
            &mut self,
            _offset: u16,
            _value: ByteEnabledDwordWrite,
        ) -> Option<IoResult> {
            self.stats.lock().direct_writes += 1;
            Some(IoResult::Ok)
        }

        fn pci_cfg_read_with_routing(
            &mut self,
            access_type: PciConfigAccessType,
            address: PciConfigAddress,
            mut value: ByteEnabledDwordRead<'_>,
        ) -> Option<IoResult> {
            let mut stats = self.stats.lock();
            match access_type {
                PciConfigAccessType::Type0 => stats.type0_reads.push(address),
                PciConfigAccessType::Type1 => stats.type1_reads.push(address),
            }
            value.set(0x1234_5678);
            Some(IoResult::Ok)
        }

        fn pci_cfg_write_with_routing(
            &mut self,
            access_type: PciConfigAccessType,
            address: PciConfigAddress,
            _value: ByteEnabledDwordWrite,
        ) -> Option<IoResult> {
            let mut stats = self.stats.lock();
            match access_type {
                PciConfigAccessType::Type0 => stats.type0_writes.push(address),
                PciConfigAccessType::Type1 => stats.type1_writes.push(address),
            }
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
            &msi_conn.target(),
            PciePortSettings::default(),
            None,
            None,
        );

        // Initially, presence detect state should be 0
        let slot_status_val = port.cfg_space.read_u32(0x58); // 0x40 (cap start) + 0x18 (slot control/status)
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
        let slot_status_val = port.cfg_space.read_u32(0x58); // 0x40 (cap start) + 0x18 (slot control/status)
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
            &msi_conn.target(),
            PciePortSettings::default(),
            None,
            None,
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

        let msi_target = MsiTarget::disconnected();
        let mut port = PcieDownstreamPort::new(
            "test-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings::default(),
            None,
            None,
        );

        port.cfg_space.write_u32(0x18, (1u32 << 16) | (1u32 << 8));

        let stats = Arc::new(Mutex::new(RoutingStats::default()));
        port.link = Some((
            "mf-device".into(),
            Box::new(MultiFunctionMockDevice {
                stats: Arc::clone(&stats),
            }),
        ));

        let mut value = 0;
        // All accesses on the secondary bus go through the linked device's
        // Type 0 routing hook, which is responsible for dispatching function 0
        // to its own config space.
        assert!(matches!(
            port.forward_cfg_read_with_routing(
                PciConfigAddress::new(1, 0, 0x10 / 4).unwrap(),
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            IoResult::Ok
        ));
        assert!(matches!(
            port.forward_cfg_read_with_routing(
                PciConfigAddress::new(1, 3, 0x14 / 4).unwrap(),
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            IoResult::Ok
        ));

        let stats = stats.lock().clone();
        assert_eq!(stats.direct_reads, 0);
        assert_eq!(
            stats.type0_reads,
            vec![
                PciConfigAddress::new(1, 0, 0x10 / 4).unwrap(),
                PciConfigAddress::new(1, 3, 0x14 / 4).unwrap()
            ]
        );
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
            &msi_conn.target(),
            PciePortSettings::default(),
            None,
            None,
        );

        port.cfg_space.write_u32(0x18, (1u32 << 16) | (1u32 << 8));

        let stats = Arc::new(Mutex::new(RoutingStats::default()));
        port.link = Some((
            "mf-device".into(),
            Box::new(MultiFunctionMockDevice {
                stats: Arc::clone(&stats),
            }),
        ));

        // All accesses on the secondary bus go through the linked device's
        // Type 0 routing hook, which is responsible for dispatching function 0
        // to its own config space.
        assert!(matches!(
            port.forward_cfg_write_with_routing(
                PciConfigAddress::new(1, 0, 0x10 / 4).unwrap(),
                ByteEnabledDwordWrite::with_all_bytes_enabled(0xAAAA_0000)
            ),
            IoResult::Ok
        ));
        assert!(matches!(
            port.forward_cfg_write_with_routing(
                PciConfigAddress::new(1, 2, 0x14 / 4).unwrap(),
                ByteEnabledDwordWrite::with_all_bytes_enabled(0xBBBB_0000)
            ),
            IoResult::Ok
        ));

        let stats = stats.lock().clone();
        assert_eq!(stats.direct_writes, 0);
        assert_eq!(
            stats.type0_writes,
            vec![
                PciConfigAddress::new(1, 0, 0x10 / 4).unwrap(),
                PciConfigAddress::new(1, 2, 0x14 / 4).unwrap()
            ]
        );
    }

    /// Builds a root port with secondary bus 1 and a linked mock device, returning
    /// the port and a handle to the device's routing stats.
    fn make_root_port_with_linked_device() -> (PcieDownstreamPort, Arc<Mutex<RoutingStats>>) {
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

        let msi_target = MsiTarget::disconnected();
        let mut port = PcieDownstreamPort::new(
            "test-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings::default(),
            None,
            None,
        );

        // Set the secondary and subordinate bus numbers to 1, giving the port an
        // assigned (secondary) bus range of 1..=1. Primary bus number stays 0.
        port.cfg_space.write_u32(0x18, (1u32 << 16) | (1u32 << 8));

        let stats = Arc::new(Mutex::new(RoutingStats::default()));
        port.link = Some((
            "mf-device".into(),
            Box::new(MultiFunctionMockDevice {
                stats: Arc::clone(&stats),
            }),
        ));

        (port, stats)
    }

    #[test]
    fn test_secondary_bus_nonzero_device_without_ari_is_unsupported_request() {
        let (mut port, stats) = make_root_port_with_linked_device();

        // Device 0 (devfn 0x00) on the secondary bus converts to Type 0 and forwards.
        let mut value = 0;
        assert!(matches!(
            port.forward_cfg_read_with_routing(
                PciConfigAddress::new(1, 0x00, 0x10 / 4).unwrap(),
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            IoResult::Ok
        ));
        assert_eq!(value, 0x1234_5678);

        // Device 1 (devfn 0x08) without ARI Forwarding is an Unsupported Request:
        // all-ones is returned and the connected device is never reached.
        let mut value = 0;
        assert!(matches!(
            port.forward_cfg_read_with_routing(
                PciConfigAddress::new(1, 0x08, 0x14 / 4).unwrap(),
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            IoResult::Ok
        ));
        assert_eq!(value, !0);

        // A write to the same non-zero device is silently dropped.
        assert!(matches!(
            port.forward_cfg_write_with_routing(
                PciConfigAddress::new(1, 0x08, 0x14 / 4).unwrap(),
                ByteEnabledDwordWrite::with_all_bytes_enabled(0xDEAD_BEEF)
            ),
            IoResult::Ok
        ));

        let stats = stats.lock().clone();
        // Only the device-0 read reached the device; the non-zero device accesses did not.
        assert_eq!(
            stats.type0_reads,
            vec![PciConfigAddress::new(1, 0x00, 0x10 / 4).unwrap()]
        );
        assert!(stats.type0_writes.is_empty());
        assert!(stats.type1_reads.is_empty());
        assert!(stats.type1_writes.is_empty());
    }

    #[test]
    fn test_secondary_bus_nonzero_device_with_ari_forwards_full_devfn() {
        let (mut port, stats) = make_root_port_with_linked_device();

        // Enable ARI Forwarding in Device Control 2 (PCIe cap base 0x40 + 0x28),
        // bit 5 (0x20).
        port.cfg_space.write_u32(0x68, 0x0000_0020);
        assert_eq!(
            port.cfg_space.read_u32(0x68) & 0x20,
            0x20,
            "ARI Forwarding Enable should be set"
        );

        // ARI function 8 arrives as devfn 0x08 (device bits non-zero) and must be
        // forwarded as a Type 0 access carrying the full 8-bit function number.
        let mut value = 0;
        assert!(matches!(
            port.forward_cfg_read_with_routing(
                PciConfigAddress::new(1, 0x08, 0x10 / 4).unwrap(),
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            IoResult::Ok
        ));
        assert_eq!(value, 0x1234_5678);

        assert!(matches!(
            port.forward_cfg_write_with_routing(
                PciConfigAddress::new(1, 0x30, 0x14 / 4).unwrap(),
                ByteEnabledDwordWrite::with_all_bytes_enabled(0xCAFE_0000)
            ),
            IoResult::Ok
        ));

        let stats = stats.lock().clone();
        assert_eq!(
            stats.type0_reads,
            vec![PciConfigAddress::new(1, 0x08, 0x10 / 4).unwrap()]
        );
        assert_eq!(
            stats.type0_writes,
            vec![PciConfigAddress::new(1, 0x30, 0x14 / 4).unwrap()]
        );
        assert!(stats.type1_reads.is_empty());
        assert!(stats.type1_writes.is_empty());
    }

    #[test]
    fn test_port_cfg_space_save_restore() {
        use pci_core::spec::hwid::{ClassCode, ProgrammingInterface, Subclass};
        use vmcore::save_restore::SaveRestore;

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
            &msi_conn.target(),
            PciePortSettings::default(),
            None,
            None,
        );

        // Program bridge bus numbers (Type1 register at offset 0x18).
        port.cfg_space.write_u32(0x18, 0x0012_1000);
        assert_eq!(port.cfg_space.assigned_bus_range(), 0x10..=0x12);

        let saved = port.cfg_space.save().expect("save should succeed");

        // Change state away from saved values.
        port.cfg_space.write_u32(0x18, 0x0000_0000);
        assert_eq!(port.cfg_space.assigned_bus_range(), 0..=0);

        port.cfg_space
            .restore(saved)
            .expect("restore should succeed");
        assert_eq!(port.cfg_space.assigned_bus_range(), 0x10..=0x12);
    }

    #[test]
    fn test_filter_acs_capabilities_for_bridge_type() {
        assert_eq!(
            filter_acs_capabilities_for_bridge(&DevicePortType::RootPort, 0x00ff),
            0x00df
        );
        assert_eq!(
            filter_acs_capabilities_for_bridge(&DevicePortType::DownstreamSwitchPort, 0x00ff),
            0x00df
        );
        assert_eq!(
            filter_acs_capabilities_for_bridge(&DevicePortType::UpstreamSwitchPort, 0x00ff),
            0
        );
        assert_eq!(
            filter_acs_capabilities_for_bridge(&DevicePortType::Endpoint, 0x00ff),
            0
        );
    }

    #[test]
    fn test_root_port_adds_acs_only_when_non_zero() {
        use pci_core::spec::caps::ExtendedCapabilityId;
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

        let msi_target = MsiTarget::disconnected();
        let with_acs = PcieDownstreamPort::new(
            "with-acs",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                acs_capabilities_supported: 0x005f,
                ..Default::default()
            },
            None,
            None,
        );
        let value = with_acs.cfg_space.read_u32(0x100);
        assert_eq!(value & 0xffff, ExtendedCapabilityId::ACS.0 as u32);

        let without_acs = PcieDownstreamPort::new(
            "without-acs",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings::default(),
            None,
            None,
        );
        let value = without_acs.cfg_space.read_u32(0x100);
        assert_eq!(value, 0);
    }

    #[test]
    fn test_invalid_cxl_component_register_locator_disables_cxl_exposure() {
        use cxl_spec::pci_registers::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecCapability;
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

        let msi_target = MsiTarget::disconnected();
        let port = PcieDownstreamPort::new(
            "test-port",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                acs_capabilities_supported: 0,
                cxl_flex_bus_port_capability: Some(
                    CxlFlexBusPortDvsecCapability::new().with_mem_capable(true),
                ),
                tlp_prefixing_supported: None,
            },
            None,
            Some(PortBarDefinition {
                index: 0,
                size_bytes: 0x1000,
                subregions: vec![PortBarSubregionDefinition {
                    kind: PortBarSubregionKind::CxlComponentRegisters,
                    offset: 0,
                    size_bytes: 0x1000,
                }],
            }),
        );

        let value = port.cfg_space.read_u32(0x100);
        assert_eq!(
            value, 0,
            "CXL DVSECs should be absent when CXL component-register BAR backing is invalid"
        );
        assert!(
            port.cxl_component_registers.is_none(),
            "component-register backing should not be allocated"
        );
    }

    #[test]
    fn test_cxl_component_register_bar_rejects_1_or_2_byte_reads() {
        let mut port = make_cxl_bar_port();

        let mut read1 = [0u8; 1];
        assert!(matches!(
            port.bar_mmio_read(0, 0, &mut read1),
            IoResult::Err(IoError::InvalidAccessSize)
        ));

        let mut read2 = [0u8; 2];
        assert!(matches!(
            port.bar_mmio_read(0, 0, &mut read2),
            IoResult::Err(IoError::InvalidAccessSize)
        ));
    }

    #[test]
    fn test_cxl_component_register_bar_rejects_1_or_2_byte_writes() {
        let mut port = make_cxl_bar_port();

        let write1 = [0u8; 1];
        assert!(matches!(
            port.bar_mmio_write(0, 0, &write1),
            IoResult::Err(IoError::InvalidAccessSize)
        ));

        let write2 = [0u8; 2];
        assert!(matches!(
            port.bar_mmio_write(0, 0, &write2),
            IoResult::Err(IoError::InvalidAccessSize)
        ));
    }
}
