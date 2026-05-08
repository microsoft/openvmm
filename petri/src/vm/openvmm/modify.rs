// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers to modify a [`PetriVmConfigOpenVmm`] from its defaults.

// TODO: Delete all modification functions that are not backend-specific
// from this file, add necessary settings to the backend-agnostic
// `PetriVmConfig`, and add corresponding functions to `PetriVmBuilder`.

use super::MANA_INSTANCE;
use super::NIC_MAC_ADDRESS;
use super::PetriVmConfigOpenVmm;
use chipset_resources::battery::BatteryDeviceHandleX64;
use chipset_resources::battery::HostBatteryUpdate;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use gdma_resources::GdmaDeviceHandle;
use gdma_resources::VportDefinition;
use get_resources::ged::IgvmAttestTestConfig;
use guid::Guid;
use memory_range::MemoryRange;
use net_backend_resources::mac_address::MacAddress;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use openvmm_defs::config::Config;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::PcieRootPortConfig;
use openvmm_defs::config::PcieSwitchConfig;
use openvmm_defs::config::VpciDeviceConfig;
use openvmm_defs::config::Vtl2BaseAddressType;
use vm_resource::IntoResource;
use vmotherboard::ChipsetDeviceHandle;

impl PetriVmConfigOpenVmm {
    /// Enable the VTL0 alias map.
    // TODO: Remove once #912 is fixed.
    pub fn with_vtl0_alias_map(mut self) -> Self {
        self.config
            .hypervisor
            .with_vtl2
            .as_mut()
            .expect("Not an openhcl config.")
            .vtl0_alias_map = true;
        self
    }

    /// Enable the battery for the VM.
    pub fn with_battery(mut self) -> Self {
        if self.resources.properties.is_openhcl {
            self.ged.as_mut().unwrap().enable_battery = true;
        } else {
            self.config.chipset_devices.push(ChipsetDeviceHandle {
                name: "battery".to_string(),
                resource: BatteryDeviceHandleX64 {
                    battery_status_recv: {
                        let (tx, rx) = mesh::channel();
                        tx.send(HostBatteryUpdate::default_present());
                        rx
                    },
                }
                .into_resource(),
            });
            if let LoadMode::Uefi { enable_battery, .. } = &mut self.config.load_mode {
                *enable_battery = true;
            }
        }
        self
    }

    /// Set test config for the GED's IGVM attest request handler
    pub fn with_igvm_attest_test_config(mut self, config: IgvmAttestTestConfig) -> Self {
        if !self.resources.properties.is_openhcl {
            panic!("IGVM Attest test config is only supported for OpenHCL.")
        };

        let ged = self.ged.as_mut().expect("No GED to configure TPM");

        ged.igvm_attest_test_config = Some(config);

        self
    }

    /// Enable a synthnic for the VM.
    ///
    /// Uses a mana emulator and the paravisor if a paravisor is present.
    pub fn with_nic(mut self) -> Self {
        let endpoint = net_backend_resources::consomme::ConsommeHandle {
            cidr: None,
            ports: Vec::new(),
        }
        .into_resource();
        if let Some(vtl2_settings) = self.runtime_config.vtl2_settings.as_mut() {
            self.config.vpci_devices.push(VpciDeviceConfig {
                vtl: DeviceVtl::Vtl2,
                instance_id: MANA_INSTANCE,
                resource: GdmaDeviceHandle {
                    vports: vec![VportDefinition {
                        mac_address: NIC_MAC_ADDRESS,
                        endpoint,
                    }],
                }
                .into_resource(),
            });

            vtl2_settings.dynamic.as_mut().unwrap().nic_devices.push(
                vtl2_settings_proto::NicDeviceLegacy {
                    instance_id: MANA_INSTANCE.to_string(),
                    subordinate_instance_id: None,
                    max_sub_channels: None,
                },
            );
        } else {
            const NETVSP_INSTANCE: Guid = guid::guid!("c6c46cc3-9302-4344-b206-aef65e5bd0a2");
            self.config.vmbus_devices.push((
                DeviceVtl::Vtl0,
                netvsp_resources::NetvspHandle {
                    instance_id: NETVSP_INSTANCE,
                    mac_address: NIC_MAC_ADDRESS,
                    endpoint,
                    max_queues: None,
                }
                .into_resource(),
            ));
        }

        self
    }

