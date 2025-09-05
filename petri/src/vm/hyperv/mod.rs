// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod hvc;
pub mod powershell;
pub mod vm;
use vmsocket::VmAddress;
use vmsocket::VmSocket;

use super::ProcessorTopology;
use crate::Firmware;
use crate::IsolationType;
use crate::NoPetriVmInspector;
use crate::OpenHclConfig;
use crate::OpenHclServicingFlags;
use crate::PetriHaltReason;
use crate::PetriVmConfig;
use crate::PetriVmResources;
use crate::PetriVmRuntime;
use crate::PetriVmmBackend;
use crate::SecureBootTemplate;
use crate::ShutdownKind;
use crate::UefiConfig;
use crate::disk_image::AgentImage;
use crate::hyperv::powershell::HyperVSecureBootTemplate;
use crate::kmsg_log_task;
use crate::openhcl_diag::OpenHclDiagHandler;
use crate::vm::append_cmdline;
use anyhow::Context;
use async_trait::async_trait;
use get_resources::ged::FirmwareEvent;
use jiff::Timestamp;
use jiff::ToSpan;
use pal_async::DefaultDriver;
use pal_async::pipe::PolledPipe;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use petri_artifacts_common::tags::GuestQuirks;
use petri_artifacts_common::tags::GuestQuirksInner;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use pipette_client::PipetteClient;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use vm::HyperVVM;

/// The Hyper-V Petri backend
pub struct HyperVPetriBackend {}

/// Resources needed at runtime for a Hyper-V Petri VM
pub struct HyperVPetriRuntime {
    vm: HyperVVM,
    log_tasks: Vec<Task<anyhow::Result<()>>>,
    temp_dir: tempfile::TempDir,
    driver: DefaultDriver,

    is_openhcl: bool,
    is_isolated: bool,
}

#[async_trait]
impl PetriVmmBackend for HyperVPetriBackend {
    type VmmConfig = ();
    type VmRuntime = HyperVPetriRuntime;

    fn check_compat(firmware: &Firmware, arch: MachineArch) -> bool {
        arch == MachineArch::host()
            && !firmware.is_linux_direct()
            && !(firmware.is_pcat() && arch == MachineArch::Aarch64)
    }

    fn select_quirks(quirks: GuestQuirks) -> GuestQuirksInner {
        quirks.hyperv
    }

    fn new(_resolver: &ArtifactResolver<'_>) -> Self {
        HyperVPetriBackend {}
    }

