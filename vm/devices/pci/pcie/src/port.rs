// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common PCIe port implementation shared between different port types.

use anyhow::bail;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciAerErrorKind;
use chipset_device::pci::PciAerInjection;
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
use pci_core::capabilities::extended::aer::AerCapabilityConfig;
use pci_core::capabilities::extended::aer::AerExtendedCapability;
use pci_core::capabilities::extended::aer::AerInjectedErrorKind;
use pci_core::capabilities::extended::aer::AerInjection;
use pci_core::capabilities::extended::dpc::DpcCapabilityConfig;
use pci_core::capabilities::extended::dpc::DpcExtendedCapability;
use pci_core::capabilities::msi_cap::MsiCapability;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::msi::MsiTarget;
use pci_core::spec::caps::pci_express::DevicePortType;
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

    /// Optional AER configuration. `None` means the AER extended capability
    /// is not present on this port.
    pub aer: Option<PcieAerSettings>,

    /// Optional DPC configuration. `None` means the DPC extended capability
    /// is not present on this port.
    pub dpc: Option<PcieDpcSettings>,

    /// Flex Bus Port capability bits used to advertise CXL support on ports.
    ///
    /// CXL DVSECs are added only when this is `Some` and either `cache_capable`
    /// or `mem_capable` is set.
    pub cxl_flex_bus_port_capability: Option<CxlFlexBusPortDvsecCapability>,
}

/// AER settings for a PCIe port.
#[derive(Debug, Default, Clone, Copy)]
pub struct PcieAerSettings {
    /// Default value for the AER Correctable Error Mask register.
    pub correctable_mask: Option<u32>,
    /// Default value for the AER Uncorrectable Error Mask register.
    pub uncorrectable_mask: Option<u32>,
    /// Default value for the AER Uncorrectable Error Severity register.
    pub uncorrectable_severity_mask: Option<u32>,
}

/// DPC settings for a PCIe port.
#[derive(Debug, Clone, Copy)]
pub struct PcieDpcSettings {
    /// Whether software-triggering support is advertised.
    pub software_trigger_supported: bool,
    /// Whether poisoned TLP egress blocking support is advertised.
    pub poisoned_tlp_egress_blocking_supported: bool,
    /// Whether DL_Active ERR_COR signaling support is advertised.
    pub dl_active_err_cor_signaling_supported: bool,
}

impl Default for PcieDpcSettings {
    fn default() -> Self {
        Self {
            software_trigger_supported: true,
            poisoned_tlp_egress_blocking_supported: false,
            dl_active_err_cor_signaling_supported: false,
        }
    }
}

/// Runtime AER injection request for a downstream-facing port.
#[derive(Debug, Clone, Copy)]
pub struct PcieAerInjectRequest {
    /// Error kind.
    pub kind: PciAerErrorKind,
    /// Status bits to OR into the corresponding AER status register.
    pub status_bits: u32,
    /// Header log DWORDs.
    pub header_log: [u32; 4],
    /// Source Requester ID (Bus<<8 | DevFn).
    pub source_id: u16,
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

#[derive(Copy, Clone)]
enum PortInterruptKind {
    Hotplug,
    Aer,
    Dpc,
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
        let msi_interrupt_supported = msi_capability.interrupt().is_some();
        let aer_root_interrupt_supported =
            msi_interrupt_supported && matches!(&port_type, DevicePortType::RootPort);
        let pcie_interrupt_message_number =
            msi_interrupt_supported.then(|| msi_capability.pcie_interrupt_message_number());
        let aer_interrupt_message_number =
            aer_root_interrupt_supported.then(|| msi_capability.aer_interrupt_message_number());
        let dpc_interrupt_message_number =
            msi_interrupt_supported.then(|| msi_capability.dpc_interrupt_message_number());
        let acs_supported =
            filter_acs_capabilities_for_bridge(&port_type, settings.acs_capabilities_supported);

        let mut extended_capabilities: Vec<Box<dyn PciExtendedCapability>> = Vec::new();

        if acs_supported != 0 {
            extended_capabilities.push(Box::new(AcsExtendedCapability::with_capabilities(
                acs_supported,
            )));
        }

        if let Some(aer_settings) = settings.aer {
            extended_capabilities.push(Box::new(AerExtendedCapability::with_config(
                &port_type,
                AerCapabilityConfig {
                    correctable_mask: aer_settings.correctable_mask,
                    uncorrectable_mask: aer_settings.uncorrectable_mask,
                    uncorrectable_severity_mask: aer_settings.uncorrectable_severity_mask,
                    root_error_interrupt_message_number: aer_interrupt_message_number,
                },
            )));
        }

