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
use net_backend_resources::mac_address::MacAddress;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use openvmm_defs::config::Config;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieMmioRangeConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::PcieRootPortConfig;
use openvmm_defs::config::PcieSwitchConfig;
use openvmm_defs::config::SmmuInstanceConfig;
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

    /// Expose nested-virtualization extensions (VMX/SVM) to the guest so it
    /// can itself host a nested VM.
    ///
    /// Currently a no-op for the openvmm/KVM L1 path: KVM enables nested
    /// virtualization at the kernel-module level (`kvm_intel.nested=1` /
    /// `kvm_amd.nested=1`) and openvmm forwards VMX/SVM CPUID through to
    /// the guest by default. For the openvmm/WHP L1 path this method will
    /// flip the WHP `NestedVirtualization` partition property once that
    /// plumbing lands in `virt_whp`.
    ///
    /// The method exists today so tests that require nested virt can
    /// declare their intent uniformly via `.modify_backend(|b|
    /// b.with_nested_virt())`, regardless of which backend ultimately
    /// runs them.
    pub fn with_nested_virt(self) -> Self {
        // TODO: when WHP plumbing lands, flip the partition property
        // here for the WHP backend. For now the KVM L1 path needs no
        // openvmm-side action.
        self
    }

    /// Opt out of the framework's default save/restore smoke check that
    /// runs after the VM resumes for the first time.
    ///
    /// The default flow saves, resets, and restores the VM twice to catch
    /// regressions in device save/restore. Tests that intentionally attach
    /// a device whose backend cannot round-trip through save/restore
    /// (notably virtio-fs and virtio-9p, which hold host-side filesystem
    /// state that is not serialised in the saved state) should call this
    /// to avoid a guaranteed failure that has nothing to do with what the
    /// test is exercising. See the trait doc-comment on
    /// `VirtioDevice::supports_save_restore` for which device backends
    /// fall in this category.
    pub fn without_save_restore_check(mut self) -> Self {
        self.skip_save_restore_check = true;
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
        const LOW_MMIO_SIZE: u64 = 64 * 1024 * 1024; // 64 MB
        const HIGH_MMIO_SIZE: u64 = 1024 * 1024 * 1024; // 1 GB

        // Add the root complexes to the VM
        for segment in 0..segment_count {
            let bus_count_per_rc = 256 / root_complex_per_segment;
            for rc_index_in_segment in 0..root_complex_per_segment {
                let index = segment * root_complex_per_segment + rc_index_in_segment;
                let name = format!("s{}rc{}", segment, rc_index_in_segment);

                let start_bus = rc_index_in_segment * bus_count_per_rc;
                let end_bus = start_bus + bus_count_per_rc - 1;

                let ports = (0..root_ports_per_root_complex)
                    .map(|i| PcieRootPortConfig {
                        name: format!("s{}rc{}rp{}", segment, rc_index_in_segment, i),
                        hotplug: true,
                        acs_capabilities_supported: Some(0),
                        cxl: false,
                    })
                    .collect();

                self.config.pcie_root_complexes.push(PcieRootComplexConfig {
                    index: index.try_into().unwrap(),
                    name,
                    segment: segment.try_into().unwrap(),
                    start_bus: start_bus.try_into().unwrap(),
                    end_bus: end_bus.try_into().unwrap(),
                    low_mmio: PcieMmioRangeConfig::Dynamic {
                        size: LOW_MMIO_SIZE,
                    },
                    high_mmio: PcieMmioRangeConfig::Dynamic {
                        size: HIGH_MMIO_SIZE,
                    },
                    cxl: None,
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
            acs_capabilities_supported: Some(0),
        });
        self
    }

    /// Enable SMMUv3 IOMMU on the specified root complexes (aarch64 only).
    ///
    /// Each name must match a root complex added via
    /// [`with_pcie_root_topology`](Self::with_pcie_root_topology). The SMMU
    /// provides stage 1 IOVA translation for devices behind those root
    /// complexes.
    pub fn with_smmu(mut self, rc_names: &[&str]) -> Self {
        let arch = self
            .config
            .processor_topology
            .arch
            .as_mut()
            .expect("arch topology not set");

        match arch {
            openvmm_defs::config::ArchTopologyConfig::Aarch64(aarch64) => {
                aarch64.smmu = rc_names
                    .iter()
                    .map(|name| SmmuInstanceConfig {
                        rc_name: name.to_string(),
                    })
                    .collect();
            }
            _ => panic!("SMMU is only supported on aarch64"),
        }
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