    async fn run(
        self,
        config: PetriVmConfig,
        modify_vmm_config: Option<impl FnOnce(Self::VmmConfig) -> Self::VmmConfig + Send>,
        resources: &PetriVmResources,
    ) -> anyhow::Result<Self::VmRuntime> {
        if modify_vmm_config.is_some() {
            panic!("specified modify_vmm_config, but that is not supported for hyperv");
        }

        let PetriVmConfig {
            name,
            arch,
            firmware,
            memory,
            proc_topology,
            agent_image,
            openhcl_agent_image,
            vmgs: _, // TODO
        } = &config;

        let PetriVmResources {
            driver,
            output_dir: _,
            log_source,
        } = resources;

        let temp_dir = tempfile::tempdir()?;

        let (
            guest_state_isolation_type,
            generation,
            guest_artifact,
            uefi_config,
            mut openhcl_config,
        ) = match &firmware {
            Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => {
                todo!("linux direct not supported on hyper-v")
            }
            Firmware::Pcat {
                guest,
                bios_firmware: _, // TODO
                svga_firmware: _, // TODO
            } => (
                powershell::HyperVGuestStateIsolationType::Disabled,
                powershell::HyperVGeneration::One,
                Some(guest.artifact()),
                None,
                None,
            ),
            Firmware::OpenhclPcat {
                guest,
                igvm_path,
                bios_firmware: _, // TODO
                svga_firmware: _, // TODO
                openhcl_config,
            } => (
                powershell::HyperVGuestStateIsolationType::OpenHCL,
                powershell::HyperVGeneration::One,
                Some(guest.artifact()),
                None,
                Some((igvm_path, openhcl_config.clone())),
            ),
            Firmware::Uefi {
                guest,
                uefi_firmware: _, // TODO
                uefi_config,
            } => (
                powershell::HyperVGuestStateIsolationType::Disabled,
                powershell::HyperVGeneration::Two,
                guest.artifact(),
                Some(uefi_config),
                None,
            ),
            Firmware::OpenhclUefi {
                guest,
                isolation,
                igvm_path,
                uefi_config,
                openhcl_config,
            } => (
                match isolation {
                    Some(IsolationType::Vbs) => powershell::HyperVGuestStateIsolationType::Vbs,
                    Some(IsolationType::Snp) => powershell::HyperVGuestStateIsolationType::Snp,
                    Some(IsolationType::Tdx) => powershell::HyperVGuestStateIsolationType::Tdx,
                    None => powershell::HyperVGuestStateIsolationType::TrustedLaunch,
                },
                powershell::HyperVGeneration::Two,
                guest.artifact(),
                Some(uefi_config),
                Some((igvm_path, openhcl_config.clone())),
            ),
        };

        let vhd_paths = guest_artifact
            .map(|artifact| vec![vec![artifact.get()]])
            .unwrap_or_default();

        let mut log_tasks = Vec::new();

        let mut vm = HyperVVM::new(
            name,
            generation,
            guest_state_isolation_type,
            memory.startup_bytes,
            log_source.log_file("hyperv")?,
            driver.clone(),
        )
        .await?;

        {
            let ProcessorTopology {
                vp_count,
                vps_per_socket,
                enable_smt,
                apic_mode,
            } = proc_topology;
            // TODO: fix this mapping, and/or update petri to better match
            // Hyper-V's capabilities.
            let apic_mode = apic_mode
                .map(|m| match m {
                    super::ApicMode::Xapic => powershell::HyperVApicMode::Legacy,
                    super::ApicMode::X2apicSupported => powershell::HyperVApicMode::X2Apic,
                    super::ApicMode::X2apicEnabled => powershell::HyperVApicMode::X2Apic,
                })
                .or((*arch == MachineArch::X86_64
                    && generation == powershell::HyperVGeneration::Two)
                    .then_some({
                        // This is necessary for some tests to pass. TODO: fix.
                        powershell::HyperVApicMode::X2Apic
                    }));
            vm.set_processor(&powershell::HyperVSetVMProcessorArgs {
                count: Some(*vp_count),
                apic_mode,
                hw_thread_count_per_core: enable_smt.map(|smt| if smt { 2 } else { 1 }),
                maximum_count_per_numa_node: *vps_per_socket,
            })
            .await?;
        }

        if let Some(UefiConfig {
            secure_boot_enabled,
            secure_boot_template,
            disable_frontpage,
        }) = uefi_config
        {
            vm.set_secure_boot(
                *secure_boot_enabled,
                secure_boot_template.map(|t| match t {
                    SecureBootTemplate::MicrosoftWindows => {
                        HyperVSecureBootTemplate::MicrosoftWindows
                    }
                    SecureBootTemplate::MicrosoftUefiCertificateAuthority => {
                        HyperVSecureBootTemplate::MicrosoftUEFICertificateAuthority
                    }
                }),
            )
            .await?;

            if *disable_frontpage {
                // TODO: Disable frontpage for non-OpenHCL Hyper-V VMs
                if let Some((_, config)) = openhcl_config.as_mut() {
                    append_cmdline(&mut config.command_line, "OPENHCL_DISABLE_UEFI_FRONTPAGE=1");
                };
            }
        }

        for (i, vhds) in vhd_paths.iter().enumerate() {
            let (controller_type, controller_number) = match generation {
                powershell::HyperVGeneration::One => (powershell::ControllerType::Ide, i as u32),
                powershell::HyperVGeneration::Two => (
                    powershell::ControllerType::Scsi,
                    vm.add_scsi_controller(0).await?,
                ),
            };
            for (controller_location, vhd) in vhds.iter().enumerate() {
                let diff_disk_path = temp_dir.path().join(format!(
                    "{}_{}_{}",
                    controller_number,
                    controller_location,
                    vhd.file_name()
                        .context("path has no filename")?
                        .to_string_lossy()
                ));

                {
                    let path = diff_disk_path.clone();
                    let parent_path = vhd.to_path_buf();
                    tracing::debug!(?path, ?parent_path, "creating differencing vhd");
                    blocking::unblock(move || disk_vhdmp::Vhd::create_diff(&path, &parent_path))
                        .await?;
                }

                vm.add_vhd(
                    &diff_disk_path,
                    controller_type,
                    Some(controller_location as u32),
                    Some(controller_number),
                )
                .await?;
            }
        }

        if let Some(agent_image) = agent_image {
            // Construct the agent disk.
            let agent_disk_path = temp_dir.path().join("cidata.vhd");

            if build_and_persist_agent_image(agent_image, &agent_disk_path)
                .context("vtl0 agent disk")?
            {
                if agent_image.contains_pipette()
                    && matches!(firmware.os_flavor(), OsFlavor::Windows)
                {
                    // Make a file for the IMC hive. It's not guaranteed to be at a fixed
                    // location at runtime.
                    let imc_hive = temp_dir.path().join("imc.hiv");
                    {
                        let mut imc_hive_file = fs_err::File::create_new(&imc_hive)?;
                        imc_hive_file
                            .write_all(include_bytes!("../../../guest-bootstrap/imc.hiv"))
                            .context("failed to write imc hive")?;
                    }

                    // Set the IMC
                    vm.set_imc(&imc_hive).await?;
                }

                let controller_number = vm.add_scsi_controller(0).await?;
                vm.add_vhd(
                    &agent_disk_path,
                    powershell::ControllerType::Scsi,
                    Some(0),
                    Some(controller_number),
                )
                .await?;
            }
        }

        if let Some((
            src_igvm_file,
            OpenHclConfig {
                vtl2_nvme_boot: _, // TODO, see #1649.
                vmbus_redirect,
                command_line,
            },
        )) = &openhcl_config
        {
            // Copy the IGVM file locally, since it may not be accessible by
            // Hyper-V (e.g., if it is in a WSL filesystem).
            let igvm_file = temp_dir.path().join("igvm.bin");
            fs_err::copy(src_igvm_file, &igvm_file).context("failed to copy igvm file")?;
            acl_read_for_vm(&igvm_file, Some(*vm.vmid()))
                .context("failed to set ACL for igvm file")?;

            // TODO: only increase VTL2 memory on debug builds
            vm.set_openhcl_firmware(
                &igvm_file,
                // don't increase VTL2 memory on CVMs
                !matches!(
                    guest_state_isolation_type,
                    powershell::HyperVGuestStateIsolationType::Vbs
                        | powershell::HyperVGuestStateIsolationType::Snp
                        | powershell::HyperVGuestStateIsolationType::Tdx
                ),
            )
            .await?;

            if let Some(command_line) = command_line {
                vm.set_vm_firmware_command_line(command_line).await?;
            }

            vm.set_vmbus_redirect(*vmbus_redirect).await?;

            if let Some(agent_image) = openhcl_agent_image {
                let agent_disk_path = temp_dir.path().join("paravisor_cidata.vhd");

                if build_and_persist_agent_image(agent_image, &agent_disk_path)
                    .context("vtl2 agent disk")?
                {
                    let controller_number = vm.add_scsi_controller(2).await?;
                    vm.add_vhd(
                        &agent_disk_path,
                        powershell::ControllerType::Scsi,
                        Some(0),
                        Some(controller_number),
                    )
                    .await?;
                }
            }

            // Attempt to enable COM3 and use that to get KMSG logs, otherwise
            // fall back to use diag_client.
            let supports_com3 = {
                // Hyper-V VBS VMs don't work with COM3 enabled.
                // Hypervisor support is needed for this to work.
                let is_not_vbs = !matches!(
                    guest_state_isolation_type,
                    powershell::HyperVGuestStateIsolationType::Vbs
                );

                // The Hyper-V serial device for ARM doesn't support additional
                // serial ports yet.
                let is_x86 = matches!(arch, MachineArch::X86_64);

                // The registry key to enable additional COM ports is only
                // available in newer builds of Windows.
                let current_winver = windows_version::OsVersion::current();
                tracing::debug!(?current_winver, "host windows version");
                // This is the oldest working build used in CI
                // TODO: determine the actual minimum version
                const COM3_MIN_WINVER: u32 = 27813;
                let is_supported_winver = current_winver.build >= COM3_MIN_WINVER;

                is_not_vbs && is_x86 && is_supported_winver
            };

            let openhcl_log_file = log_source.log_file("openhcl")?;
            if supports_com3 {
                tracing::debug!("getting kmsg logs from COM3");

                let openhcl_serial_pipe_path = vm.set_vm_com_port(3).await?;
                log_tasks.push(driver.spawn(
                    "openhcl-log",
                    hyperv_serial_log_task(
                        driver.clone(),
                        openhcl_serial_pipe_path,
                        openhcl_log_file,
                    ),
                ));
            } else {
                tracing::debug!("getting kmsg logs from diag_client");

                log_tasks.push(driver.spawn(
                    "openhcl-log",
                    kmsg_log_task(
                        openhcl_log_file,
                        diag_client::DiagClient::from_hyperv_id(driver.clone(), *vm.vmid()),
                    ),
                ));
            }
        }

        let serial_pipe_path = vm.set_vm_com_port(1).await?;
        let serial_log_file = log_source.log_file("guest")?;
        log_tasks.push(driver.spawn(
            "guest-log",
            hyperv_serial_log_task(driver.clone(), serial_pipe_path, serial_log_file),
        ));

        vm.start().await?;

        Ok(HyperVPetriRuntime {
            vm,
            log_tasks,
            temp_dir,
            driver: driver.clone(),
            is_openhcl: openhcl_config.is_some(),
            is_isolated: firmware.isolation().is_some(),
        })
    }
}