        if let Some(dpc_settings) = settings.dpc {
            if matches!(
                &port_type,
                DevicePortType::RootPort | DevicePortType::DownstreamSwitchPort
            ) {
                let rp_extensions_for_dpc = matches!(&port_type, DevicePortType::RootPort);
                extended_capabilities.push(Box::new(DpcExtendedCapability::with_config(
                    &port_type,
                    DpcCapabilityConfig {
                        dpc_interrupt_message_number,
                        rp_extensions_for_dpc,
                        poisoned_tlp_egress_blocking_supported: dpc_settings
                            .poisoned_tlp_egress_blocking_supported,
                        dpc_software_triggering_supported: dpc_settings.software_trigger_supported,
                        dl_active_err_cor_signaling_supported: dpc_settings
                            .dl_active_err_cor_signaling_supported,
                        rp_pio_log_size_dw: if rp_extensions_for_dpc { 4 } else { 0 },
                    },
                )));
            } else {
                tracelimit::warn_ratelimited!(
                    "DPC capability is only supported for Root Ports and Downstream Switch Ports"
                );
            }
        }

        let pcie_cap = if hotplug {
            let slot_num = slot_number.unwrap_or(0);
            let cap = PciExpressCapability::new(port_type, None);
            let cap = if let Some(interrupt_message_number) = pcie_interrupt_message_number {
                cap.with_interrupt_message_number(interrupt_message_number.into())
            } else {
                cap
            };

            cap.with_hotplug_support(slot_num)
        } else {
            PciExpressCapability::new(port_type, None)
        };

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

    /// Notify the guest using the selected interrupt source and message number.
    fn fire_port_interrupt(&self, kind: PortInterruptKind) {
        let interrupt_message_number = match kind {
            PortInterruptKind::Hotplug => self
                .cfg_space
                .capabilities()
                .iter()
                .find_map(|cap| cap.as_pci_express())
                .filter(|pcie| pcie.hot_plug_interrupt_enabled())
                .map(|pcie| pcie.interrupt_message_number()),
            PortInterruptKind::Aer => self
                .cfg_space
                .extended_capabilities()
                .iter()
                .find_map(|cap| cap.as_aer())
                .and_then(|aer| aer.interrupt_message_number()),
            PortInterruptKind::Dpc => self
                .cfg_space
                .extended_capabilities()
                .iter()
                .find_map(|cap| cap.as_dpc())
                .map(|dpc| dpc.interrupt_message_number()),
        };

        let Some(interrupt_message_number) = interrupt_message_number else {
            return;
        };

        if let Some(msi) = self
            .cfg_space
            .capabilities()
            .iter()
            .find_map(|cap| cap.as_msi_cap())
        {
            msi.signal_message_number(interrupt_message_number);
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
        if self
            .cfg_space
            .extended_capabilities()
            .iter()
            .find_map(|cap| cap.as_dpc())
            .is_some_and(|dpc| dpc.containment_active())
        {
            *value = !0;
            return IoResult::Ok;
        }

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
        if self
            .cfg_space
            .extended_capabilities()
            .iter()
            .find_map(|cap| cap.as_dpc())
            .is_some_and(|dpc| dpc.containment_active())
        {
            return IoResult::Ok;
        }

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
        self.fire_port_interrupt(PortInterruptKind::Hotplug);
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
        self.fire_port_interrupt(PortInterruptKind::Hotplug);
        Ok(())
    }

    /// Inject an AER event into this port.
    ///
    /// This is only supported for root ports with AER capability present.
    /// The source device update is best-effort through the routed child device.
    pub fn inject_aer(&mut self, request: PcieAerInjectRequest) -> anyhow::Result<()> {
        self.inject_aer_internal(request, true)
    }

    /// Inject DPC phase 1 on this port.
    ///
    /// Supported on ports that expose the DPC capability (root ports and
    /// downstream switch ports). This also injects the corresponding AER event
    /// onto this port and the routed source hierarchy.
    pub fn inject_dpc_begin(&mut self, request: PcieAerInjectRequest) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(request.kind, PciAerErrorKind::Uncorrectable),
            "DPC injection requires an uncorrectable AER event"
        );

        self.inject_aer_internal(request, false)?;

        let mut found_dpc = false;
        let mut should_interrupt = false;
        for ext in self.cfg_space.extended_capabilities_mut().iter_mut() {
            if let Some(dpc) = ext.as_dpc_mut() {
                found_dpc = true;
                let outcome = dpc.trigger_from_uncorrectable_begin(request.source_id);
                should_interrupt = outcome.should_interrupt;
                break;
            }
        }

