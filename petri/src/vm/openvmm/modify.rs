// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers to modify a [`PetriVmConfigOpenVmm`] from its defaults.

use super::MANA_INSTANCE;
use super::NIC_MAC_ADDRESS;
use super::PetriVmConfigOpenVmm;
use chipset_resources::battery::BatteryDeviceHandleX64;
use chipset_resources::battery::HostBatteryUpdate;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::DiskLayerHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use fs_err::File;
use gdma_resources::GdmaDeviceHandle;
use gdma_resources::VportDefinition;
use hvlite_defs::config::Config;
use hvlite_defs::config::DeviceVtl;
use hvlite_defs::config::LoadMode;
use hvlite_defs::config::VpciDeviceConfig;
use hvlite_defs::config::Vtl2BaseAddressType;
use hvlite_helpers::disk::open_disk_type;
use petri_artifacts_common::tags::IsOpenhclIgvm;
use petri_artifacts_core::ResolvedArtifact;
use std::path::Path;
use tpm_resources::TpmDeviceHandle;
use tpm_resources::TpmRegisterLayout;
use vm_resource::IntoResource;
use vmcore::non_volatile_store::resources::EphemeralNonVolatileStoreHandle;
use vmotherboard::ChipsetDeviceHandle;
use vtl2_settings_proto::Vtl2Settings;

impl PetriVmConfigOpenVmm {
    /// Enable VMBus redirection.
    pub fn with_vmbus_redirect(mut self) -> Self {
        self.config
            .vmbus
            .as_mut()
            .expect("vmbus not configured")
            .vtl2_redirect = true;

        let Some(ged) = &mut self.ged else {
            panic!("VMBus redirection is only supported for OpenHCL.")
        };
        ged.vmbus_redirection = true;

        self
    }

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

    /// Enable the TPM with ephemeral storage.
    pub fn with_tpm(mut self) -> Self {
        if self.firmware.is_openhcl() {
            self.ged.as_mut().unwrap().enable_tpm = true;
        } else {
            self.config.chipset_devices.push(ChipsetDeviceHandle {
                name: "tpm".to_string(),
                resource: TpmDeviceHandle {
                    ppi_store: EphemeralNonVolatileStoreHandle.into_resource(),
                    nvram_store: EphemeralNonVolatileStoreHandle.into_resource(),
                    refresh_tpm_seeds: false,
                    ak_cert_type: tpm_resources::TpmAkCertTypeResource::None,
                    register_layout: TpmRegisterLayout::IoPort,
                    guest_secret_key: None,
                    logger: None,
                }
                .into_resource(),
            });
            if let LoadMode::Uefi { enable_tpm, .. } = &mut self.config.load_mode {
                *enable_tpm = true;
            }
        }

        self
    }

    /// Set the VM to use the specified number of virtual processors.
    ///
    /// Using 1 CPU is useful for heavier OpenHCL tests, as our WHP emulation
    /// layer is rather slow when dealing with cross-cpu communication.
    pub fn with_processors(mut self, count: u32) -> Self {
        self.config.processor_topology.proc_count = count;
        self
    }

    /// Enable secure boot for the VM.
    pub fn with_secure_boot(mut self) -> Self {
        if !self.firmware.is_uefi() {
            panic!("Secure boot is only supported for UEFI firmware.");
        }
        if self.firmware.is_openhcl() {
            self.ged.as_mut().unwrap().secure_boot_enabled = true;
        } else {
            self.config.secure_boot_enabled = true;
        }
        self
    }

