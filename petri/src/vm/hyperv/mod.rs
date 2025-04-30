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
use crate::PetriLogSource;
use crate::PetriTestParams;
use crate::PetriVm;
use crate::PetriVmConfig;
use crate::ShutdownKind;
use crate::disk_image::AgentImage;
use crate::openhcl_diag::OpenHclDiagHandler;
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
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use vm::HyperVVM;
use vmm_core_defs::HaltReason;

/// Hyper-V VM configuration and resources
pub struct PetriVmConfigHyperV {
    // Specifies the name of the new virtual machine.
    name: String,
    arch: MachineArch,
    // Specifies the generation for the virtual machine.
    generation: powershell::HyperVGeneration,
    // Specifies the Guest State Isolation Type
    guest_state_isolation_type: powershell::HyperVGuestStateIsolationType,
    // Specifies the amount of memory, in bytes, to assign to the virtual machine.
    memory: u64,
    proc_topology: ProcessorTopology,
    // Specifies the path to a virtual hard disk file(s) to attach to the
    // virtual machine as SCSI (Gen2) or IDE (Gen1) drives.
    vhd_paths: Vec<Vec<PathBuf>>,
    secure_boot_template: Option<powershell::HyperVSecureBootTemplate>,
    openhcl_igvm: Option<ResolvedArtifact>,
    openhcl_command_line: String,
    disable_frontpage: bool,

    driver: DefaultDriver,
    agent_image: AgentImage,
    openhcl_agent_image: Option<AgentImage>,

    os_flavor: OsFlavor,
    expected_boot_event: Option<FirmwareEvent>,

    // Folder to store temporary data for this test
    temp_dir: tempfile::TempDir,

    log_source: PetriLogSource,
}

#[async_trait]
impl PetriVmConfig for PetriVmConfigHyperV {
    async fn run_without_agent(self: Box<Self>) -> anyhow::Result<Box<dyn PetriVm>> {
        Ok(Box::new(Self::run_without_agent(*self).await?))
    }

    async fn run_with_lazy_pipette(mut self: Box<Self>) -> anyhow::Result<Box<dyn PetriVm>> {
        Ok(Box::new(Self::run_with_lazy_pipette(*self).await?))
    }

    async fn run(self: Box<Self>) -> anyhow::Result<(Box<dyn PetriVm>, PipetteClient)> {
        let (vm, client) = Self::run(*self).await?;
        Ok((Box::new(vm), client))
    }

    fn with_windows_secure_boot_template(self: Box<Self>) -> Box<dyn PetriVmConfig> {
        Box::new(Self::with_windows_secure_boot_template(*self))
    }

    fn with_processor_topology(
        self: Box<Self>,
        topology: ProcessorTopology,
    ) -> Box<dyn PetriVmConfig> {
        Box::new(Self::with_processor_topology(*self, topology))
    }

    fn with_custom_openhcl(self: Box<Self>, artifact: ResolvedArtifact) -> Box<dyn PetriVmConfig> {
        Box::new(Self::with_custom_openhcl(*self, artifact))
    }

    fn with_openhcl_command_line(self: Box<Self>, command_line: &str) -> Box<dyn PetriVmConfig> {
        Box::new(Self::with_openhcl_command_line(*self, command_line))
    }

    fn with_agent_file(
        self: Box<Self>,
        name: &str,
        artifact: ResolvedArtifact,
    ) -> Box<dyn PetriVmConfig> {
        Box::new(Self::with_agent_file(*self, name, artifact))
    }

    fn with_openhcl_agent_file(
        self: Box<Self>,
        name: &str,
        artifact: ResolvedArtifact,
    ) -> Box<dyn PetriVmConfig> {
        Box::new(Self::with_openhcl_agent_file(*self, name, artifact))
    }

    fn with_uefi_frontpage(self: Box<Self>, enable: bool) -> Box<dyn PetriVmConfig> {
        Box::new(Self::with_uefi_frontpage(*self, enable))
    }
}