    /// Add a PCIe NIC to the VM using the MANA emulator.
    pub fn with_pcie_nic(mut self, port_name: &str, mac_address: MacAddress) -> Self {
        let endpoint = net_backend_resources::consomme::ConsommeHandle {
            cidr: None,
            ports: Vec::new(),
        }
        .into_resource();
        self.config.pcie_devices.push(PcieDeviceConfig {
            port_name: port_name.to_string(),
            resource: GdmaDeviceHandle {
                vports: vec![VportDefinition {
                    mac_address,
                    endpoint,
                }],
            }
            .into_resource(),
        });

        self
    }

    /// Add a PCIe NVMe device to the VM using the NVMe emulator.
    pub fn with_pcie_nvme(mut self, port_name: &str, subsystem_id: Guid) -> Self {
        self.config.pcie_devices.push(PcieDeviceConfig {
            port_name: port_name.to_string(),
            resource: NvmeControllerHandle {
                subsystem_id,
                max_io_queues: 64,
                msix_count: 64,
                namespaces: vec![NamespaceDefinition {
                    nsid: 1,
                    disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                        len: Some(1024 * 1024),
                        sector_size: None,
                    })
                    .into_resource(),
                    read_only: false,
                }],
                requests: None,
            }
            .into_resource(),
        });

        self
    }

    /// Enable a virtio-net NIC for the VM backed by Consomme.
    ///
    /// This exposes a virtio-net device on a PCIe root port, suitable for
    /// guests running virtio drivers (e.g. Linux with UEFI boot).
    pub fn with_virtio_nic(mut self, port_name: &str) -> Self {
        let endpoint = net_backend_resources::consomme::ConsommeHandle {
            cidr: None,
            ports: Vec::new(),
        }
        .into_resource();

        self.config.pcie_devices.push(PcieDeviceConfig {
            port_name: port_name.to_string(),
            resource: virtio_resources::VirtioPciDeviceHandle(
                virtio_resources::net::VirtioNetHandle {
                    max_queues: None,
                    mac_address: NIC_MAC_ADDRESS,
                    endpoint,
                }
                .into_resource(),
            )
            .into_resource(),
        });

        self
    }

    /// Load with the specified VTL2 relocation mode.
    pub fn with_vtl2_relocation_mode(mut self, mode: Vtl2BaseAddressType) -> Self {
        let LoadMode::Igvm {
            vtl2_base_address, ..
        } = &mut self.config.load_mode
        else {
            panic!("vtl2 relocation mode is only supported for OpenHCL firmware")
        };
        *vtl2_base_address = mode;
        self
    }

    /// Use a file-backed memory region instead of anonymous RAM.
    ///
    /// The file at the given path will be created (or opened) and sized to
    /// match the VM's configured memory. Guest memory is then backed by
    /// this file, which persists across snapshot save/restore.
    pub fn with_memory_backing_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.memory_backing_file = Some(path.into());
        self
    }

    /// Use explicit hugetlb-backed guest memory.
    pub fn with_hugepages(mut self, hugepage_size: Option<u64>) -> Self {
        self.config.memory.hugepages = true;
        self.config.memory.hugepage_size = hugepage_size;
        self
    }

    /// Add a symmetric PCIe topology to the VM based on some basic scale factors
    ///
    /// All root ports are named according to their index within their parent
    /// using the naming scheme `sXrcYrpZ`. For example, the third root port on
    /// the fourth root complex in segment 0 would be named `s0rc3rp2`.
    pub fn with_pcie_root_topology(
        mut self,
        segment_count: u64,
        root_complex_per_segment: u64,
        root_ports_per_root_complex: u64,
    ) -> Self {
        const SINGLE_BUS_NUMBER_ECAM_SIZE: u64 = 1024 * 1024; // 1 MB
        const FULL_SEGMENT_ECAM_SIZE: u64 = 256 * SINGLE_BUS_NUMBER_ECAM_SIZE; // 256 MB
        const LOW_MMIO_SIZE: u64 = 64 * 1024 * 1024; // 64 MB
        const HIGH_MMIO_SIZE: u64 = 1024 * 1024 * 1024; // 1 GB

        // Allocate and configure the address space gaps
        let ecam_size = segment_count * FULL_SEGMENT_ECAM_SIZE;
        let low_mmio_size = segment_count * root_complex_per_segment * LOW_MMIO_SIZE;
        let high_mmio_size = segment_count * root_complex_per_segment * HIGH_MMIO_SIZE;

        let low_mmio_start = self.config.memory.mmio_gaps[0].start();
        let high_mmio_end = self.config.memory.mmio_gaps[1].end();

        let ecam_gap = MemoryRange::new(low_mmio_start - ecam_size..low_mmio_start);
        let low_gap = MemoryRange::new(ecam_gap.start() - low_mmio_size..ecam_gap.start());
        let high_gap = MemoryRange::new(high_mmio_end..high_mmio_end + high_mmio_size);

        self.config.memory.pci_ecam_gaps.push(ecam_gap);
        self.config.memory.pci_mmio_gaps.push(low_gap);
        self.config.memory.pci_mmio_gaps.push(high_gap);

        // Add the root complexes to the VM
        for segment in 0..segment_count {
            let bus_count_per_rc = 256 / root_complex_per_segment;
            for rc_index_in_segment in 0..root_complex_per_segment {
                let index = segment * root_complex_per_segment + rc_index_in_segment;
                let name = format!("s{}rc{}", segment, rc_index_in_segment);

                let start_bus = rc_index_in_segment * bus_count_per_rc;
                let end_bus = start_bus + bus_count_per_rc - 1;

                let ecam_range_start = ecam_gap.start()
                    + segment * FULL_SEGMENT_ECAM_SIZE
                    + start_bus * SINGLE_BUS_NUMBER_ECAM_SIZE;
                let ecam_range_end =
                    ecam_range_start + bus_count_per_rc * SINGLE_BUS_NUMBER_ECAM_SIZE;

                let low_mmio_start = low_gap.start() + index * LOW_MMIO_SIZE;
                let low_mmio_end = low_gap.start() + (index + 1) * LOW_MMIO_SIZE;
                let high_mmio_start = high_gap.start() + index * HIGH_MMIO_SIZE;
                let high_mmio_end = high_gap.start() + (index + 1) * HIGH_MMIO_SIZE;

                let ports = (0..root_ports_per_root_complex)
                    .map(|i| PcieRootPortConfig {
                        name: format!("s{}rc{}rp{}", segment, rc_index_in_segment, i),
                        hotplug: true,
                    })
                    .collect();

                self.config.pcie_root_complexes.push(PcieRootComplexConfig {
                    index: index.try_into().unwrap(),
                    name,
                    segment: segment.try_into().unwrap(),
                    start_bus: start_bus.try_into().unwrap(),
                    end_bus: end_bus.try_into().unwrap(),
                    ecam_range: MemoryRange::new(ecam_range_start..ecam_range_end),
                    low_mmio: MemoryRange::new(low_mmio_start..low_mmio_end),
                    high_mmio: MemoryRange::new(high_mmio_start..high_mmio_end),
                    ports,
                });
            }
        }

        self
    }

    /// Add a PCIe switch to the VM.
    pub fn with_pcie_switch(
        mut self,
        port_name: &str,
        switch_name: &str,
        port_count: u8,
        hotplug: bool,
    ) -> Self {
        self.config.pcie_switches.push(PcieSwitchConfig {
            name: switch_name.to_string(),
            num_downstream_ports: port_count,
            parent_port: port_name.to_string(),
            hotplug,
        });
        self
    }

    /// This is intended for special one-off use cases. As soon as something
    /// is needed in multiple tests we should consider making it a supported
    /// pattern.
    pub fn with_custom_config(mut self, f: impl FnOnce(&mut Config)) -> Self {
        f(&mut self.config);
        self
    }

    /// Specifies whether VTL2 should be allowed to access VTL0 memory before it
    /// sets any VTL protections.
    ///
    /// This is needed just for the TMK VMM, and only until it gains support for
    /// setting VTL protections.
    pub fn with_allow_early_vtl0_access(mut self, allow: bool) -> Self {
        self.config
            .hypervisor
            .with_vtl2
            .as_mut()
            .unwrap()
            .late_map_vtl0_memory =
            (!allow).then_some(openvmm_defs::config::LateMapVtl0MemoryPolicy::InjectException);

        self
    }
}