    /// Inject Windows secure boot templates into the VM's UEFI.
    pub fn with_windows_secure_boot_template(mut self) -> Self {
        if !self.firmware.is_uefi() {
            panic!("Secure boot templates are only supported for UEFI firmware.");
        }
        if self.firmware.is_openhcl() {
            self.ged.as_mut().unwrap().secure_boot_template =
                get_resources::ged::GuestSecureBootTemplateType::MicrosoftWindows;
        } else {
            self.config.custom_uefi_vars = hyperv_secure_boot_templates::x64::microsoft_windows();
        }
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

    /// Enable TPM state persistence
    pub fn with_tpm_state_persistence(mut self) -> Self {
        if !self.firmware.is_openhcl() {
            panic!("TPM state persistence is only supported for OpenHCL.")
        };

        let ged = self.ged.as_mut().expect("No GED to configure TPM");

        // Disable no_persistent_secrets implies preserving TPM states
        // across boots
        ged.no_persistent_secrets = false;

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

    /// Specifies whether the UEFI frontpage app is enabled.
    ///
    /// If it is disabled, then the VM will shutdown if there are no configured
    /// boot apps.
    pub fn with_uefi_frontpage(mut self, enable_frontpage: bool) -> Self {
        match self.config.load_mode {
            LoadMode::Uefi {
                ref mut disable_frontpage,
                ..
            } => {
                *disable_frontpage = !enable_frontpage;
            }
            LoadMode::Igvm { .. } => {
                let ged = self.ged.as_mut().expect("no GED to configure DPS");
                match ged.firmware {
                    get_resources::ged::GuestFirmwareConfig::Uefi {
                        ref mut disable_frontpage,
                        ..
                    } => {
                        *disable_frontpage = !enable_frontpage;
                    }
                    _ => {
                        panic!("not a UEFI boot");
                    }
                }
            }
            _ => panic!("not a UEFI boot"),
        }
        self
    }

    /// Specifies whether the UEFI will always attempt a default boot
    pub fn with_default_boot_always_attempt(mut self, val: bool) -> Self {
        match self.config.load_mode {
            LoadMode::Uefi {
                ref mut default_boot_always_attempt,
                ..
            } => {
                *default_boot_always_attempt = val;
            }
            LoadMode::Igvm { .. } => {
                let ged = self.ged.as_mut().expect("no GED to configure DPS");
                match ged.firmware {
                    get_resources::ged::GuestFirmwareConfig::Uefi {
                        ref mut default_boot_always_attempt,
                        ..
                    } => {
                        *default_boot_always_attempt = val;
                    }
                    _ => {
                        panic!("not a UEFI boot");
                    }
                }
            }
            _ => panic!("not a UEFI boot"),
        }
        self
    }

    /// Specifies an existing VMGS file to use
    pub fn with_vmgs(mut self, vmgs_path: impl AsRef<Path>) -> Self {
        let vmgs_disk = LayeredDiskHandle {
            layers: vec![
                RamDiskLayerHandle { len: None }.into_resource().into(),
                DiskLayerHandle(
                    open_disk_type(vmgs_path.as_ref(), true).expect("failed to open VMGS file"),
                )
                .into_resource()
                .into(),
            ],
        }
        .into_resource();

        if self.firmware.is_openhcl() {
            self.ged.as_mut().unwrap().vmgs_disk = Some(vmgs_disk);
        } else {
            self.config.vmgs_disk = Some(vmgs_disk);
        }
        self
    }

    /// Add custom command line arguments to OpenHCL.
    pub fn with_openhcl_command_line(mut self, additional_cmdline: &str) -> Self {
        if !self.firmware.is_openhcl() {
            panic!("Not an OpenHCL firmware.");
        }
        let LoadMode::Igvm { cmdline, .. } = &mut self.config.load_mode else {
            unreachable!()
        };
        cmdline.push(' ');
        cmdline.push_str(additional_cmdline);
        self
    }

    /// Enable confidential filtering, even if the VM is not confidential.
    pub fn with_confidential_filtering(self) -> Self {
        if !self.firmware.is_openhcl() {
            panic!("Confidential filtering is only supported for OpenHCL");
        }
        self.with_openhcl_command_line(&format!(
            "{}=1 {}=0",
            underhill_confidentiality::OPENHCL_CONFIDENTIAL_ENV_VAR_NAME,
            underhill_confidentiality::OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME
        ))
    }

    /// Load a custom OpenHCL firmware file.
    pub fn with_custom_openhcl<A: IsOpenhclIgvm>(mut self, artifact: ResolvedArtifact<A>) -> Self {
        let LoadMode::Igvm { file, .. } = &mut self.config.load_mode else {
            panic!("Custom OpenHCL is only supported for OpenHCL firmware.")
        };
        *file = File::open(artifact)
            .expect("Failed to open custom OpenHCL file")
            .into();
        self
    }

    /// Add custom VTL 2 settings.
    // TODO: At some point we want to replace uses of this with nicer with_disk,
    // with_nic, etc. methods.
    pub fn with_custom_vtl2_settings(mut self, f: impl FnOnce(&mut Vtl2Settings)) -> Self {
        f(self
            .vtl2_settings
            .as_mut()
            .expect("Custom VTL 2 settings are only supported with OpenHCL."));
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

    /// Adds a file to the agent image.
    pub fn with_agent_file(mut self, name: &str, artifact: ResolvedArtifact) -> Self {
        self.resources.agent_image.add_file(name, artifact);
        self
    }

    /// Adds a file to the OpenHCL agent image.
    pub fn with_openhcl_agent_file(mut self, name: &str, artifact: ResolvedArtifact) -> Self {
        self.resources
            .openhcl_agent_image
            .as_mut()
            .unwrap()
            .add_file(name, artifact);
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
            (!allow).then_some(hvlite_defs::config::LateMapVtl0MemoryPolicy::InjectException);

        self
    }
}
