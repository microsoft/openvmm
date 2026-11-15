// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe topology construction and validation helpers.
//!
//! These turn the manifest's [`PcieRootComplexConfig`]/[`PciePortConfig`]
//! entries into runtime root-port definitions and validate that the configured
//! root complexes form a consistent bus-number topology before the VM is built.

use cxl_spec::pci_registers::flex_bus_port_dvsec::CxlFlexBusPortDvsecCapability;
use openvmm_defs::config::PCIE_MAX_EJECT_TIMEOUT;
use openvmm_defs::config::PciePortConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use pci_core::spec::caps::acs::DEFAULT_ACS_CAP_MASK;
use pci_core::spec::caps::pci_express::MaxEndEndTlpPrefixes;
use pcie::GenericPciePortDefinition;
use pcie::PciePortSettings;
use std::collections::HashMap;
use std::time::Duration;

/// Builds port PCIe settings from manifest flags.
///
/// When CXL is enabled, emit a default Flex Bus capability advertising both
/// cache and memory support.
///
/// When PASID is enabled, advertise support for up to four TLP prefixes to
/// work for both switch and root ports.
fn build_port_settings(port_cfg: &PciePortConfig) -> PciePortSettings {
    PciePortSettings {
        attention_button: port_cfg
            .hotplug
            .is_some_and(|hotplug| hotplug.attention_button.is_some()),
        acs_capabilities_supported: port_cfg
            .acs_capabilities_supported
            .unwrap_or(DEFAULT_ACS_CAP_MASK),
        cxl_flex_bus_port_capability: port_cfg.cxl.then_some(
            CxlFlexBusPortDvsecCapability::new()
                .with_cache_capable(true)
                .with_mem_capable(true),
        ),
        tlp_prefixing_supported: port_cfg.pasid.then_some(MaxEndEndTlpPrefixes::Four),
    }
}

/// Converts a manifest port entry into the runtime port definition.
pub(super) fn build_port_definition(port_cfg: &PciePortConfig) -> GenericPciePortDefinition {
    let settings = build_port_settings(port_cfg);

    GenericPciePortDefinition {
        name: port_cfg.name.as_str().into(),
        devfn: port_cfg.devfn,
        hotplug: port_cfg.hotplug.is_some(),
        settings,
    }
}

/// Maps each root port configured for orderly removal to its eject timeout.
///
/// Root ports only: runtime hot-add and hot-remove reach root-complex ports, and the
/// command line refuses `attention_button` on switch downstream ports.
///
/// A timeout above [`PCIE_MAX_EJECT_TIMEOUT`] is refused, not clamped, as the command line
/// and ttrpc parsers do: a pending removal defers stop, reset and save, so the cap must hold
/// for every caller that builds a [`PciePortConfig`], not only those going through a parser.
pub(super) fn eject_timeouts(
    root_complexes: &[PcieRootComplexConfig],
) -> anyhow::Result<HashMap<String, Duration>> {
    let mut timeouts = HashMap::new();
    for rc in root_complexes {
        for port in &rc.ports {
            let Some(timeout) = port.hotplug.and_then(|hotplug| hotplug.attention_button) else {
                continue;
            };
            anyhow::ensure!(
                timeout <= PCIE_MAX_EJECT_TIMEOUT,
                "invalid PCIe root port '{}' on '{}': eject timeout {} ms exceeds the maximum of {} ms",
                port.name,
                rc.name,
                timeout.as_millis(),
                PCIE_MAX_EJECT_TIMEOUT.as_millis(),
            );
            timeouts.insert(port.name.clone(), timeout);
        }
    }
    Ok(timeouts)
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
    use openvmm_defs::config::PcieHotplugConfig;
    use openvmm_defs::config::PcieMmioRangeConfig;
    fn rc(name: &str, segment: u16, start_bus: u8, end_bus: u8) -> PcieRootComplexConfig {
        PcieRootComplexConfig {
            index: 0,
            name: name.to_string(),
            segment,
            start_bus,
            end_bus,
            legacy_pci_config_io: false,
            low_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
            high_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
            ports: Vec::new(),
            cxl: None,
            iommu: None,
            vnode: None,
            preserve_bars: false,
        }
    }

    fn port(name: &str, hotplug: Option<PcieHotplugConfig>) -> PciePortConfig {
        PciePortConfig {
            name: name.into(),
            devfn: None,
            hotplug,
            acs_capabilities_supported: None,
            cxl: false,
            pasid: false,
        }
    }

    #[test]
    fn attention_button_follows_the_hotplug_config() {
        let button = Some(PcieHotplugConfig {
            attention_button: Some(Duration::from_secs(15)),
        });
        for hotplug in [None, Some(PcieHotplugConfig::default()), button] {
            let definition = build_port_definition(&port("rp0", hotplug));
            assert_eq!(definition.hotplug, hotplug.is_some());
            assert_eq!(
                definition.settings.attention_button,
                hotplug.is_some_and(|h| h.attention_button.is_some())
            );
        }
    }

    #[test]
    fn eject_timeouts_list_only_attention_button_ports() {
        let mut rc0 = rc("rc0", 0, 0, 4);
        rc0.ports = vec![
            port("plain", None),
            port("hotplug", Some(PcieHotplugConfig::default())),
            port(
                "button",
                Some(PcieHotplugConfig {
                    attention_button: Some(Duration::from_millis(1500)),
                }),
            ),
        ];
        let timeouts = eject_timeouts(&[rc0]).unwrap();
        assert_eq!(timeouts.len(), 1);
        assert_eq!(timeouts["button"], Duration::from_millis(1500));
    }

    #[test]
    fn eject_timeouts_refuse_a_timeout_above_the_maximum() {
        let button = |timeout| {
            Some(PcieHotplugConfig {
                attention_button: Some(timeout),
            })
        };
        let mut rc0 = rc("rc0", 0, 0, 4);
        rc0.ports = vec![port("max", button(PCIE_MAX_EJECT_TIMEOUT))];
        assert_eq!(
            eject_timeouts(std::slice::from_ref(&rc0)).unwrap()["max"],
            PCIE_MAX_EJECT_TIMEOUT
        );

        rc0.ports.push(port(
            "long",
            button(PCIE_MAX_EJECT_TIMEOUT + Duration::from_millis(1)),
        ));
        let err = eject_timeouts(&[rc0]).unwrap_err().to_string();
        assert!(err.contains("'long'") && err.contains("exceeds"), "{err}");
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