#[async_trait]
impl PetriVmRuntime for HyperVPetriRuntime {
    type VmInspector = NoPetriVmInspector;
    type VmFramebufferAccess = vm::HyperVFramebufferAccess;

    async fn teardown(mut self) -> anyhow::Result<()> {
        futures::future::join_all(self.log_tasks.into_iter().map(|t| t.cancel())).await;
        self.vm.remove().await
    }

    async fn wait_for_halt(&mut self, allow_reset: bool) -> anyhow::Result<PetriHaltReason> {
        self.vm.wait_for_halt(allow_reset).await
    }

    async fn wait_for_agent(&mut self, set_high_vtl: bool) -> anyhow::Result<PipetteClient> {
        let socket = VmSocket::new().context("failed to create AF_HYPERV socket")?;
        socket
            .set_connect_timeout(Duration::from_secs(5))
            .context("failed to set connect timeout")?;
        socket
            .set_high_vtl(set_high_vtl)
            .context("failed to set socket for VTL0")?;

        // TODO: This maximum is specific to hyper-v tests and should be configurable.
        //
        // Allow for the slowest test (hyperv_pcat_x64_ubuntu_2204_server_x64_boot)
        // but fail before the nextest timeout. (~1 attempt for second)
        let connect_timeout = 240.seconds();
        let start = Timestamp::now();

        let mut socket = PolledSocket::new(&self.driver, socket)?.convert();
        while let Err(e) = socket
            .connect(
                &VmAddress::hyperv_vsock(*self.vm.vmid(), pipette_client::PIPETTE_VSOCK_PORT)
                    .into(),
            )
            .await
        {
            if connect_timeout.compare(Timestamp::now() - start)? == std::cmp::Ordering::Less {
                anyhow::bail!("Pipette connection timed out: {e}")
            }
            PolledTimer::new(&self.driver)
                .sleep(Duration::from_secs(1))
                .await;
        }

        PipetteClient::new(&self.driver, socket, self.temp_dir.path())
            .await
            .context("failed to connect to pipette")
    }

