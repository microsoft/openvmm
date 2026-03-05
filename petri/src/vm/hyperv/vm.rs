// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides an interface for creating and managing Hyper-V VMs

use super::hvc;
use super::hvc::VmState;
use super::powershell;
use crate::CommandError;
use crate::OpenHclServicingFlags;
use crate::PetriHaltReason;
use crate::PetriLogFile;
use crate::PetriLogSource;
use crate::PetriVmFramebufferAccess;
use crate::VmScreenshotMeta;
use anyhow::Context;
use async_trait::async_trait;
use get_resources::ged::FirmwareEvent;
use guid::Guid;
use jiff::Timestamp;
use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use tempfile::TempDir;
use tracing::Level;

/// A Hyper-V VM
pub struct HyperVVM {
    // properties
    vmid: Guid,
    name: String,
    create_time: Timestamp,
    is_isolated: bool,

    // resources
    temp_dir: Arc<TempDir>,
    ps_mod: PathBuf,
    // TODO: use a trait interface here
    log_file: PetriLogFile,
    logger: PetriLogSource,
    driver: DefaultDriver,

    // state
    destroyed: bool,
    last_start_time: Option<Timestamp>,
    last_log_flushed: Option<Timestamp>,
}

impl HyperVVM {
    /// Create a new Hyper-V VM
    pub async fn new(
        name: &str,
        generation: powershell::HyperVGeneration,
        guest_state_isolation_type: powershell::HyperVGuestStateIsolationType,
        memory: u64,
        vmgs_path: Option<&Path>,
        logger: PetriLogSource,
        driver: DefaultDriver,
    ) -> anyhow::Result<Self> {
        let log_file = logger.log_file("hyperv")?;
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

        // Used to ignore `hvc restart` error on CVMs
        let is_isolated = {
            use powershell::HyperVGuestStateIsolationType as IsolationType;
            matches!(
                guest_state_isolation_type,
                IsolationType::Snp | IsolationType::Tdx | IsolationType::Vbs
            )
        };

        // Delete the VM if it already exists
        let cleanup = async |vmid: &Guid| -> anyhow::Result<()> {
            hvc::hvc_ensure_off(vmid).await?;
            powershell::run_remove_vm(vmid).await
        };

        if let Ok(vmids) = powershell::vm_id_from_name(&name).await {
            for vmid in vmids {
                match cleanup(&vmid).await {
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
            source_guest_state_path: vmgs_path,
        })
        .await?;

        tracing::info!(name, vmid = vmid.to_string(), "Created Hyper-V VM");

        // Instantiate this now so that its drop runs if there's a failure
        // below.
        let this = Self {
            vmid,
            name,
            create_time,
            is_isolated,
            temp_dir: Arc::new(temp_dir),
            ps_mod,
            log_file,
            logger,
            driver,
            destroyed: false,
            last_start_time: None,
            last_log_flushed: None,
        };

        // Remove the default network adapter
        powershell::run_remove_vm_network_adapter(&vmid)
            .await
            .context("remove default network adapter")?;

        // Remove the default SCSI controller
        powershell::run_remove_vm_scsi_controller(&vmid, 0)
            .await
            .context("remove default SCSI controller")?;

        // Disable dynamic memory
        powershell::run_set_vm_memory(
            &vmid,
            &powershell::HyperVSetVMMemoryArgs {
                dynamic_memory_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;

        // Disable secure boot for generation 2 VMs
        if generation == powershell::HyperVGeneration::Two {
            powershell::run_set_vm_firmware(powershell::HyperVSetVMFirmwareArgs {
                vmid: &vmid,
                secure_boot_enabled: Some(false),
                secure_boot_template: None,
            })
            .await?;
        }

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
    pub async fn flush_logs(&mut self) -> anyhow::Result<()> {
        let start_time = self.last_log_flushed.as_ref().unwrap_or(&self.create_time);
        for event in powershell::hyperv_event_logs(Some(&self.vmid), start_time).await? {
            self.log_winevent(&event);
            if self.last_log_flushed.is_none_or(|t| t < event.time_created) {
                // add 1ms to avoid duplicate log entries
                self.last_log_flushed =
                    Some(event.time_created.checked_add(Duration::from_millis(1))?);
            }
        }
        Ok(())
    }

    fn log_winevent(&self, event: &powershell::WinEvent) {
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

        const HYPERV_CRASHDUMP_PROVIDER: &str = "Microsoft-Windows-Hyper-V-CrashDump";
        const CRASH_DUMP_WRITTEN_EVENT_ID: u32 = 40001;

        // If we see an event indicating a crash dump was written, parse the message
        // to find the dump file path and attach it.
        if event.provider_name == HYPERV_CRASHDUMP_PROVIDER
            && event.id == CRASH_DUMP_WRITTEN_EVENT_ID
        {
            let Some(path_start) = event.message.find("C:\\") else {
                tracing::warn!("could not find crash dump path in crash event message");
                return;
            };
            // Messages end with a period, exclude it
            let path = Path::new(&event.message[path_start..event.message.len() - 1]);
            let filename = path.file_name().and_then(|x| x.to_str()).unwrap();
            if let Err(e) = self.logger.copy_attachment(filename, path) {
                tracing::warn!(
                    error = e.as_ref() as &dyn std::error::Error,
                    "failed to copy hyper-v crash dump file"
                );
            }
            tracing::info!("copied hyper-v crash dump file {filename}");
        }
    }

    /// Waits for an event emitted by the firmware about its boot status, and
    /// returns that status.
    pub async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent> {
        self.wait_for_off_or_internal(Self::boot_event).await
    }

    async fn boot_event(&self) -> anyhow::Result<Option<FirmwareEvent>> {
        let events = powershell::hyperv_boot_events(
            &self.vmid,
            self.last_start_time.as_ref().unwrap_or(&self.create_time),
        )
        .await?;

        if events.len() > 1 {
            anyhow::bail!("Got more than one boot event");
        }

        events
            .first()
            .map(|e| match e.id {
                powershell::MSVM_BOOT_RESULTS_SUCCESS => Ok(FirmwareEvent::BootSuccess),
                powershell::MSVM_BOOT_RESULTS_FAILURE => Ok(FirmwareEvent::BootFailed),
                powershell::MSVM_BOOT_RESULTS_FAILURE_NO_DEVICES => Ok(FirmwareEvent::NoBootDevice),
                powershell::MSVM_BOOT_RESULTS_ATTEMPT => Ok(FirmwareEvent::BootAttempt),
                powershell::MSVM_BOOT_RESULTS_FAILURE_SECURE_BOOT_FAILURES => {
                    Ok(FirmwareEvent::BootFailed)
                }
                id => anyhow::bail!("Unexpected event id: {id}"),
            })
            .transpose()
    }

    /// Set the VM processor topology.
    pub async fn set_processor(
        &mut self,
        args: &powershell::HyperVSetVMProcessorArgs,
    ) -> anyhow::Result<()> {
        powershell::run_set_vm_processor(&self.vmid, args).await
    }

    /// Set the OpenHCL firmware file
    pub async fn set_openhcl_firmware(
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
        .await
    }

    /// Configure secure boot
    pub async fn set_secure_boot(
        &mut self,
        secure_boot_enabled: bool,
        secure_boot_template: Option<powershell::HyperVSecureBootTemplate>,
    ) -> anyhow::Result<()> {
        powershell::run_set_vm_firmware(powershell::HyperVSetVMFirmwareArgs {
            vmid: &self.vmid,
            secure_boot_enabled: Some(secure_boot_enabled),
            secure_boot_template,
        })
        .await
    }

    /// Add a SCSI controller
    pub async fn add_scsi_controller(
        &mut self,
        vsid: &Guid,
        target_vtl: u32,
    ) -> anyhow::Result<()> {
        powershell::run_add_vm_scsi_controller_with_id(&self.ps_mod, &self.vmid, vsid, target_vtl)
            .await
    }

    /// Add a drive to the scsi controller
    pub async fn set_drive_scsi(
        &mut self,
        controller_vsid: &Guid,
        controller_location: u8,
        path: Option<&Path>,
        dvd: bool,
        allow_modify_existing: bool,
    ) -> anyhow::Result<()> {
        powershell::run_set_vm_drive_scsi(
            &self.ps_mod,
            &self.vmid,
            controller_vsid,
            controller_location,
            path,
            dvd,
            allow_modify_existing,
        )
        .await
    }

    /// Add a drive to the ide controller
    pub async fn set_drive_ide(
        &mut self,
        controller_number: u32,
        controller_location: u8,
        path: Option<&Path>,
        dvd: bool,
        allow_modify_existing: bool,
    ) -> anyhow::Result<()> {
        powershell::run_set_vm_drive_ide(
            &self.ps_mod,
            &self.vmid,
            controller_number,
            controller_location,
            path,
            dvd,
            allow_modify_existing,
        )
        .await
    }

    /// Set the initial machine configuration (IMC hive file)
    pub async fn set_imc(&mut self, imc_hive: &Path) -> anyhow::Result<()> {
        powershell::run_set_initial_machine_configuration(&self.vmid, &self.ps_mod, imc_hive).await
    }

    async fn state(&self) -> anyhow::Result<VmState> {
        hvc::hvc_state(&self.vmid).await
    }

    async fn check_state(&self, expected: VmState) -> anyhow::Result<()> {
        let state = self.state().await?;
        if state != expected {
            anyhow::bail!("unexpected VM state {state:?}, should be {expected:?}");
        }
        Ok(())
    }

    /// Start the VM
    pub async fn start(&self) -> anyhow::Result<()> {
        self.check_state(VmState::Off).await?;
        hvc::hvc_start(&self.vmid).await?;
        Ok(())
    }

    /// Attempt to gracefully shut down the VM
    pub async fn stop(&self) -> anyhow::Result<()> {
        self.check_shutdown_ic().await?;
        self.check_state(VmState::Running).await?;
        hvc::hvc_stop(&self.vmid).await?;
        Ok(())
    }

    /// Attempt to gracefully restart the VM
    pub async fn restart(&self) -> anyhow::Result<()> {
        self.check_shutdown_ic().await?;
        self.check_state(VmState::Running).await?;
        let res = hvc::hvc_restart(&self.vmid).await;

        const KNOWN_HVC_RESTART_ERROR: &str = "The VM is in the wrong state for this operation.";
        if self.is_isolated
            && matches!(&res, Err(CommandError::Command(code, msg))
            if matches!(code.code(), Some(1))
            && msg.trim() == KNOWN_HVC_RESTART_ERROR)
        {
            // Ignore this error when isolated, since it seems to work anyways.
        } else {
            res?;
        }

        Ok(())
    }

    /// Kill the VM
    pub async fn kill(&self) -> anyhow::Result<()> {
        hvc::hvc_kill(&self.vmid).await.context("hvc_kill")
    }

    /// Issue a hard reset to the VM
    pub async fn reset(&self) -> anyhow::Result<()> {
        hvc::hvc_reset(&self.vmid).await.context("hvc_reset")
    }

    /// Enable serial output and return the named pipe path
    pub async fn set_vm_com_port(&mut self, port: u8) -> anyhow::Result<String> {
        let pipe_path = format!(r#"\\.\pipe\{}-{}"#, self.vmid, port);
        powershell::run_set_vm_com_port(&self.vmid, port, Path::new(&pipe_path)).await?;
        Ok(pipe_path)
    }

    /// Wait for the VM to stop
    pub async fn wait_for_halt(&mut self, _allow_reset: bool) -> anyhow::Result<PetriHaltReason> {
        // Allow CVMs some time for the VM to be off after reset.
        const CVM_ALLOWED_OFF_TIME: Duration = Duration::from_secs(15);

        let (halt_reason, timestamp) = self.wait_for_off_or_internal(Self::halt_event).await?;

        if halt_reason == PetriHaltReason::Reset {
            // add 1ms to avoid getting the same event again
            self.last_start_time = Some(timestamp.checked_add(Duration::from_millis(1))?);

            // wait for the CVM to start again
            if self.is_isolated {
                let mut timer = PolledTimer::new(&self.driver);
                loop {
                    match self.state().await? {
                        VmState::Off | VmState::Stopping | VmState::Resetting => {}
                        VmState::Running | VmState::Starting => break,
                        VmState::Saved
                        | VmState::Paused
                        | VmState::Saving
                        | VmState::Pausing
                        | VmState::Resuming => anyhow::bail!("Unexpected vm state"),
                    }

                    if Timestamp::now().duration_since(timestamp).unsigned_abs()
                        > CVM_ALLOWED_OFF_TIME
                    {
                        anyhow::bail!("VM did not start after reset in the required time");
                    }

                    timer.sleep(Duration::from_secs(1)).await;
                }
            }
        }
        Ok(halt_reason)
    }

    async fn halt_event(&self) -> anyhow::Result<Option<(PetriHaltReason, Timestamp)>> {
        let events = powershell::hyperv_halt_events(
            &self.vmid,
            self.last_start_time.as_ref().unwrap_or(&self.create_time),
        )
        .await?;

        if events.len() > 1 {
            anyhow::bail!("Got more than one halt event");
        }
        let event = events.first();

        event
            .map(|e| {
                Ok((
                    match e.id {
                        powershell::MSVM_HOST_STOP_SUCCESS
                        | powershell::MSVM_HOST_SHUTDOWN_SUCCESS
                        | powershell::MSVM_GUEST_SHUTDOWN_SUCCESS => PetriHaltReason::PowerOff,
                        powershell::MSVM_HOST_RESET_SUCCESS
                        | powershell::MSVM_GUEST_RESET_SUCCESS
                        | powershell::MSVM_STOP_FOR_GUEST_RESET_SUCCESS => PetriHaltReason::Reset,
                        powershell::MSVM_GUEST_HIBERNATE_SUCCESS => PetriHaltReason::Hibernate,
                        powershell::MSVM_TRIPLE_FAULT_GENERAL_ERROR
                        | powershell::MSVM_TRIPLE_FAULT_UNSUPPORTED_FEATURE_ERROR
                        | powershell::MSVM_TRIPLE_FAULT_INVALID_VP_REGISTER_ERROR
                        | powershell::MSVM_TRIPLE_FAULT_UNRECOVERABLE_EXCEPTION_ERROR => {
                            PetriHaltReason::TripleFault
                        }
                        powershell::MSVM_STOP_CRITICAL_SUCCESS
                        | powershell::MSVM_VMMS_VM_TERMINATE_ERROR => PetriHaltReason::Other,
                        id => anyhow::bail!("Unexpected event id: {id}"),
                    },
                    e.time_created,
                ))
            })
            .transpose()
    }

    /// Wait for the VM shutdown ic
    pub async fn wait_for_enlightened_shutdown_ready(&mut self) -> anyhow::Result<()> {
        self.wait_for_off_or_internal(async move |s| {
            Ok((s.shutdown_ic_status().await? == powershell::VmShutdownIcStatus::Ok).then_some(()))
        })
        .await
        .context("wait_for_enlightened_shutdown_ready")
    }

    async fn shutdown_ic_status(&self) -> anyhow::Result<powershell::VmShutdownIcStatus> {
        powershell::vm_shutdown_ic_status(&self.vmid).await
    }

    async fn check_shutdown_ic(&self) -> anyhow::Result<()> {
        let status = self.shutdown_ic_status().await?;
        if status != powershell::VmShutdownIcStatus::Ok {
            anyhow::bail!("unexpected shutdown ic status {status:?}, should be Ok");
        }
        Ok(())
    }

    pub(crate) async fn wait_for_off_or_internal<T>(
        &mut self,
        f: impl AsyncFn(&Self) -> anyhow::Result<Option<T>>,
    ) -> anyhow::Result<T> {
        // flush the logs every time we start waiting for something in case
        // they don't get flushed when the VM is destroyed.
        // TODO: run this periodically in a task.
        self.flush_logs().await?;

        // Even if the VM is rebooting or otherwise transitioning power states
        // it should never be considered fully off. The only exception is if we
        // are waiting for the VM to turn off, and we haven't detected the halt
        // event yet. To avoid this race condition, allow for one more attempt
        // a second after the VM turns off.
        let mut last_off = false;

        let mut timer = PolledTimer::new(&self.driver);
        loop {
            if let Some(output) = f(self).await? {
                return Ok(output);
            }

            let off = self.state().await? == VmState::Off;
            if last_off && off {
                if let Some((halt_event, _)) = self.halt_event().await? {
                    anyhow::bail!("Unexpected halt event: {halt_event:?}");
                } else {
                    anyhow::bail!(
                        "The VM is no longer running, but no known halt event was received."
                    );
                }
            }
            last_off = off;

            timer.sleep(Duration::from_secs(1)).await;
        }
    }

    /// Remove the VM
    pub async fn remove(mut self) -> anyhow::Result<()> {
        self.remove_inner().await
    }

    async fn remove_inner(&mut self) -> anyhow::Result<()> {
        if !self.destroyed {
            let res_off = hvc::hvc_ensure_off(&self.vmid)
                .await
                .inspect_err(|e| tracing::error!("failed to stop vm: {e:?}"));

            // Wait for logs to propagate and any crash dumps to be written
            std::thread::sleep(Duration::from_secs(1));
            // Flush logs before we remove the VM so we can capture any
            // interesting files before they get deleted.
            let res_flush = self
                .flush_logs()
                .await
                .inspect_err(|e| tracing::error!("failed to flush logs: {e:?}"));
            let res_remove = powershell::run_remove_vm(&self.vmid)
                .await
                .inspect_err(|e| tracing::error!("failed to remove vm: {e:?}"));

            res_off?;
            res_remove?;
            self.destroyed = true;
            res_flush?;
        }

        Ok(())
    }

    /// Sets the VM firmware command line.
    pub async fn set_vm_firmware_command_line(
        &self,
        openhcl_command_line: impl AsRef<str>,
    ) -> anyhow::Result<()> {
        powershell::run_set_vm_command_line(&self.vmid, &self.ps_mod, openhcl_command_line).await
    }

    /// Enable VMBusRelay
    pub async fn set_vmbus_redirect(&self, enable: bool) -> anyhow::Result<()> {
        powershell::run_set_vmbus_redirect(&self.vmid, &self.ps_mod, enable).await
    }

    /// Perform an OpenHCL servicing operation.
    pub async fn restart_openhcl(&self, flags: OpenHclServicingFlags) -> anyhow::Result<()> {
        powershell::run_restart_openhcl(&self.vmid, &self.ps_mod, flags).await
    }

    /// Get the Framebuffer access
    pub fn get_framebuffer_access(&self) -> HyperVFramebufferAccess {
        HyperVFramebufferAccess {
            vmid: self.vmid,
            temp_dir: Arc::downgrade(&self.temp_dir),
            temp_bin_path: self.temp_dir.path().join("screenshot.bin"),
            ps_mod: self.ps_mod.clone(),
        }
    }

    /// Get the VM's guest state file
    pub async fn get_guest_state_file(&self) -> anyhow::Result<PathBuf> {
        powershell::run_get_guest_state_file(&self.vmid, &self.ps_mod).await
    }

    /// Set the VTL2 settings in the `Base` namespace (fixed settings, storage
    /// settings, etc).
    pub async fn set_base_vtl2_settings(
        &self,
        settings: &vtl2_settings_proto::Vtl2Settings,
    ) -> anyhow::Result<()> {
        powershell::run_set_base_vtl2_settings(&self.vmid, &self.ps_mod, settings).await
    }

    /// Set GuestStateIsolationMode
    pub async fn set_guest_state_isolation_mode(
        &self,
        mode: powershell::HyperVGuestStateIsolationMode,
    ) -> anyhow::Result<()> {
        powershell::run_set_guest_state_isolation_mode(&self.vmid, &self.ps_mod, mode).await
    }

    /// Enable the TPM
    pub async fn enable_tpm(&self) -> anyhow::Result<()> {
        powershell::run_enable_vmtpm(&self.vmid).await
    }

    /// Disable the TPM
    pub async fn disable_tpm(&self) -> anyhow::Result<()> {
        powershell::run_disable_vmtpm(&self.vmid).await
    }
}

impl Drop for HyperVVM {
    fn drop(&mut self) {
        if std::env::var("PETRI_PRESERVE_VM")
            .ok()
            .is_none_or(|v| v.is_empty() || v == "0")
        {
            let _ = futures::executor::block_on(self.remove_inner());
        }
    }
}

/// Interface to the Hyper-V framebuffer for taking screenshots
pub struct HyperVFramebufferAccess {
    vmid: Guid,
    temp_dir: Weak<TempDir>,
    temp_bin_path: PathBuf,
    ps_mod: PathBuf,
}

#[async_trait]
impl PetriVmFramebufferAccess for HyperVFramebufferAccess {
    async fn screenshot(
        &mut self,
        image: &mut Vec<u8>,
    ) -> anyhow::Result<Option<VmScreenshotMeta>> {
        // make sure that the temp directory containing the powershell module
        // and temp bin file still exists.
        self.temp_dir.upgrade().context("VM no longer exists")?;
        if hvc::hvc_state(&self.vmid).await? == VmState::Running {
            Ok(Some(
                powershell::run_get_vm_screenshot(
                    &self.vmid,
                    image,
                    &self.ps_mod,
                    &self.temp_bin_path,
                )
                .await?,
            ))
        } else {
            Ok(None)
        }
    }
}