        anyhow::ensure!(
            found_dpc,
            "DPC injection is only supported for ports with a DPC capability"
        );

        if should_interrupt {
            self.fire_port_interrupt(PortInterruptKind::Dpc);
        }

        Ok(())
    }

    /// Inject DPC phase 2 on this port by clearing RP busy.
    pub fn inject_dpc_complete(&mut self) -> anyhow::Result<()> {
        for ext in self.cfg_space.extended_capabilities_mut().iter_mut() {
            if let Some(dpc) = ext.as_dpc_mut() {
                dpc.clear_rp_busy();
                return Ok(());
            }
        }

        anyhow::bail!("DPC completion is only supported for ports with a DPC capability")
    }

    fn inject_aer_internal(
        &mut self,
        request: PcieAerInjectRequest,
        require_root_outcome: bool,
    ) -> anyhow::Result<()> {
        let mut injected_root = false;
        let mut injected_local = false;
        let mut should_interrupt = false;

        for ext in self.cfg_space.extended_capabilities_mut().iter_mut() {
            if let Some(aer) = ext.as_aer_mut() {
                let injection = AerInjection {
                    kind: match request.kind {
                        PciAerErrorKind::Correctable => AerInjectedErrorKind::Correctable,
                        PciAerErrorKind::Uncorrectable => AerInjectedErrorKind::Uncorrectable,
                    },
                    status_bits: request.status_bits,
                    header_log: request.header_log,
                    source_id: request.source_id,
                };

                if let Some(outcome) = aer.inject(injection) {
                    injected_root = true;
                    should_interrupt = outcome.should_interrupt;
                }
                injected_local = true;
                break;
            }
        }

        if require_root_outcome {
            anyhow::ensure!(
                injected_root,
                "AER injection is only supported for root ports with an AER capability"
            );
        } else {
            anyhow::ensure!(
                injected_local,
                "AER injection requires an AER capability on the target port"
            );
        }

        if let Some((_, device)) = &mut self.link {
            let secondary_bus = *self.cfg_space.assigned_bus_range().start();
            let target_bus = (request.source_id >> 8) as u8;
            let function = (request.source_id & 0xff) as u8;
            let _ = device.pci_inject_aer_with_routing(
                secondary_bus,
                target_bus,
                function,
                PciAerInjection {
                    kind: request.kind,
                    status_bits: request.status_bits,
                    header_log: request.header_log,
                    source_id: request.source_id,
                },
            );
        }

        if should_interrupt {
            self.fire_port_interrupt(PortInterruptKind::Aer);
        }

        Ok(())
    }

    /// Inject AER state into this port's local AER capability.
    ///
    /// This updates status/header/source fields for the source function device
    /// and does not perform root-port-only interrupt handling.
    pub fn inject_local_aer_state(&mut self, request: PciAerInjection) -> bool {
        for ext in self.cfg_space.extended_capabilities_mut().iter_mut() {
            if let Some(aer) = ext.as_aer_mut() {
                let _ = aer.inject(AerInjection {
                    kind: match request.kind {
                        PciAerErrorKind::Correctable => AerInjectedErrorKind::Correctable,
                        PciAerErrorKind::Uncorrectable => AerInjectedErrorKind::Uncorrectable,
                    },
                    status_bits: request.status_bits,
                    header_log: request.header_log,
                    source_id: request.source_id,
                });
                return true;
            }
        }

        false
    }

