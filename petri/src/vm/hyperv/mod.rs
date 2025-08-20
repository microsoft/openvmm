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
use crate::PetriVmConfig;
use crate::PetriVmFramebufferAccess;
use crate::PetriVmResources;
use crate::PetriVmRuntime;
use crate::PetriVmmBackend;
use crate::SecureBootTemplate;
use crate::ShutdownKind;
use crate::UefiConfig;
use crate::VmScreenshotMeta;
use crate::disk_image::AgentImage;
use crate::hyperv::powershell::HyperVSecureBootTemplate;
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
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use pipette_client::PipetteClient;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use vm::HyperVVM;
use vmm_core_defs::HaltReason;

/// The Hyper-V Petri backend
pub struct HyperVPetriBackend {}

/// Resources needed at runtime for a Hyper-V Petri VM
pub struct HyperVPetriRuntime {
    vm: Arc<HyperVVM>,
    log_tasks: Vec<Task<anyhow::Result<()>>>,
    temp_dir: tempfile::TempDir,
    is_openhcl: bool,
    is_isolated: bool,
    driver: DefaultDriver,
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
            firmware.expected_boot_event(),
            driver.clone(),
        )?;

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
            })?;
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
            )?;

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
                powershell::HyperVGeneration::Two => {
                    (powershell::ControllerType::Scsi, vm.add_scsi_controller(0)?)
                }
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

                create_child_vhd_locking(driver, &diff_disk_path, vhd).await?;
                vm.add_vhd(
                    &diff_disk_path,
                    controller_type,
                    Some(controller_location as u32),
                    Some(controller_number),
                )?;
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
                        let mut imc_hive_file = fs::File::create_new(&imc_hive)?;
                        imc_hive_file
                            .write_all(include_bytes!("../../../guest-bootstrap/imc.hiv"))
                            .context("failed to write imc hive")?;
                    }

                    // Set the IMC
                    vm.set_imc(&imc_hive)?;
                }

                let controller_number = vm.add_scsi_controller(0)?;
                vm.add_vhd(
                    &agent_disk_path,
                    powershell::ControllerType::Scsi,
                    Some(0),
                    Some(controller_number),
                )?;
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
            )?;

            if let Some(command_line) = command_line {
                vm.set_vm_firmware_command_line(command_line)?;
            }

            vm.set_vmbus_redirect(*vmbus_redirect)?;

            if let Some(agent_image) = openhcl_agent_image {
                let agent_disk_path = temp_dir.path().join("paravisor_cidata.vhd");

                if build_and_persist_agent_image(agent_image, &agent_disk_path)
                    .context("vtl2 agent disk")?
                {
                    let controller_number = vm.add_scsi_controller(2)?;
                    vm.add_vhd(
                        &agent_disk_path,
                        powershell::ControllerType::Scsi,
                        Some(0),
                        Some(controller_number),
                    )?;
                }
            }

            let openhcl_log_file = log_source.log_file("openhcl")?;
            log_tasks.push(driver.spawn("openhcl-log", {
                let driver = driver.clone();
                let vmid = *vm.vmid();
                async move {
                    let diag_client = diag_client::DiagClient::from_hyperv_id(driver.clone(), vmid);
                    loop {
                        diag_client.wait_for_server().await?;
                        crate::kmsg_log_task(
                            openhcl_log_file.clone(),
                            diag_client.kmsg(true).await?,
                        )
                        .await?
                    }
                }
            }));
        }

        let serial_pipe_path = vm.set_vm_com_port(1)?;
        let serial_log_file = log_source.log_file("guest")?;
        log_tasks.push(driver.spawn("guest-log", {
            let driver = driver.clone();
            async move {
                let serial = diag_client::hyperv::open_serial_port(
                    &driver,
                    diag_client::hyperv::ComPortAccessInfo::PortPipePath(&serial_pipe_path),
                )
                .await?;
                crate::log_stream(serial_log_file, PolledPipe::new(&driver, serial)?).await
            }
        }));

        vm.start()?;

        Ok(HyperVPetriRuntime {
            vm: Arc::new(vm),
            log_tasks,
            temp_dir,
            is_openhcl: openhcl_config.is_some(),
            is_isolated: firmware.isolation().is_some(),
            driver: driver.clone(),
        })
    }
}

#[async_trait]
impl PetriVmRuntime for HyperVPetriRuntime {
    type VmInspector = NoPetriVmInspector;
    type VmFramebufferAccess = HyperVFramebufferAccess;

    async fn teardown(mut self) -> anyhow::Result<()> {
        futures::future::join_all(self.log_tasks.into_iter().map(|t| t.cancel())).await;
        Arc::into_inner(self.vm)
            .context("all references to the Hyper-V VM object have not been closed")?
            .remove()
    }

    async fn wait_for_halt(&mut self) -> anyhow::Result<HaltReason> {
        self.vm.wait_for_halt().await?;
        Ok(HaltReason::PowerOff) // TODO: Get actual halt reason
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

    async fn wait_for_successful_boot_event(&mut self) -> anyhow::Result<()> {
        self.vm.wait_for_successful_boot_event().await
    }

    async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent> {
        self.vm.wait_for_boot_event().await
    }

    async fn wait_for_enlightened_shutdown_ready(&mut self) -> anyhow::Result<()> {
        self.vm.wait_for_enlightened_shutdown_ready().await
    }

    async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()> {
        match kind {
            ShutdownKind::Shutdown => self.vm.stop()?,
            ShutdownKind::Reboot => self.vm.restart()?,
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

    fn take_framebuffer_access(&mut self) -> Option<HyperVFramebufferAccess> {
        (!self.is_isolated).then(|| HyperVFramebufferAccess {
            vm: Arc::downgrade(&self.vm),
        })
    }
}

/// Interface to the Hyper-V framebuffer
pub struct HyperVFramebufferAccess {
    vm: Weak<HyperVVM>,
}

impl PetriVmFramebufferAccess for HyperVFramebufferAccess {
    fn screenshot(&mut self, image: &mut Vec<u8>) -> anyhow::Result<VmScreenshotMeta> {
        self.vm
            .upgrade()
            .context("VM no longer exists")?
            .screenshot(image)
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

/// Create a new differencing VHD with the provided parent.
pub async fn create_child_vhd_locking(
    driver: &DefaultDriver,
    path: &Path,
    parent_path: &Path,
) -> anyhow::Result<()> {
    let lock_file_path = {
        let mut path = parent_path.to_owned();
        path.as_mut_os_string().push(".lock");
        path
    };

    tracing::debug!("creating child vhd from {}", path.to_string_lossy());

    let start = Timestamp::now();
    loop {
        match fs_err::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_file_path)
        {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                tracing::debug!("vhd lock taken, waiting...");
                PolledTimer::new(driver).sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let time_elapsed = Timestamp::now() - start;

    tracing::debug!(
        "waited {}s for {}",
        time_elapsed.total(jiff::Unit::Second).unwrap(),
        lock_file_path.to_string_lossy()
    );

    let res = powershell::create_child_vhd(path, parent_path);

    fs_err::remove_file(&lock_file_path)?;

    res
}
