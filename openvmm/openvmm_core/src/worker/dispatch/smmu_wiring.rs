// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(guest_arch = "aarch64")]

//! SMMU resource resolution and wiring helpers for aarch64 VMs.
//!
//! This module handles combining SMMU MMIO ranges (from the memory layout
//! allocator) with SPI assignments (from the SPI allocator) into resolved
//! resources and instantiating SMMU chipset devices.

use anyhow::Context as _;
use chipset_device_resources::IRQ_LINE_SET;
use guestmem::GuestMemory;
use std::sync::Arc;
use vm_topology::pcie::PcieHostBridge;
use vmotherboard::ChipsetBuilder;

/// Default advertised OAS (in bits) for an `oas=auto` SMMU.
///
/// This is a fixed sizing policy, not a computed maximum: rather than sizing
/// the advertised OAS to the guest memory layout (or to the host's supported
/// IPA width, which on aarch64/KVM can be up to 52 bits), an `auto` SMMU
/// advertises a constant 48 bits. This matches the fixed-OAS approach taken by
/// Hyper-V's emulated SMMU.
///
/// The memory-layout allocator packs high MMIO compactly bottom-up just above
/// guest RAM, so for typical configurations every translatable address sits
/// far below 48 bits (256 TiB). This is not a hard guarantee, though: a large
/// enough RAM size, or an explicitly pinned high MMIO/ECAM base, can place
/// addresses above 256 TiB (up to the host IPA width). Such a configuration
/// must pass an explicit `oas=` (e.g. `oas=52`) rather than relying on `auto`.
///
/// For accelerated SMMUs this is only a provisional value, replaced by the
/// host SMMU's OAS when a device attaches.
const DEFAULT_AUTO_OAS_BITS: u8 = 48;

/// Resources for a single SMMUv3 instance, identified by root complex.
pub(super) struct ResolvedSmmuResources {
    /// Position in the unfiltered root-complex and host-bridge arrays.
    pub rc_pos: usize,
    /// Root-complex identity, not a position in a filtered list.
    pub rc_index: u32,
    /// MMIO base address (from the memory layout allocator).
    pub base: u64,
    /// GIC INTID for the event queue interrupt (from the SPI allocator).
    pub evtq_intid: u32,
    /// GIC INTID for the global error interrupt (from the SPI allocator).
    pub gerr_intid: u32,
}

/// All resolved resources shared by the VM's SMMUv3 instances.
#[derive(Default)]
pub(super) struct ResolvedSmmu {
    /// Per-instance MMIO and interrupt resources.
    pub instances: Vec<ResolvedSmmuResources>,
    /// IOVA range reserved for assigned-device MSI writes.
    pub device_assignment_msi_iova_range: Option<memory_range::MemoryRange>,
}

/// Associates the allocators' ordered MMIO and SPI slots with root complexes
/// once, preserving allocation order. Consumers use the recorded RC identity.
pub(super) fn resolve_smmu_resources(
    root_complexes: &[openvmm_defs::config::PcieRootComplexConfig],
    smmu_ranges: &[memory_range::MemoryRange],
    spi_layout: &crate::worker::spi_layout::ResolvedSpiLayout,
    device_assignment_msi_iova_range: Option<memory_range::MemoryRange>,
) -> ResolvedSmmu {
    assert_eq!(smmu_ranges.len(), spi_layout.smmu.len());
    let mut allocations = smmu_ranges.iter().zip(&spi_layout.smmu);
    let mut instances = Vec::new();
    for (rc_pos, rc) in root_complexes.iter().enumerate() {
        if !matches!(
            rc.iommu,
            Some(openvmm_defs::config::PcieIommuConfig::Smmu { .. })
        ) {
            continue;
        }
        let (range, spis) = allocations
            .next()
            .expect("resources allocated for every SMMU");
        instances.push(ResolvedSmmuResources {
            rc_pos,
            rc_index: rc.index,
            base: range.start(),
            evtq_intid: spis.evtq_intid,
            gerr_intid: spis.gerr_intid,
        });
    }
    assert!(allocations.next().is_none(), "unused SMMU allocations");
    ResolvedSmmu {
        instances,
        device_assignment_msi_iova_range,
    }
}

