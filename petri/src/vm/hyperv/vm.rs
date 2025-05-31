// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides an interface for creating and managing Hyper-V VMs

use super::hvc;
use super::hvc::VmState;
use super::powershell;
use crate::PetriLogFile;
use anyhow::Context;
use get_resources::ged::FirmwareEvent;
use guid::Guid;
use jiff::Timestamp;
use jiff::ToSpan;
use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
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
    expected_boot_event: Option<FirmwareEvent>,
    driver: DefaultDriver,
}

impl HyperVVM {
    /// Create a new Hyper-V VM
    pub fn new(
        name: &str,
        generation: powershell::HyperVGeneration,
        guest_state_isolation_type: powershell::HyperVGuestStateIsolationType,
        memory: u64,
        log_file: PetriLogFile,
        expected_boot_event: Option<FirmwareEvent>,
        driver: DefaultDriver,
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
        let cleanup = |vmid: &Guid| -> anyhow::Result<()> {
            hvc::hvc_ensure_off(vmid)?;
            powershell::run_remove_vm(vmid)
        };

        if let Ok(vmids) = powershell::vm_id_from_name(&name) {
            for vmid in vmids {
                match cleanup(&vmid) {
                    Ok(_) => {
                        tracing::info!("Successfully cleaned up VM from previous test run ({vmid})")
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to clean up VM from previous test run ({vmid}): {e:?}"
                        )
                    }
                }
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

        // Instantiate this now so that its drop runs if there's a failure
        // below.
        let this = Self {
            name,
            vmid,
            destroyed: false,
            _temp_dir: temp_dir,
            ps_mod,
            create_time,
            log_file,
            expected_boot_event,
            driver,
        };

        // Remove the default network adapter
        powershell::run_remove_vm_network_adapter(&vmid)
            .context("remove default network adapter")?;

        // Remove the default SCSI controller
        powershell::run_remove_vm_scsi_controller(&vmid, 0)
            .context("remove default SCSI controller")?;

        // Disable dynamic memory
        powershell::run_set_vm_memory(
            &vmid,
            &powershell::HyperVSetVMMemoryArgs {
                dynamic_memory_enabled: Some(false),
                ..Default::default()
            },
        )?;

        Ok(this)
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

    /// Waits for an event emitted by the firmware about its boot status, and
    /// verifies that it is the expected success value.
    pub async fn wait_for_successful_boot_event(&mut self) -> anyhow::Result<()> {
        if let Some(expected_boot_event) = self.expected_boot_event {
            self.wait_for(Self::boot_event, Some(expected_boot_event), 240.seconds())
                .await
                .context("wait_for_successful_boot_event")?;
        } else {
            tracing::warn!("Configured firmware does not emit a boot event, skipping");
        }

        Ok(())
    }

    /// Waits for an event emitted by the firmware about its boot status, and
    /// returns that status.
    pub async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent> {
        self.wait_for_some(Self::boot_event, 240.seconds()).await
    }

    fn boot_event(&self) -> anyhow::Result<Option<FirmwareEvent>> {
        let events = powershell::hyperv_boot_events(&self.vmid, &self.create_time)?;

        if events.len() > 1 {
            anyhow::bail!("Got more than one boot event");
        }

        events
            .first()
            .map(|e| match e.id {
                powershell::EVENT_ID_BOOT_SUCCESS => Ok(FirmwareEvent::BootSuccess),
                powershell::EVENT_ID_BOOT_FAILURE => Ok(FirmwareEvent::BootFailed),
                powershell::EVENT_ID_NO_BOOT_DEVICE => Ok(FirmwareEvent::NoBootDevice),
                powershell::EVENT_ID_BOOT_ATTEMPT => Ok(FirmwareEvent::BootAttempt),
                id => anyhow::bail!("Unexpected event id: {id}"),
            })
            .transpose()
    }

    /// Set the VM processor topology.
    pub fn set_processor(
        &mut self,
        args: &powershell::HyperVSetVMProcessorArgs,
    ) -> anyhow::Result<()> {
        powershell::run_set_vm_processor(&self.vmid, args)
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
    pub fn add_scsi_controller(&mut self, target_vtl: u32) -> anyhow::Result<u32> {
        let controller_number = powershell::run_add_vm_scsi_controller(&self.vmid)?;
        if target_vtl != 0 {
            powershell::run_set_vm_scsi_controller_target_vtl(
                &self.ps_mod,
                &self.vmid,
                controller_number,
                target_vtl,
            )?;
        }
        Ok(controller_number)
    }

    /// Add a VHD
    pub fn add_vhd(
        &mut self,
        path: &Path,
        controller_type: powershell::ControllerType,
        controller_location: Option<u32>,
        controller_number: Option<u32>,
    ) -> anyhow::Result<()> {
        powershell::run_add_vm_hard_disk_drive(powershell::HyperVAddVMHardDiskDriveArgs {
            vmid: &self.vmid,
            controller_type,
            controller_location,
            controller_number,
            path: Some(path),
        })
    }

    /// Set the initial machine configuration (IMC hive file)
    pub fn set_imc(&mut self, imc_hive: &Path) -> anyhow::Result<()> {
        powershell::run_set_initial_machine_configuration(&self.vmid, &self.ps_mod, imc_hive)
    }

    fn state(&self) -> anyhow::Result<VmState> {
        hvc::hvc_state(&self.vmid)
    }

    fn check_state(&self, expected: VmState) -> anyhow::Result<()> {
        let state = self.state()?;
        if state != expected {
            anyhow::bail!("unexpected VM state {state:?}, should be {expected:?}");
        }
        Ok(())
    }

    /// Start the VM
    pub async fn start(&self) -> anyhow::Result<()> {
        self.check_state(VmState::Off)?;
        hvc::hvc_start(&self.vmid)?;
        Ok(())
    }

    /// Attempt to gracefully shut down the VM
    pub async fn stop(&self) -> anyhow::Result<()> {
        self.wait_for_shutdown_ic().await?;
        self.check_state(VmState::Running)?;
        hvc::hvc_stop(&self.vmid)?;
        Ok(())
    }

    /// Attempt to gracefully restart the VM
    pub async fn restart(&self) -> anyhow::Result<()> {
        self.wait_for_shutdown_ic().await?;
        self.check_state(VmState::Running)?;
        hvc::hvc_restart(&self.vmid)?;
        Ok(())
    }

    /// Kill the VM
    pub fn kill(&self) -> anyhow::Result<()> {
        hvc::hvc_kill(&self.vmid).context("hvc_kill")
    }

    /// Issue a hard reset to the VM
    pub fn reset(&self) -> anyhow::Result<()> {
        hvc::hvc_reset(&self.vmid).context("hvc_reset")
    }

    /// Enable serial output and return the named pipe path
    pub fn set_vm_com_port(&mut self, port: u8) -> anyhow::Result<String> {
        let pipe_path = format!(r#"\\.\pipe\{}-{}"#, self.vmid, port);
        powershell::run_set_vm_com_port(&self.vmid, port, Path::new(&pipe_path))?;
        Ok(pipe_path)
    }

    /// Wait for the VM to stop
    pub async fn wait_for_halt(&self) -> anyhow::Result<()> {
        self.wait_for_state(VmState::Off).await
    }

    async fn wait_for_state(&self, target: VmState) -> anyhow::Result<()> {
        self.wait_for(Self::state, target, 240.seconds())
            .await
            .context("wait_for_state")
    }

    /// Wait for the VM shutdown ic
    async fn wait_for_shutdown_ic(&self) -> anyhow::Result<()> {
        self.wait_for(
            Self::shutdown_ic_status,
            powershell::VmShutdownIcStatus::Ok,
            240.seconds(),
        )
        .await
        .context("wait_for_shutdown_ic")
    }

    fn shutdown_ic_status(&self) -> anyhow::Result<powershell::VmShutdownIcStatus> {
        powershell::vm_shutdown_ic_status(&self.vmid)
    }

    // TODO: replace timeouts throughout the hyper-v petri infrastructure
    // with a watchdog
    async fn wait_for<T: std::fmt::Debug + PartialEq>(
        &self,
        f: fn(&Self) -> anyhow::Result<T>,
        target: T,
        timeout: jiff::Span,
    ) -> anyhow::Result<()> {
        let start = Timestamp::now();
        loop {
            let state = f(self)?;
            if state == target {
                break;
            }
            if timeout.compare(Timestamp::now() - start)? == std::cmp::Ordering::Less {
                anyhow::bail!("timed out waiting for {target:?}. current: {state:?}");
            }
            PolledTimer::new(&self.driver)
                .sleep(Duration::from_secs(1))
                .await;
        }

        Ok(())
    }

    async fn wait_for_some<T: std::fmt::Debug + PartialEq>(
        &self,
        f: fn(&Self) -> anyhow::Result<Option<T>>,
        timeout: jiff::Span,
    ) -> anyhow::Result<T> {
        let start = Timestamp::now();
        loop {
            let state = f(self)?;
            if let Some(state) = state {
                return Ok(state);
            }
            if timeout.compare(Timestamp::now() - start)? == std::cmp::Ordering::Less {
                anyhow::bail!("timed out waiting for Some");
            }
            PolledTimer::new(&self.driver)
                .sleep(Duration::from_secs(1))
                .await;
        }
    }

    /// Remove the VM
    pub fn remove(mut self) -> anyhow::Result<()> {
        self.remove_inner()
    }

    fn remove_inner(&mut self) -> anyhow::Result<()> {
        if !self.destroyed {
            let res_off = hvc::hvc_ensure_off(&self.vmid);
            let res_remove = powershell::run_remove_vm(&self.vmid);

            self.flush_logs()?;

            res_off?;
            res_remove?;
            self.destroyed = true;
        }

        Ok(())
    }

    /// Sets the VM firmware  command line.
    pub fn set_vm_firmware_command_line(&self, openhcl_command_line: &str) -> anyhow::Result<()> {
        powershell::run_set_vm_command_line(&self.vmid, &self.ps_mod, openhcl_command_line)
    }

    /// Enable VMBusRelay
    pub fn set_vmbus_relay(&self, enable: bool) {
        if enable {
            powershell::run_enable_vmbus_relay(&self.vmid);
        } else {
            powershell::run_disable_vmbus_relay(&self.vmid);
        }
    }
}

impl Drop for HyperVVM {
    fn drop(&mut self) {
        if std::env::var("PETRI_PRESERVE_VM")
            .ok()
            .is_none_or(|v| v.is_empty() || v == "0")
        {
            let _ = self.remove_inner();
        }
    }
}
