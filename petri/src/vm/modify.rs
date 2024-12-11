// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers to modify a [`PetriVmConfig`] from its defaults.

use crate::PetriVmConfig;
use chipset_resources::battery::BatteryDeviceHandleX64;
use chipset_resources::battery::HostBatteryUpdate;
use fs_err::File;
use hvlite_defs::config::Config;
use hvlite_defs::config::LoadMode;
use hvlite_defs::config::Vtl2BaseAddressType;
use petri_artifacts_common::tags::IsOpenhclIgvm;
use petri_artifacts_core::ArtifactHandle;
use tpm_resources::TpmDeviceHandle;
use tpm_resources::TpmRegisterLayout;
use vm_resource::IntoResource;
use vmcore::non_volatile_store::resources::EphemeralNonVolatileStoreHandle;
use vmotherboard::ChipsetDeviceHandle;
use vtl2_settings_proto::Vtl2Settings;

impl PetriVmConfig {
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
                }
                .into_resource(),
            });
            if let LoadMode::Uefi { enable_tpm, .. } = &mut self.config.load_mode {
                *enable_tpm = true;
            }
        }

        self
    }

    /// Set the VM to use a single processor.
    /// This is useful mainly for heavier OpenHCL tests, as our WHP emulation
    /// layer is rather slow when dealing with cross-cpu communication.
    pub fn with_single_processor(mut self) -> Self {
        self.config.processor_topology.proc_count = 1;
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
    pub fn with_custom_openhcl<A: IsOpenhclIgvm>(mut self, artifact: ArtifactHandle<A>) -> Self {
        let LoadMode::Igvm { file, .. } = &mut self.config.load_mode else {
            panic!("Custom OpenHCL is only supported for OpenHCL firmware.")
        };
        *file = File::open(self.resources.resolver.resolve(artifact))
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
}