/// A running VM that tests can interact with.
pub struct PetriVmHyperV {
    config: PetriVmConfigHyperV,
    vm: HyperVVM,
    openhcl_diag_handler: Option<OpenHclDiagHandler>,
    log_tasks: Vec<Task<anyhow::Result<()>>>,
}

#[async_trait]
impl PetriVm for PetriVmHyperV {
    fn arch(&self) -> MachineArch {
        self.config.arch
    }

    async fn wait_for_halt(&mut self) -> anyhow::Result<HaltReason> {
        Self::wait_for_halt(self).await
    }

    async fn wait_for_teardown(self: Box<Self>) -> anyhow::Result<HaltReason> {
        Self::wait_for_teardown(*self).await
    }

    async fn test_inspect_openhcl(&mut self) -> anyhow::Result<()> {
        Self::test_inspect_openhcl(self).await
    }

    async fn wait_for_agent(&mut self) -> anyhow::Result<PipetteClient> {
        Self::wait_for_agent(self).await
    }

    async fn wait_for_vtl2_agent(&mut self) -> anyhow::Result<PipetteClient> {
        Self::wait_for_vtl2_agent(self).await
    }

    async fn wait_for_vtl2_ready(&mut self) -> anyhow::Result<()> {
        Self::wait_for_vtl2_ready(self).await
    }

    async fn wait_for_successful_boot_event(&mut self) -> anyhow::Result<()> {
        Self::wait_for_successful_boot_event(self).await
    }

    async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent> {
        Self::wait_for_boot_event(self).await
    }

    async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()> {
        Self::send_enlightened_shutdown(self, kind).await
    }
}

/// Artifacts needed to create a [`PetriVmConfigHyperV`].
pub struct PetriVmArtifactsHyperV {
    arch: MachineArch,
    agent_image: AgentImage,
    openhcl_agent_image: Option<AgentImage>,
    firmware: Firmware,
}

impl PetriVmArtifactsHyperV {
    /// Resolves the artifacts needed to instantiate a [`PetriVmConfigHyperV`].
    ///
    /// Returns `None` if the supplied configuration is not supported on this platform.
    pub fn new(
        resolver: &ArtifactResolver<'_>,
        firmware: Firmware,
        arch: MachineArch,
    ) -> Option<Self> {
        if arch != MachineArch::host() {
            return None;
        }
        Some(Self {
            arch,
            agent_image: AgentImage::new(resolver, arch, firmware.os_flavor()),
            openhcl_agent_image: if firmware.is_openhcl() {
                Some(AgentImage::new(resolver, arch, OsFlavor::Linux))
            } else {
                None
            },
            firmware,
        })
    }
}

