// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides an interface for creating and managing Hyper-V VMs

use super::hvc;
use super::powershell;
use crate::PetriLogFile;
use anyhow::Context;
use guid::Guid;
use jiff::Timestamp;
use pal_async::DefaultDriver;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;
use tracing::Level;

/// A Hyper-V VM
pub struct HyperVVM {
    name: String,
    vmid: Guid,
    destroyed: bool,
    _temp_dir: TempDir,
    ps_mod: PathBuf,
    create_time: Timestamp,
    log_file: PetriLogFile,
}

impl HyperVVM {
    /// Create a new Hyper-V VM
    pub fn new(
        name: &str,
        generation: powershell::HyperVGeneration,
        guest_state_isolation_type: powershell::HyperVGuestStateIsolationType,
        memory: u64,
        log_file: PetriLogFile,
    ) -> anyhow::Result<Self> {
        let create_time = Timestamp::now();
        let name = name.to_owned();
        let temp_dir = tempfile::tempdir()?;
        let ps_mod = temp_dir.path().join("hyperv.psm1");
        {
            let mut ps_mod_file = std::fs::File::create_new(&ps_mod)?;
            ps_mod_file
                .write_all(include_bytes!("hyperv.psm1"))
                .context("failed to write hyperv helpers powershell module")?;
        }

        // Delete the VM if it already exists
        if let Ok(vmids) = powershell::vm_id_from_name(&name) {
            for vmid in vmids {
                hvc::hvc_ensure_off(&vmid)?;
                powershell::run_remove_vm(&vmid)?;
            }
        }

        let vmid = powershell::run_new_vm(powershell::HyperVNewVMArgs {
            name: &name,
            generation: Some(generation),
            guest_state_isolation_type: Some(guest_state_isolation_type),
            memory_startup_bytes: Some(memory),
            path: None,
            vhd_path: None,
        })?;

        tracing::info!(name, vmid = vmid.to_string(), "Created Hyper-V VM");

        Ok(Self {
            name,
            vmid,
            destroyed: false,
            _temp_dir: temp_dir,
            ps_mod,
            create_time,
            log_file,
        })
    }

    /// Get the name of the VM
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the VmId Guid of the VM
    pub fn vmid(&self) -> &Guid {
        &self.vmid
    }

    /// Get Hyper-V logs and write them to the log file
    pub fn flush_logs(&self) -> anyhow::Result<()> {
        for event in powershell::hyperv_event_logs(&self.vmid, &self.create_time)? {
            self.log_file.write_entry_fmt(
                Some(event.time_created),
                match event.level {
                    1 | 2 => Level::ERROR,
                    3 => Level::WARN,
                    5 => Level::TRACE,
                    _ => Level::INFO,
                },
                format_args!(
                    "[{}] {}: ({}, {}) {}",
                    event.time_created, event.provider_name, event.level, event.id, event.message,
                ),
            );
        }
        Ok(())
    }

    /// Set the OpenHCL firmware file
    pub fn set_openhcl_firmware(
        &mut self,
        igvm_file: &Path,
        increase_vtl2_memory: bool,
    ) -> anyhow::Result<()> {
        powershell::run_set_openhcl_firmware(
            &self.vmid,
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
            vmid: &self.vmid,
            secure_boot_template: Some(secure_boot_template),
        })
    }

    /// Add a SCSI controller
    pub fn add_scsi_controller(&mut self) -> anyhow::Result<()> {
        powershell::run_add_vm_scsi_controller(&self.vmid)
    }

    /// Add a VHD
    pub fn add_vhd(
        &mut self,
        path: &Path,
        controller_location: Option<u32>,
        controller_number: Option<u32>,
    ) -> anyhow::Result<()> {
        powershell::run_add_vm_hard_disk_drive(powershell::HyperVAddVMHardDiskDriveArgs {
            vmid: &self.vmid,
            controller_location,
            controller_number,
            path: Some(path),
        })
    }

    /// Set the initial machine configuration (IMC hive file)
    pub fn set_imc(&mut self, imc_hive: &Path) -> anyhow::Result<()> {
        powershell::run_set_initial_machine_configuration(&self.vmid, &self.ps_mod, imc_hive)
    }

    /// Start the VM
    pub fn start(&self) -> anyhow::Result<()> {
        hvc::hvc_start(&self.vmid)
    }

    /// Enable serial output and return the named pipe path
    pub fn set_vm_com_port(&mut self, port: u8) -> anyhow::Result<String> {
        let pipe_path = format!(r#"\\.\pipe\{}-{}"#, self.vmid, port);
        powershell::run_set_vm_com_port(&self.vmid, port, Path::new(&pipe_path))?;
        Ok(pipe_path)
    }

    /// Wait for the VM to turn off
    pub async fn wait_for_power_off(&self, driver: &DefaultDriver) -> anyhow::Result<()> {
        hvc::hvc_wait_for_power_off(driver, &self.vmid).await
    }

    /// Remove the VM
    pub fn remove(mut self) -> anyhow::Result<()> {
        self.remove_inner()
    }

    fn remove_inner(&mut self) -> anyhow::Result<()> {
        if !self.destroyed {
            hvc::hvc_ensure_off(&self.vmid)?;
            powershell::run_remove_vm(&self.vmid)?;
            self.flush_logs()?;
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