    fn openhcl_diag(&self) -> Option<OpenHclDiagHandler> {
        self.is_openhcl.then(|| {
            OpenHclDiagHandler::new(diag_client::DiagClient::from_hyperv_id(
                self.driver.clone(),
                *self.vm.vmid(),
            ))
        })
    }

    async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent> {
        self.vm.wait_for_boot_event().await
    }

    async fn wait_for_enlightened_shutdown_ready(&mut self) -> anyhow::Result<()> {
        self.vm.wait_for_enlightened_shutdown_ready().await
    }

    async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()> {
        match kind {
            ShutdownKind::Shutdown => self.vm.stop().await?,
            ShutdownKind::Reboot => self.vm.restart().await?,
        }

        Ok(())
    }

    async fn restart_openhcl(
        &mut self,
        _new_openhcl: &ResolvedArtifact,
        flags: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        // TODO: Updating the file causes failure ... self.vm.set_openhcl_firmware(new_openhcl.get(), false)?;
        self.vm.restart_openhcl(flags).await
    }

    fn take_framebuffer_access(&mut self) -> Option<vm::HyperVFramebufferAccess> {
        (!self.is_isolated).then(|| self.vm.get_framebuffer_access())
    }
}

fn acl_read_for_vm(path: &Path, id: Option<guid::Guid>) -> anyhow::Result<()> {
    let sid_arg = format!(
        "NT VIRTUAL MACHINE\\{name}:R",
        name = if let Some(id) = id {
            format!("{id:X}")
        } else {
            "Virtual Machines".to_string()
        }
    );
    let output = std::process::Command::new("icacls.exe")
        .arg(path)
        .arg("/grant")
        .arg(sid_arg)
        .output()
        .context("failed to run icacls")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("icacls failed: {stderr}");
    }
    Ok(())
}