impl PetriVmConfigHyperV {
    /// Create a new Hyper-V petri VM config
    pub fn new(
        params: &PetriTestParams<'_>,
        artifacts: PetriVmArtifactsHyperV,
        driver: &DefaultDriver,
    ) -> anyhow::Result<Self> {
        let PetriVmArtifactsHyperV {
            arch,
            agent_image,
            openhcl_agent_image,
            firmware,
        } = artifacts;
        let temp_dir = tempfile::tempdir()?;

        let (guest_state_isolation_type, generation, guest_artifact, igvm_artifact) =
            match &firmware {
                Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => {
                    todo!("linux direct not supported on hyper-v")
                }
                Firmware::Pcat { guest, .. } => (
                    powershell::HyperVGuestStateIsolationType::Disabled,
                    powershell::HyperVGeneration::One,
                    Some(guest.artifact()),
                    None,
                ),
                Firmware::Uefi { guest, .. } => (
                    powershell::HyperVGuestStateIsolationType::Disabled,
                    powershell::HyperVGeneration::Two,
                    guest.artifact(),
                    None,
                ),
                Firmware::OpenhclUefi {
                    guest,
                    isolation,
                    igvm_path,
                    vtl2_nvme_boot: _, // TODO
                } => (
                    match isolation {
                        Some(IsolationType::Vbs) => powershell::HyperVGuestStateIsolationType::Vbs,
                        Some(IsolationType::Snp) => powershell::HyperVGuestStateIsolationType::Snp,
                        Some(IsolationType::Tdx) => powershell::HyperVGuestStateIsolationType::Tdx,
                        None => powershell::HyperVGuestStateIsolationType::TrustedLaunch,
                    },
                    powershell::HyperVGeneration::Two,
                    guest.artifact(),
                    Some(igvm_path),
                ),
                // TODO: OpenHCL PCAT
            };

        let vhd_paths = guest_artifact
            .map(|artifact| vec![vec![artifact.into()]])
            .unwrap_or_default();
        let openhcl_igvm = igvm_artifact.cloned();

        Ok(PetriVmConfigHyperV {
            name: params.test_name.to_owned(),
            arch,
            generation,
            guest_state_isolation_type,
            memory: 0x1_0000_0000,
            proc_topology: ProcessorTopology::default(),
            vhd_paths,
            secure_boot_template: matches!(generation, powershell::HyperVGeneration::Two)
                .then_some(match firmware.os_flavor() {
                    OsFlavor::Windows => powershell::HyperVSecureBootTemplate::MicrosoftWindows,
                    OsFlavor::Linux => {
                        powershell::HyperVSecureBootTemplate::MicrosoftUEFICertificateAuthority
                    }
                    OsFlavor::FreeBsd | OsFlavor::Uefi => {
                        powershell::HyperVSecureBootTemplate::SecureBootDisabled
                    }
                }),
            openhcl_igvm,
            agent_image,
            openhcl_agent_image,
            driver: driver.clone(),
            os_flavor: firmware.os_flavor(),
            expected_boot_event: firmware.expected_boot_event(),
            temp_dir,
            log_source: params.logger.clone(),
            disable_frontpage: true,
            openhcl_command_line: String::new(),
        })
    }

    /// Build and boot the requested VM. Does not configure and start pipette.
    /// Should only be used for testing platforms that pipette does not support.
    pub async fn run_without_agent(self) -> anyhow::Result<PetriVmHyperV> {
        self.run_core(false).await
    }

    /// Run the VM, configuring pipette to automatically start, but do not wait
    /// for it to connect. This is useful for tests where the first boot attempt
    /// is expected to not succeed, but pipette functionality is still desired.
    pub async fn run_with_lazy_pipette(self) -> anyhow::Result<PetriVmHyperV> {
        self.run_core(true).await
    }

    /// Run the VM, launching pipette and returning a client to it.
    pub async fn run(self) -> anyhow::Result<(PetriVmHyperV, PipetteClient)> {
        let mut vm = self.run_core(true).await?;
        let client = vm.wait_for_agent().await?;
        Ok((vm, client))
    }