/// Result of [`setup_smmu`].
#[derive(Default)]
pub(super) struct SmmuDevicesResult {
    /// Per-RC SMMU shared state, indexed parallel to `pcie_host_bridges`.
    /// `None` for root complexes without an SMMU.
    pub shared_states: Vec<Option<Arc<smmu::SmmuSharedState>>>,
    /// Firmware wiring paired with the corresponding device state.
    devices: Vec<SmmuFirmwareDevice>,
}

struct SmmuFirmwareDevice {
    config: vmm_core::acpi_builder::AcpiSmmuConfig,
    shared_state: Arc<smmu::SmmuSharedState>,
}

impl SmmuDevicesResult {
    /// Builds firmware configuration after PCI assignment has started the
    /// devices and frozen their capabilities.
    pub fn firmware_configs(&self) -> Vec<vmm_core::acpi_builder::AcpiSmmuConfig> {
        self.devices
            .iter()
            .map(|device| vmm_core::acpi_builder::AcpiSmmuConfig {
                // Platform policy: offer ATS on the RC when its SMMU supports
                // it. IORT describes RC support, not the SMMU's IDR0 bit.
                ats_supported: device.shared_state.ats_supported(),
                ..device.config.clone()
            })
            .collect()
    }
}

fn reserved_iova_ranges(
    accel: bool,
    device_assignment_msi_iova_range: Option<memory_range::MemoryRange>,
) -> anyhow::Result<Vec<memory_range::MemoryRange>> {
    if !accel {
        return Ok(Vec::new());
    }
    Ok(vec![device_assignment_msi_iova_range.context(
        "the hypervisor does not support an accelerated device-assignment MSI IOVA reservation",
    )?])
}

