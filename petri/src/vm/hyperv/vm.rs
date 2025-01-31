// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides an interface for creating and managing Hyper-V VMs

use super::hvc;
use super::powershell;
use anyhow::Context;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;

/// A Hyper-V VM
pub struct HyperVVM {
    name: String,
    destroyed: bool,
    _temp_dir: TempDir,
    ps_mod: PathBuf,
}

impl HyperVVM {
    /// Create a new Hyper-V VM
    pub fn new(
        name: &str,
        generation: powershell::HyperVGeneration,
        guest_state_isolation_type: powershell::HyperVGuestStateIsolationType,
        memory: u64,
    ) -> anyhow::Result<Self> {
        let temp_dir = tempfile::tempdir()?;
        let ps_mod = temp_dir.path().join("hyperv.psm1");
        {
            let mut ps_mod_file = std::fs::File::create_new(&ps_mod)?;
            ps_mod_file
                .write_all(include_bytes!("hyperv.psm1"))
                .context("failed to write hyperv helpers powershell module")?;
        }

        let vm = Self {
            name: name.to_owned(),
            destroyed: false,
            _temp_dir: temp_dir,
            ps_mod,
        };

        // Delete the VM if it already exists
        if hvc::hvc_list()?.contains(&vm.name) {
            hvc::hvc_ensure_off(name)?;
            powershell::run_remove_vm(name)?;
        }

        powershell::run_new_vm(powershell::HyperVNewVMArgs {
            name,
            generation: Some(generation),
            guest_state_isolation_type: Some(guest_state_isolation_type),
            memory_startup_bytes: Some(memory),
            path: None,
            vhd_path: None,
        })?;

        Ok(vm)
    }

    /// Set the OpenHCL firmware file
    pub fn set_openhcl_firmware(
        &mut self,
        igvm_file: &Path,
        increase_vtl2_memory: bool,
    ) -> anyhow::Result<()> {
        powershell::run_set_openhcl_firmware(
            &self.name,
            &self.ps_mod,
            igvm_file,
            increase_vtl2_memory,
        )
    }

    /// Set the secure boot template
    pub fn set_secure_boot_template(
        &mut self,
        secure_boot_template: powershell::HyperVSecureBootTemplate,
    ) -> anyhow::Result<()> {
        powershell::run_set_vm_firmware(powershell::HyperVSetVMFirmwareArgs {
            name: &self.name,
            secure_boot_template: Some(secure_boot_template),
        })
    }

    /// Add a SCSI controller
    pub fn add_scsi_controller(&mut self) -> anyhow::Result<()> {
        powershell::run_add_vm_scsi_controller(&self.name)
    }

    /// Add a VHD
    pub fn add_vhd(
        &mut self,
        path: &Path,
        controller_location: Option<u32>,
        controller_number: Option<u32>,
    ) -> anyhow::Result<()> {
        powershell::run_add_vm_hard_disk_drive(powershell::HyperVAddVMHardDiskDriveArgs {
            name: &self.name,
            controller_location,
            controller_number,
            path: Some(path),
        })
    }

    /// Set the initial machine configuration (IMC hive file)
    pub fn set_imc(&mut self, imc_hive: &Path) -> anyhow::Result<()> {
        powershell::run_set_initial_machine_configuration(&self.name, &self.ps_mod, imc_hive)
    }

    /// Start the VM
    pub fn start(&self) -> anyhow::Result<()> {
        hvc::hvc_start(&self.name)
    }

    /// Wait for the VM to turn off
    pub fn wait_for_power_off(&self) -> anyhow::Result<()> {
        hvc::hvc_wait_for_power_off(&self.name)
    }

    /// Remove the VM
    pub fn remove(mut self) -> anyhow::Result<()> {
        self.remove_inner()
    }

    fn remove_inner(&mut self) -> anyhow::Result<()> {
        if !self.destroyed {
            hvc::hvc_ensure_off(&self.name)?;
            powershell::run_remove_vm(&self.name)?;
            self.destroyed = true;
        }

        Ok(())
    }
}

impl Drop for HyperVVM {
    fn drop(&mut self) {
        let _ = self.remove_inner();
    }
}