    /// Build and boot the requested VM
    async fn run_core(mut self, with_agent: bool) -> anyhow::Result<PetriVmHyperV> {
        let mut vm = HyperVVM::new(
            &self.name,
            self.generation,
            self.guest_state_isolation_type,
            self.memory,
            self.log_source.log_file("hyperv")?,
            self.expected_boot_event,
            self.driver.clone(),
        )?;

        {
            let ProcessorTopology {
                vp_count,
                vps_per_socket,
                enable_smt,
                apic_mode,
            } = self.proc_topology;
            // TODO: fix this mapping, and/or update petri to better match
            // Hyper-V's capabilities.
            let apic_mode = apic_mode
                .map(|m| match m {
                    super::ApicMode::Xapic => powershell::HyperVApicMode::Legacy,
                    super::ApicMode::X2apicSupported => powershell::HyperVApicMode::X2Apic,
                    super::ApicMode::X2apicEnabled => powershell::HyperVApicMode::X2Apic,
                })
                .or((self.arch == MachineArch::X86_64
                    && self.generation == powershell::HyperVGeneration::Two)
                    .then_some({
                        // This is necessary for some tests to pass. TODO: fix.
                        powershell::HyperVApicMode::X2Apic
                    }));
            vm.set_processor(&powershell::HyperVSetVMProcessorArgs {
                count: Some(vp_count),
                apic_mode,
                hw_thread_count_per_core: enable_smt.map(|smt| if smt { 2 } else { 1 }),
                maximum_count_per_numa_node: vps_per_socket,
            })?;
        }

        if let Some(secure_boot_template) = self.secure_boot_template {
            vm.set_secure_boot_template(secure_boot_template)?;
        }

        for (i, vhds) in self.vhd_paths.iter().enumerate() {
            let (controller_type, controller_number) = match self.generation {
                powershell::HyperVGeneration::One => (powershell::ControllerType::Ide, i as u32),
                powershell::HyperVGeneration::Two => {
                    (powershell::ControllerType::Scsi, vm.add_scsi_controller(0)?)
                }
            };
            for (controller_location, vhd) in vhds.iter().enumerate() {
                let diff_disk_path = self.temp_dir.path().join(format!(
                    "{}_{}_{}",
                    controller_number,
                    controller_location,
                    vhd.file_name()
                        .context("path has no filename")?
                        .to_string_lossy()
                ));

                powershell::create_child_vhd(&diff_disk_path, vhd)?;
                vm.add_vhd(
                    &diff_disk_path,
                    controller_type,
                    Some(controller_location as u32),
                    Some(controller_number),
                )?;
            }
        }

        if with_agent {
            // Construct the agent disk.
            let agent_disk_path = self.temp_dir.path().join("cidata.vhd");
            {
                let agent_disk = self
                    .agent_image
                    .build()
                    .context("failed to build agent image")?;
                disk_vhd1::Vhd1Disk::make_fixed(agent_disk.as_file())
                    .context("failed to make vhd for agent image")?;
                agent_disk.persist(&agent_disk_path)?;
            }

            if matches!(self.os_flavor, OsFlavor::Windows) {
                // Make a file for the IMC hive. It's not guaranteed to be at a fixed
                // location at runtime.
                let imc_hive = self.temp_dir.path().join("imc.hiv");
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

        if let Some(src_igvm_file) = &self.openhcl_igvm {
            // Copy the IGVM file locally, since it may not be accessible by
            // Hyper-V (e.g., if it is in a WSL filesystem).
            let igvm_file = self.temp_dir.path().join("igvm.bin");
            fs_err::copy(src_igvm_file, &igvm_file).context("failed to copy igvm file")?;
            acl_read_for_vm(&igvm_file, Some(*vm.vmid()))
                .context("failed to set ACL for igvm file")?;

            // TODO: only increase VTL2 memory on debug builds
            vm.set_openhcl_firmware(
                &igvm_file,
                // don't increase VTL2 memory on CVMs
                !matches!(
                    self.guest_state_isolation_type,
                    powershell::HyperVGuestStateIsolationType::Vbs
                        | powershell::HyperVGuestStateIsolationType::Snp
                        | powershell::HyperVGuestStateIsolationType::Tdx
                ),
            )?;

            if self.disable_frontpage {
                self.openhcl_command_line
                    .push_str(" OPENHCL_DISABLE_UEFI_FRONTPAGE=1");
            }
            vm.set_vm_firmware_command_line(&self.openhcl_command_line)?;

            // Construct the agent disk.
            let agent_disk_path = self.temp_dir.path().join("paravisor_cidata.vhd");
            {
                let agent_disk = self
                    .openhcl_agent_image
                    .as_ref()
                    .unwrap()
                    .build()
                    .context("failed to build openhcl agent image")?;
                disk_vhd1::Vhd1Disk::make_fixed(agent_disk.as_file())
                    .context("failed to make vhd for agent image")?;
                agent_disk.persist(&agent_disk_path)?;
            }

            let controller_number = vm.add_scsi_controller(2)?;
            vm.add_vhd(
                &agent_disk_path,
                powershell::ControllerType::Scsi,
                Some(0),
                Some(controller_number),
            )?;
        }

        let mut log_tasks = Vec::new();

        let serial_pipe_path = vm.set_vm_com_port(1)?;
        let serial_log_file = self.log_source.log_file("guest")?;
        log_tasks.push(self.driver.spawn("guest-log", {
            let driver = self.driver.clone();
            async move {
                let serial = diag_client::hyperv::open_serial_port(
                    &driver,
                    diag_client::hyperv::ComPortAccessInfo::PortPipePath(&serial_pipe_path),
                )
                .await?;
                crate::log_stream(serial_log_file, PolledPipe::new(&driver, serial)?).await
            }
        }));

        let openhcl_diag_handler = if self.openhcl_igvm.is_some() {
            let openhcl_log_file = self.log_source.log_file("openhcl")?;
            log_tasks.push(self.driver.spawn("openhcl-log", {
                let driver = self.driver.clone();
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
            Some(OpenHclDiagHandler::new(
                diag_client::DiagClient::from_hyperv_id(self.driver.clone(), *vm.vmid()),
            ))
        } else {
            None
        };

        vm.start().await?;

        Ok(PetriVmHyperV {
            config: self,
            vm,
            openhcl_diag_handler,
            log_tasks,
        })
    }

    /// Set the VM to use the specified number of virtual processors.
    pub fn with_processor_topology(mut self, topology: ProcessorTopology) -> Self {
        self.proc_topology = topology;
        self
    }

    /// Inject Windows secure boot templates into the VM's UEFI.
    pub fn with_windows_secure_boot_template(mut self) -> Self {
        if !matches!(self.generation, powershell::HyperVGeneration::Two) {
            panic!("Secure boot templates are only supported for UEFI firmware.");
        }
        self.secure_boot_template = Some(powershell::HyperVSecureBootTemplate::MicrosoftWindows);
        self
    }

    /// Sets a custom OpenHCL IGVM image to use.
    pub fn with_custom_openhcl(mut self, artifact: ResolvedArtifact) -> Self {
        self.openhcl_igvm = Some(artifact);
        self
    }

    /// Appends to the OpenHCL command line.
    pub fn with_openhcl_command_line(mut self, command_line: &str) -> Self {
        assert!(self.openhcl_igvm.is_some());
        self.openhcl_command_line.push(' ');
        self.openhcl_command_line.push_str(command_line);
        self
    }

    /// Adds a file to the agent image.
    pub fn with_agent_file(mut self, name: &str, artifact: ResolvedArtifact) -> Self {
        self.agent_image.add_file(name, artifact);
        self
    }

    /// Adds a file to the OpenHCL agent image.
    pub fn with_openhcl_agent_file(mut self, name: &str, artifact: ResolvedArtifact) -> Self {
        self.openhcl_agent_image
            .as_mut()
            .unwrap()
            .add_file(name, artifact);
        self
    }

    /// Set whether to disable the UEFI frontpage.
    pub fn with_uefi_frontpage(mut self, enable: bool) -> Self {
        self.disable_frontpage = !enable;
        self
    }
}

impl PetriVmHyperV {
    /// Wait for the VM to halt, returning the reason for the halt.
    pub async fn wait_for_halt(&mut self) -> anyhow::Result<HaltReason> {
        self.vm.wait_for_halt().await?;
        Ok(HaltReason::PowerOff) // TODO: Get actual halt reason
    }

    /// Wait for the VM to halt, returning the reason for the halt,
    /// and cleanly tear down the VM.
    pub async fn wait_for_teardown(mut self) -> anyhow::Result<HaltReason> {
        let halt_reason = self.wait_for_halt().await?;
        for t in self.log_tasks {
            _ = t.cancel();
        }
        self.vm.remove()?;
        Ok(halt_reason)
    }

    /// Test that we are able to inspect OpenHCL.
    pub async fn test_inspect_openhcl(&mut self) -> anyhow::Result<()> {
        self.openhcl_diag()?.test_inspect().await
    }

    /// Wait for VTL 2 to report that it is ready to respond to commands.
    /// Will fail if the VM is not running OpenHCL.
    ///
    /// This should only be necessary if you're doing something manual. All
    /// Petri-provided methods will wait for VTL 2 to be ready automatically.
    pub async fn wait_for_vtl2_ready(&mut self) -> anyhow::Result<()> {
        self.openhcl_diag()?.wait_for_vtl2().await
    }

    /// Wait for a connection from a pipette agent running in the guest.
    /// Useful if you've rebooted the vm or are otherwise expecting a fresh connection.
    pub async fn wait_for_agent(&mut self) -> anyhow::Result<PipetteClient> {
        self.wait_for_agent_core(false).await
    }

    /// Waits for a connection from a pipette agent running in the paravisor.
    pub async fn wait_for_vtl2_agent(&mut self) -> anyhow::Result<PipetteClient> {
        // VTL 2's pipette doesn't auto launch, only launch it on demand
        self.launch_vtl2_pipette().await?;
        self.wait_for_agent_core(true).await
    }

    /// Waits for an event emitted by the firmware about its boot status, and
    /// verifies that it is the expected success value.
    ///
    /// * Linux Direct guests do not emit a boot event, so this method immediately returns Ok.
    /// * PCAT guests may not emit an event depending on the PCAT version, this
    ///   method is best effort for them.
    pub async fn wait_for_successful_boot_event(&mut self) -> anyhow::Result<()> {
        self.vm.wait_for_successful_boot_event().await
    }

    /// Waits for an event emitted by the firmware about its boot status, and
    /// returns that status.
    pub async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent> {
        self.vm.wait_for_boot_event().await
    }

    /// Instruct the guest to shutdown via the Hyper-V shutdown IC.
    pub async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()> {
        match kind {
            ShutdownKind::Shutdown => self.vm.stop().await?,
            ShutdownKind::Reboot => self.vm.restart().await?,
        }

        Ok(())
    }

    async fn wait_for_agent_core(&self, set_high_vtl: bool) -> anyhow::Result<PipetteClient> {
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

        let mut socket = PolledSocket::new(&self.config.driver, socket)?.convert();
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
            PolledTimer::new(&self.config.driver)
                .sleep(Duration::from_secs(1))
                .await;
        }

        PipetteClient::new(&self.config.driver, socket, self.config.temp_dir.path())
            .await
            .context("failed to connect to pipette")
    }

    fn openhcl_diag(&self) -> anyhow::Result<&OpenHclDiagHandler> {
        if let Some(ohd) = &self.openhcl_diag_handler {
            Ok(ohd)
        } else {
            anyhow::bail!("VM is not configured with OpenHCL")
        }
    }

    async fn launch_vtl2_pipette(&self) -> anyhow::Result<()> {
        // Start pipette through DiagClient
        let res = self
            .openhcl_diag()?
            .run_vtl2_command(
                "sh",
                &[
                    "-c",
                    "mkdir /cidata && mount LABEL=cidata /cidata && sh -c '/cidata/pipette &'",
                ],
            )
            .await?;

        if !res.exit_status.success() {
            anyhow::bail!("Failed to start VTL 2 pipette: {:?}", res);
        }

        Ok(())
    }
}

/// Error running command
#[derive(Error, Debug)]
pub enum CommandError {
    /// failed to launch command
    #[error("failed to launch command")]
    Launch(#[from] std::io::Error),
    /// command exited with non-zero status
    #[error("command exited with non-zero status ({0}): {1}")]
    Command(std::process::ExitStatus, String),
    /// command output is not utf-8
    #[error("command output is not utf-8")]
    Utf8(#[from] std::string::FromUtf8Error),
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