fn build_and_persist_agent_image(
    agent_image: &AgentImage,
    agent_disk_path: &Path,
) -> anyhow::Result<bool> {
    Ok(
        if let Some(agent_disk) = agent_image.build().context("failed to build agent image")? {
            disk_vhd1::Vhd1Disk::make_fixed(agent_disk.as_file())
                .context("failed to make vhd for agent image")?;
            agent_disk.persist(agent_disk_path)?;
            true
        } else {
            false
        },
    )
}

async fn hyperv_serial_log_task(
    driver: DefaultDriver,
    serial_pipe_path: String,
    log_file: crate::PetriLogFile,
) -> anyhow::Result<()> {
    let mut timer = None;
    loop {
        // using `std::fs` here instead of `fs_err` since `raw_os_error` always
        // returns `None` for `fs_err` errors.
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&serial_pipe_path)
        {
            Ok(file) => {
                let pipe = PolledPipe::new(&driver, file).expect("failed to create pipe");
                // connect/disconnect messages logged internally
                _ = crate::log_task(log_file.clone(), pipe, &serial_pipe_path).await;
            }
            Err(err) => {
                // Log the error if it isn't just that the VM is not running
                // or the pipe is "busy" (which is reported during reset).
                const ERROR_PIPE_BUSY: i32 = 231;
                if !(err.kind() == ErrorKind::NotFound
                    || matches!(err.raw_os_error(), Some(ERROR_PIPE_BUSY)))
                {
                    tracing::warn!("failed to open {serial_pipe_path}: {err:#}",)
                }
                // Wait a bit and try again.
                timer
                    .get_or_insert_with(|| PolledTimer::new(&driver))
                    .sleep(Duration::from_millis(100))
                    .await;
            }
        }
    }
}