    /// Route AER state update to a child device reachable through this port.
    ///
    /// This is used to locate the source function and update its local AER
    /// capability while root-port reporting remains handled by `inject_aer`.
    pub fn inject_child_aer(
        &mut self,
        target_bus: u8,
        function: u8,
        request: PciAerInjection,
    ) -> anyhow::Result<bool> {
        let Some((_, device)) = &mut self.link else {
            return Ok(false);
        };

        let secondary_bus = *self.cfg_space.assigned_bus_range().start();
        Ok(device
            .pci_inject_aer_with_routing(secondary_bus, target_bus, function, request)
            .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestAerEndpoint;
    use crate::test_helpers::TestPcieMmioRegistration;
    use crate::test_helpers::find_ext_cap_offset_type1;
    use crate::test_helpers::read_aer_dword;
    use chipset_device::io::IoResult;
    use cxl_spec::pci_registers::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecCapability;
    use parking_lot::Mutex;
    use pci_bus::GenericPciBusDevice;
    use pci_core::spec::caps::ExtendedCapabilityId;
    use pci_core::spec::caps::aer::AerExtendedCapabilityHeader;
    use pci_core::spec::caps::aer::UncorrectableErrorStatus;
    use pci_core::spec::caps::dpc::DpcControl;
    use pci_core::spec::caps::dpc::DpcExtendedCapabilityHeader;
    use pci_core::spec::caps::dpc::DpcStatus;
    use pci_core::spec::hwid::HardwareIds;
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
                aer: None,
                dpc: None,
                cxl_flex_bus_port_capability: Some(
                    CxlFlexBusPortDvsecCapability::new().with_mem_capable(true),
                ),
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
        fn pci_cfg_read(&mut self, _offset: u16, _value: &mut u32) -> Option<IoResult> {
            None
        }

        fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> Option<IoResult> {
            None
        }
    }

    #[derive(Default, Clone)]
    struct RoutingStats {
        direct_reads: u32,
        direct_writes: u32,
        forward_reads: Vec<(u8, u8, u16)>,
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
            &msi_conn.target(),
            PciePortSettings::default(),
            None,
            None,
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
            &msi_conn.target(),
            PciePortSettings::default(),
            None,
            None,
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

    #[test]
    fn test_dpc_containment_blocks_config_routing_below_port() {
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
            PciePortSettings {
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
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

        // Trigger DPC containment using DPC Control (trigger enable + software trigger).
        port.cfg_space
            .write_u32(0x104, (0x1u32 | (1 << 6)) << 16)
            .unwrap();

        let mut value = 0;
        assert!(matches!(
            port.forward_cfg_read_with_routing(&1, &0, 0x10, &mut value),
            IoResult::Ok
        ));
        assert_eq!(value, !0);

        assert!(matches!(
            port.forward_cfg_write_with_routing(&1, &0, 0x10, 0xAAAA_0000),
            IoResult::Ok
        ));

        let stats = stats.lock().clone();
        assert!(stats.forward_reads.is_empty());
        assert!(stats.forward_writes.is_empty());
    }

    #[test]
    fn test_dpc_only_supported_on_root_or_downstream_switch_ports() {
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

        let root = PcieDownstreamPort::new(
            "root",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        let mut header = 0u32;
        root.cfg_space.read_u32(0x100, &mut header).unwrap();
        assert_eq!((header & 0xffff) as u16, ExtendedCapabilityId::DPC.0);

        let downstream = PcieDownstreamPort::new(
            "dsp",
            hardware_ids,
            DevicePortType::DownstreamSwitchPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        downstream.cfg_space.read_u32(0x100, &mut header).unwrap();
        assert_eq!((header & 0xffff) as u16, ExtendedCapabilityId::DPC.0);

        let upstream = PcieDownstreamPort::new(
            "usp",
            hardware_ids,
            DevicePortType::UpstreamSwitchPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        upstream.cfg_space.read_u32(0x100, &mut header).unwrap();
        assert_ne!((header & 0xffff) as u16, ExtendedCapabilityId::DPC.0);
    }

    #[test]
    fn test_inject_dpc_two_phase_updates_aer_and_rp_busy() {
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
            "root",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );

        let injected_unc_status = UncorrectableErrorStatus::new()
            .with_data_link_protocol_error_status(true)
            .into_bits();

        port.inject_dpc_begin(PcieAerInjectRequest {
            kind: PciAerErrorKind::Uncorrectable,
            status_bits: injected_unc_status,
            header_log: [1, 2, 3, 4],
            source_id: 0x0100,
        })
        .unwrap();

        let aer_off = find_ext_cap_offset_type1(&port.cfg_space, ExtendedCapabilityId::AER);
        let dpc_off = find_ext_cap_offset_type1(&port.cfg_space, ExtendedCapabilityId::DPC);

        let mut v = 0u32;
        port.cfg_space
            .read_u32(
                aer_off + AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_STATUS.0,
                &mut v,
            )
            .unwrap();
        let unc_status = UncorrectableErrorStatus::from_bits(v);
        assert!(unc_status.data_link_protocol_error_status());

        port.cfg_space
            .read_u32(
                dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        let status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(status.dpc_rp_busy());

        port.inject_dpc_complete().unwrap();
        port.cfg_space
            .read_u32(
                dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        let status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(!status.dpc_rp_busy());
    }

    #[test]
    fn test_inject_dpc_supported_on_root_and_downstream_only() {
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
        let request = PcieAerInjectRequest {
            kind: PciAerErrorKind::Uncorrectable,
            status_bits: UncorrectableErrorStatus::new()
                .with_data_link_protocol_error_status(true)
                .into_bits(),
            header_log: [0; 4],
            source_id: 0x0100,
        };

        let mut root = PcieDownstreamPort::new(
            "root",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        assert!(root.inject_dpc_begin(request).is_ok());

        let mut downstream = PcieDownstreamPort::new(
            "dsp",
            hardware_ids,
            DevicePortType::DownstreamSwitchPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        assert!(downstream.inject_dpc_begin(request).is_ok());

        let mut upstream = PcieDownstreamPort::new(
            "usp",
            hardware_ids,
            DevicePortType::UpstreamSwitchPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        assert!(upstream.inject_dpc_begin(request).is_err());

        let mut no_dpc = PcieDownstreamPort::new(
            "root-no-dpc",
            hardware_ids,
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: None,
                ..Default::default()
            },
            None,
            None,
        );
        assert!(no_dpc.inject_dpc_begin(request).is_err());
    }

    #[test]
    fn test_inject_dpc_downstream_port_containment_updates_endpoint_and_clears_busy() {
        use pci_core::capabilities::extended::aer::AerExtendedCapability;
        use pci_core::spec::hwid::{ClassCode, ProgrammingInterface, Subclass};
        use std::sync::Mutex;

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

        let endpoint_aer = Arc::new(Mutex::new(AerExtendedCapability::new(
            &DevicePortType::Endpoint,
        )));
        let endpoint = TestAerEndpoint::new(0, endpoint_aer.clone());

        let mut port = PcieDownstreamPort::new(
            "dsp",
            hardware_ids,
            DevicePortType::DownstreamSwitchPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        port.add_pcie_device("dsp", "ep0", Box::new(endpoint))
            .unwrap();

        // Enable DPC trigger + interrupt.
        let dpc_off = find_ext_cap_offset_type1(&port.cfg_space, ExtendedCapabilityId::DPC);
        let dpc_control = DpcControl::new()
            .with_dpc_trigger_enable(1)
            .with_dpc_interrupt_enable(true);
        port.cfg_space
            .write_u32(
                dpc_off + DpcExtendedCapabilityHeader::CAPABILITY_CONTROL.0,
                (dpc_control.into_bits() as u32) << 16,
            )
            .unwrap();

        let header_log = [0xdead_0001, 0xbeef_0002, 0xcafe_0003, 0xfeed_0004];
        let injected_unc_status = UncorrectableErrorStatus::new()
            .with_data_link_protocol_error_status(true)
            .into_bits();
        port.inject_dpc_begin(PcieAerInjectRequest {
            kind: PciAerErrorKind::Uncorrectable,
            status_bits: injected_unc_status,
            header_log,
            source_id: 0,
        })
        .unwrap();

        let mut v = 0u32;
        port.cfg_space
            .read_u32(
                dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        assert_eq!((v & 0xffff) as u16, 0);
        let status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(status.dpc_rp_busy());

        let endpoint_aer = endpoint_aer.lock().expect("endpoint AER mutex poisoned");
        let endpoint_unc_status = UncorrectableErrorStatus::from_bits(read_aer_dword(
            &endpoint_aer,
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_STATUS,
        ));
        assert!(endpoint_unc_status.data_link_protocol_error_status());
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_0),
            header_log[0]
        );
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_1),
            header_log[1]
        );
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_2),
            header_log[2]
        );
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_3),
            header_log[3]
        );

        port.inject_dpc_complete().unwrap();
        port.cfg_space
            .read_u32(
                dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        let status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(!status.dpc_rp_busy());
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
        port.cfg_space.write_u32(0x18, 0x0012_1000).unwrap();
        assert_eq!(port.cfg_space.assigned_bus_range(), 0x10..=0x12);

        let saved = port.cfg_space.save().expect("save should succeed");

        // Change state away from saved values.
        port.cfg_space.write_u32(0x18, 0x0000_0000).unwrap();
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
        let mut value = 0u32;
        with_acs.cfg_space.read_u32(0x100, &mut value).unwrap();
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
        without_acs.cfg_space.read_u32(0x100, &mut value).unwrap();
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
                aer: None,
                dpc: None,
                cxl_flex_bus_port_capability: Some(
                    CxlFlexBusPortDvsecCapability::new().with_mem_capable(true),
                ),
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

        let mut value = 0u32;
        port.cfg_space.read_u32(0x100, &mut value).unwrap();
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
