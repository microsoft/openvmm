// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe topology construction and validation helpers.
//!
//! These turn the manifest's [`PcieRootComplexConfig`]/[`PciePortConfig`]
//! entries into runtime root-port definitions and validate that the configured
//! root complexes form a consistent bus-number topology before the VM is built.

use cxl_spec::pci_registers::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecCapability;
use openvmm_defs::config::PciePortConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use pci_core::spec::caps::acs::DEFAULT_ACS_CAP_MASK;
use pcie::GenericPciePortDefinition;
use pcie::PciePortSettings;

/// Builds root-port PCIe settings from manifest flags.
///
/// When CXL is enabled, emit a default Flex Bus capability advertising both
/// cache and memory support.
fn build_root_port_settings(rp_cfg: &PciePortConfig) -> PciePortSettings {
    PciePortSettings {
        acs_capabilities_supported: rp_cfg
            .acs_capabilities_supported
            .unwrap_or(DEFAULT_ACS_CAP_MASK),
        cxl_flex_bus_port_capability: rp_cfg.cxl.then_some(
            CxlFlexBusPortDvsecCapability::new()
                .with_cache_capable(true)
                .with_mem_capable(true),
        ),
    }
}

/// Converts a manifest root-port entry into the runtime root-port definition.
pub(super) fn build_root_port_definition(rp_cfg: &PciePortConfig) -> GenericPciePortDefinition {
    let settings = build_root_port_settings(rp_cfg);

    GenericPciePortDefinition {
        name: rp_cfg.name.as_str().into(),
        devfn: rp_cfg.devfn,
        hotplug: rp_cfg.hotplug,
        settings,
    }
}

/// Validates that the configured PCIe root complexes form a consistent
/// topology: each bus range is well-formed (`start_bus <= end_bus`) and no two
/// root complexes on the same PCI segment have overlapping bus ranges.
pub(super) fn validate_pcie_root_complexes(
    root_complexes: &[PcieRootComplexConfig],
) -> anyhow::Result<()> {
    for (index, root_complex) in root_complexes.iter().enumerate() {
        if root_complex.start_bus > root_complex.end_bus {
            anyhow::bail!(
                "invalid PCIe root complex '{}': start_bus ({}) must be less than or equal to end_bus ({})",
                root_complex.name,
                root_complex.start_bus,
                root_complex.end_bus,
            );
        }

        for previous in &root_complexes[..index] {
            if root_complex.segment == previous.segment
                && root_complex.start_bus <= previous.end_bus
                && previous.start_bus <= root_complex.end_bus
            {
                anyhow::bail!(
                    "invalid PCIe root complex '{}': bus range {}..={} overlaps with '{}' bus range {}..={} on PCI segment {}",
                    root_complex.name,
                    root_complex.start_bus,
                    root_complex.end_bus,
                    previous.name,
                    previous.start_bus,
                    previous.end_bus,
                    root_complex.segment,
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openvmm_defs::config::PcieMmioRangeConfig;

    fn rc(name: &str, segment: u16, start_bus: u8, end_bus: u8) -> PcieRootComplexConfig {
        PcieRootComplexConfig {
            index: 0,
            name: name.to_string(),
            segment,
            start_bus,
            end_bus,
            low_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
            high_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
            ports: Vec::new(),
            cxl: None,
            iommu: None,
            vnode: None,
            preserve_bars: false,
        }
    }

    #[test]
    fn accepts_disjoint_ranges() {
        let rcs = [
            rc("rc0", 0, 0, 4),
            rc("rc1", 0, 5, 9),
            // Same bus range but a different segment is fine.
            rc("rc2", 1, 0, 4),
        ];
        validate_pcie_root_complexes(&rcs).unwrap();
    }

    #[test]
    fn rejects_inverted_bus_range() {
        let rcs = [rc("rc0", 0, 4, 0)];
        assert!(validate_pcie_root_complexes(&rcs).is_err());
    }

    #[test]
    fn rejects_overlapping_ranges_on_same_segment() {
        let rcs = [rc("rc0", 0, 0, 4), rc("rc1", 0, 4, 8)];
        assert!(validate_pcie_root_complexes(&rcs).is_err());
    }
}