/// Instantiate SMMU chipset devices for root complexes that have SMMU
/// configured.
///
/// Creates one device per resolved instance and wires up its interrupts.
///
/// `acpi_available` gates accelerated SMMUs, which need IORT RMR nodes to
/// reserve the host's MSI IOVA window in the guest.
pub(super) fn setup_smmu(
    root_complexes: &[openvmm_defs::config::PcieRootComplexConfig],
    resolved: &ResolvedSmmu,
    pcie_host_bridges: &mut [PcieHostBridge],
    chipset_builder: &ChipsetBuilder<'_>,
    gm: &GuestMemory,
    acpi_available: bool,
) -> anyhow::Result<SmmuDevicesResult> {
    // Instantiate SMMU chipset devices.
    let mut shared_states: Vec<Option<Arc<smmu::SmmuSharedState>>> =
        vec![None; pcie_host_bridges.len()];
    let mut devices = Vec::new();

    for smmu in &resolved.instances {
        let rc_pos = smmu.rc_pos;
        let rc = &root_complexes[rc_pos];
        let bridge = &mut pcie_host_bridges[rc_pos];
        assert_eq!(
            rc.index, smmu.rc_index,
            "SMMU root-complex identity changed"
        );
        assert_eq!(
            bridge.index, smmu.rc_index,
            "SMMU host-bridge identity mismatch"
        );
        let rc_name = &rc.name;
        let Some(openvmm_defs::config::PcieIommuConfig::Smmu {
            accel,
            oas,
            ssidsize,
        }) = rc.iommu
        else {
            anyhow::bail!("root complex {rc_name} no longer has an SMMU");
        };
        anyhow::ensure!(
            shared_states[rc_pos].is_none(),
            "duplicate SMMU for root complex {rc_name}"
        );
        anyhow::ensure!(
            !accel || acpi_available,
            "SMMU on root complex {rc_name}: accelerated translation requires ACPI"
        );

        let evtq_irq_vector = smmu.evtq_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let gerror_irq_vector = smmu.gerr_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let device_name = format!("smmu:{rc_name}");
        let smmu_config = smmu::SmmuConfig {
            sidsize: 16,
            oas_policy: match oas {
                openvmm_defs::config::SmmuOas::Auto => smmu::SmmuOasPolicy::Auto {
                    provisional: DEFAULT_AUTO_OAS_BITS,
                },
                openvmm_defs::config::SmmuOas::Fixed(bits) => smmu::SmmuOasPolicy::Fixed(bits),
            },
            ssid_policy: match ssidsize {
                openvmm_defs::config::SmmuSsidSize::Auto => smmu::SmmuSsidPolicy::Auto,
                openvmm_defs::config::SmmuSsidSize::Fixed(bits) => {
                    smmu::SmmuSsidPolicy::Fixed(bits)
                }
            },
            accel,
        };
        let smmu_device = chipset_builder
            .arc_mutex_device(device_name.as_str())
            .try_add(|services| {
                let evtq_irq = services.new_line(IRQ_LINE_SET, "evtq", evtq_irq_vector);
                let gerror_irq = services.new_line(IRQ_LINE_SET, "gerror", gerror_irq_vector);
                smmu::SmmuDevice::new(
                    smmu.base,
                    gm.clone(),
                    &smmu_config,
                    Some(evtq_irq),
                    Some(gerror_irq),
                )
            })
            .with_context(|| format!("SMMU on root complex {rc_name}"))?;

        let shared_state = smmu_device.lock().shared_state().clone();
        shared_states[rc_pos] = Some(shared_state.clone());
        let reserved_iova_ranges =
            reserved_iova_ranges(accel, resolved.device_assignment_msi_iova_range)
                .with_context(|| format!("SMMU on root complex {rc_name}"))?;
        if accel {
            // These reserved IOVA ranges become IORT RMR entries. Mark the
            // root complex so the SSDT emits a PCI Firmware _DSM (function 5,
            // preserve boot config); Linux skips RMR entries for root
            // complexes without this flag.
            bridge.preserve_boot_config = true;
        }

        devices.push(SmmuFirmwareDevice {
            config: vmm_core::acpi_builder::AcpiSmmuConfig {
                rc_index: bridge.index,
                segment: bridge.segment,
                base: smmu.base,
                event_gsiv: smmu.evtq_intid,
                gerr_gsiv: smmu.gerr_intid,
                ats_supported: false,
                reserved_iova_ranges,
            },
            shared_state,
        });
    }

    Ok(SmmuDevicesResult {
        shared_states,
        devices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::mmio::MmioIntercept;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;

    const TEST_RANGE: memory_range::MemoryRange = memory_range::MemoryRange::new(0x1000..0x20_0000);

    #[test]
    fn resolved_resources_retain_root_complex_identity() {
        use crate::worker::spi_layout::SpiLayoutInput;
        use crate::worker::spi_layout::resolve_spi_layout;
        use memory_range::MemoryRange;
        use openvmm_defs::config::PcieIommuConfig;
        use openvmm_defs::config::PcieMmioRangeConfig;
        use openvmm_defs::config::PcieRootComplexConfig;
        use openvmm_defs::config::SmmuOas;
        use openvmm_defs::config::SmmuSsidSize;

        let root_complexes =
            [(9, Some(14)), (2, None), (7, Some(0))].map(|(index, ssid)| PcieRootComplexConfig {
                index,
                name: format!("rc{index}"),
                segment: index as u16,
                start_bus: 0,
                end_bus: 255,
                low_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
                high_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
                ports: Vec::new(),
                cxl: None,
                iommu: ssid.map(|bits| PcieIommuConfig::Smmu {
                    accel: false,
                    oas: SmmuOas::Auto,
                    ssidsize: SmmuSsidSize::Fixed(bits),
                }),
                vnode: None,
                preserve_bars: false,
            });
        let ranges = [
            MemoryRange::new(0x100_0000..0x102_0000),
            MemoryRange::new(0x102_0000..0x104_0000),
        ];
        let spi_layout = resolve_spi_layout(&SpiLayoutInput {
            gic_nr_irqs: 256,
            v2m_spi_count: None,
            smmu_count: 2,
        })
        .unwrap();
        let resolved = resolve_smmu_resources(&root_complexes, &ranges, &spi_layout, None);

        assert_eq!(resolved.instances.len(), 2);
        // The middle RC consumes no allocation; RC identity is neither its
        // input position nor its position in the SMMU-only list.
        for (allocation, rc_pos) in [(0, 0), (1, 2)] {
            let instance = &resolved.instances[allocation];
            assert_eq!(instance.rc_pos, rc_pos);
            assert_eq!(instance.rc_index, root_complexes[rc_pos].index);
            assert_eq!(instance.base, ranges[allocation].start());
            assert_eq!(instance.evtq_intid, spi_layout.smmu[allocation].evtq_intid);
            assert_eq!(instance.gerr_intid, spi_layout.smmu[allocation].gerr_intid);
        }
    }

    #[pal_async::async_test]
    async fn firmware_capabilities_follow_device_start() {
        struct Viommu;
        impl smmu::Invalidate for Viommu {
            fn invalidate(&self, _: &[[u64; 2]]) -> Result<(), usize> {
                Ok(())
            }
        }

        fn read_idr(device: &mut smmu::SmmuDevice, index: u64) -> u32 {
            let mut bytes = [0; 4];
            assert!(matches!(
                device.mmio_read(index * 4, &mut bytes),
                chipset_device::io::IoResult::Ok
            ));
            u32::from_le_bytes(bytes)
        }

        const IDR0_ATS: u32 = 1 << 10;
        for cold_plug_ats in [None, Some(false), Some(true)] {
            let mut device = smmu::SmmuDevice::new(
                0,
                GuestMemory::empty(),
                &smmu::SmmuConfig {
                    sidsize: 16,
                    oas_policy: smmu::SmmuOasPolicy::Fixed(40),
                    ssid_policy: smmu::SmmuSsidPolicy::Auto,
                    accel: true,
                },
                None,
                None,
            )
            .unwrap();
            let state = device.shared_state().clone();
            let viommu = Arc::new(Viommu);
            let mut idr = std::array::from_fn(|index| read_idr(&mut device, index as u64));
            // The physical SMMU supports 14-bit SSIDs.
            idr[1] |= 14 << 6;
            let devices = SmmuDevicesResult {
                shared_states: vec![Some(state.clone())],
                devices: vec![SmmuFirmwareDevice {
                    shared_state: state.clone(),
                    config: vmm_core::acpi_builder::AcpiSmmuConfig {
                        rc_index: 7,
                        segment: 2,
                        base: 0,
                        event_gsiv: 35,
                        gerr_gsiv: 36,
                        ats_supported: false,
                        reserved_iova_ranges: Vec::new(),
                    },
                }],
            };
            // Reading firmware configuration must not freeze discovery.
            assert!(!devices.firmware_configs()[0].ats_supported);
            if let Some(ats) = cold_plug_ats {
                if ats {
                    idr[0] |= IDR0_ATS;
                }
                state
                    .bind_accel_viommu(smmu::HostSmmuCaps::from_idr(idr), &viommu)
                    .unwrap();
            }
            // PCI resource assignment starts and stops device state units
            // before firmware is built, without running guest VPs.
            device.start();
            device.stop().await;
            let configs = devices.firmware_configs();
            let advertised_idr0 = read_idr(&mut device, 0);
            let advertised_idr1 = read_idr(&mut device, 1);
            assert_eq!(configs[0].ats_supported, cold_plug_ats == Some(true));
            assert_eq!(configs[0].ats_supported, advertised_idr0 & IDR0_ATS != 0);

            if cold_plug_ats.is_none() {
                idr[0] |= IDR0_ATS;
                state
                    .bind_accel_viommu(smmu::HostSmmuCaps::from_idr(idr), &viommu)
                    .unwrap();
            }
            device.reset().await;
            device.start();
            device.stop().await;
            let reloaded = devices.firmware_configs();
            assert_eq!(reloaded.len(), 1);
            assert_eq!(reloaded[0].rc_index, 7);
            assert_eq!(reloaded[0].ats_supported, configs[0].ats_supported);
            assert_eq!(read_idr(&mut device, 0), advertised_idr0);
            assert_eq!(read_idr(&mut device, 1), advertised_idr1);
        }
    }

    #[test]
    fn accelerated_smmu_uses_device_assignment_msi_iova_range() {
        assert_eq!(
            reserved_iova_ranges(true, Some(TEST_RANGE)).unwrap(),
            [TEST_RANGE]
        );
    }

    #[test]
    fn non_accelerated_smmu_has_no_reserved_iova_range() {
        assert!(
            reserved_iova_ranges(false, Some(TEST_RANGE))
                .unwrap()
                .is_empty()
        );
        assert!(reserved_iova_ranges(false, None).unwrap().is_empty());
    }

    #[test]
    fn accelerated_smmu_requires_reserved_iova_range() {
        assert!(reserved_iova_ranges(true, None).is_err());
    }
}
