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
use gdma_resources::GdmaDeviceHandle;
use gdma_resources::VportDefinition;
use get_resources::ged::IgvmAttestTestConfig;
use openvmm_defs::config::Config;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::LoadMode;
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
        if self.firmware.is_openhcl() {
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
        if !self.firmware.is_openhcl() {
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
        let endpoint =
            net_backend_resources::consomme::ConsommeHandle { cidr: None }.into_resource();
        if self.vtl2_settings.is_some() {
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

            self.vtl2_settings
                .as_mut()
                .unwrap()
                .dynamic
                .as_mut()
                .unwrap()
                .nic_devices
                .push(vtl2_settings_proto::NicDeviceLegacy {
                    instance_id: MANA_INSTANCE.to_string(),
                    subordinate_instance_id: None,
                    max_sub_channels: None,
                });
        } else {
            const NETVSP_INSTANCE: guid::Guid = guid::guid!("c6c46cc3-9302-4344-b206-aef65e5bd0a2");
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
