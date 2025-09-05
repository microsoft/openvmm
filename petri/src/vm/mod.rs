// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Hyper-V VM management
#[cfg(windows)]
pub mod hyperv;
/// OpenVMM VM management
pub mod openvmm;

use crate::PetriLogSource;
use crate::PetriTestParams;
use crate::ShutdownKind;
use crate::disk_image::AgentImage;
use crate::openhcl_diag::OpenHclDiagHandler;
use async_trait::async_trait;
use get_resources::ged::FirmwareEvent;
use pal_async::DefaultDriver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use petri_artifacts_common::tags::GuestQuirks;
use petri_artifacts_common::tags::GuestQuirksInner;
use petri_artifacts_common::tags::InitialRebootCondition;
use petri_artifacts_common::tags::IsOpenhclIgvm;
use petri_artifacts_common::tags::IsTestVmgs;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use petri_artifacts_core::ResolvedOptionalArtifact;
use pipette_client::PipetteClient;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

/// The set of artifacts and resources needed to instantiate a
/// [`PetriVmBuilder`].
pub struct PetriVmArtifacts<T: PetriVmmBackend> {
    /// Artifacts needed to launch the host VMM used for the test
    pub backend: T,
    /// Firmware and/or OS to load into the VM and associated settings
    pub firmware: Firmware,
    /// The architecture of the VM
    pub arch: MachineArch,
    /// Agent to run in the guest
    pub agent_image: Option<AgentImage>,
    /// Agent to run in OpenHCL
    pub openhcl_agent_image: Option<AgentImage>,
}

impl<T: PetriVmmBackend> PetriVmArtifacts<T> {
    /// Resolves the artifacts needed to instantiate a [`PetriVmBuilder`].
    ///
    /// Returns `None` if the supplied configuration is not supported on this platform.
    pub fn new(
        resolver: &ArtifactResolver<'_>,
        firmware: Firmware,
        arch: MachineArch,
        with_vtl0_pipette: bool,
    ) -> Option<Self> {
        if !T::check_compat(&firmware, arch) {
            return None;
        }

        Some(Self {
            backend: T::new(resolver),
            arch,
            agent_image: Some(if with_vtl0_pipette {
                AgentImage::new(firmware.os_flavor()).with_pipette(resolver, arch)
            } else {
                AgentImage::new(firmware.os_flavor())
            }),
            openhcl_agent_image: if firmware.is_openhcl() {
                Some(AgentImage::new(OsFlavor::Linux).with_pipette(resolver, arch))
            } else {
                None
            },
            firmware,
        })
    }
}

/// Petri VM builder
pub struct PetriVmBuilder<T: PetriVmmBackend> {
    /// Artifacts needed to launch the host VMM used for the test
    backend: T,
    /// VM configuration
    config: PetriVmConfig,
    /// Function to modify the VMM-specific configuration
    modify_vmm_config: Option<Box<dyn FnOnce(T::VmmConfig) -> T::VmmConfig + Send>>,
    /// VMM-agnostic resources
    resources: PetriVmResources,

    // VMM-specific quirks for the configured firmware
    quirks: GuestQuirksInner,

    // Test-specific boot behavior expectations.
    // Defaults to expected behavior for firmware configuration.
    expected_boot_event: Option<FirmwareEvent>,
    override_expect_reset: bool,
}

/// Petri VM configuration
pub struct PetriVmConfig {
    /// The name of the VM
    pub name: String,
    /// The architecture of the VM
    pub arch: MachineArch,
    /// Firmware and/or OS to load into the VM and associated settings
    pub firmware: Firmware,
    /// The amount of memory, in bytes, to assign to the VM
    pub memory: MemoryConfig,
    /// The processor tology for the VM
    pub proc_topology: ProcessorTopology,
    /// Agent to run in the guest
    pub agent_image: Option<AgentImage>,
    /// Agent to run in OpenHCL
    pub openhcl_agent_image: Option<AgentImage>,
    /// VM guest state
    pub vmgs: PetriVmgsResource,
}

/// Resources used by a Petri VM during contruction and runtime
pub struct PetriVmResources {
    driver: DefaultDriver,
    output_dir: PathBuf,
    log_source: PetriLogSource,
}

/// Trait for VMM-specific contruction and runtime resources
#[async_trait]
pub trait PetriVmmBackend {
    /// VMM-specific configuration
    type VmmConfig;

    /// Runtime object
    type VmRuntime: PetriVmRuntime;

    /// Check whether the combination of firmware and architecture is
    /// supported on the VMM.
    fn check_compat(firmware: &Firmware, arch: MachineArch) -> bool;

    /// Given a set a guest quirks, select the relevant quirks for this backend.
    fn select_quirks(quirks: GuestQuirks) -> GuestQuirksInner;

    /// Resolve any artifacts needed to use this backend
    fn new(resolver: &ArtifactResolver<'_>) -> Self;

    /// Create and start VM from the generic config using the VMM backend
    async fn run(
        self,
        config: PetriVmConfig,
        modify_vmm_config: Option<impl FnOnce(Self::VmmConfig) -> Self::VmmConfig + Send>,
        resources: &PetriVmResources,
    ) -> anyhow::Result<Self::VmRuntime>;
}

/// A constructed Petri VM
pub struct PetriVm<T: PetriVmmBackend> {
    resources: PetriVmResources,
    runtime: T::VmRuntime,
    watchdog_tasks: Vec<Task<()>>,
    openhcl_diag_handler: Option<OpenHclDiagHandler>,

    arch: MachineArch,
    quirks: GuestQuirksInner,
    expected_boot_event: Option<FirmwareEvent>,
}

impl<T: PetriVmmBackend> PetriVmBuilder<T> {
    /// Create a new VM configuration.
    pub fn new(
        params: &PetriTestParams<'_>,
        artifacts: PetriVmArtifacts<T>,
        driver: &DefaultDriver,
    ) -> anyhow::Result<Self> {
        let quirks = T::select_quirks(artifacts.firmware.quirks());
        let expected_boot_event = artifacts.firmware.expected_boot_event();

        Ok(Self {
            backend: artifacts.backend,
            config: PetriVmConfig {
                name: make_vm_safe_name(params.test_name),
                arch: artifacts.arch,
                firmware: artifacts.firmware,
                memory: Default::default(),
                proc_topology: Default::default(),
                agent_image: artifacts.agent_image,
                openhcl_agent_image: artifacts.openhcl_agent_image,
                // OpenHCL doesn't support ephemeral guest state in release 2505
                vmgs: PetriVmgsResource::Disk(None),
            },
            modify_vmm_config: None,
            resources: PetriVmResources {
                driver: driver.clone(),
                output_dir: params.output_dir.to_owned(),
                log_source: params.logger.clone(),
            },

            quirks,
            expected_boot_event,
            override_expect_reset: false,
        })
    }
}

impl<T: PetriVmmBackend> PetriVmBuilder<T> {
    /// Build and run the VM, then wait for the VM to emit the expected boot
    /// event (if configured). Does not configure and start pipette. Should
    /// only be used for testing platforms that pipette does not support.
    pub async fn run_without_agent(self) -> anyhow::Result<PetriVm<T>> {
        self.run_core().await
    }

    /// Build and run the VM, then wait for the VM to emit the expected boot
    /// event (if configured). Launches pipette and returns a client to it.
    pub async fn run(self) -> anyhow::Result<(PetriVm<T>, PipetteClient)> {
        assert!(self.config.agent_image.is_some());
        assert!(self.config.agent_image.as_ref().unwrap().contains_pipette());

        let mut vm = self.run_core().await?;
        let client = vm.wait_for_agent().await?;
        Ok((vm, client))
    }

    async fn run_core(self) -> anyhow::Result<PetriVm<T>> {
        let arch = self.config.arch;
        let expect_reset = self.expect_reset();

        let mut runtime = self
            .backend
            .run(self.config, self.modify_vmm_config, &self.resources)
            .await?;
        let openhcl_diag_handler = runtime.openhcl_diag();
        let watchdog_tasks = Self::start_watchdog_tasks(&self.resources, &mut runtime)?;

        let mut vm = PetriVm {
            resources: self.resources,
            runtime,
            watchdog_tasks,
            openhcl_diag_handler,

            arch,
            quirks: self.quirks,
            expected_boot_event: self.expected_boot_event,
        };

        if expect_reset {
            vm.wait_for_reset_core().await?;
        }

        vm.wait_for_expected_boot_event().await?;

        Ok(vm)
    }

    fn expect_reset(&self) -> bool {
        // TODO: use presence of TPM here once with_tpm() backend-agnostic.
        self.override_expect_reset
            || matches!(
                (
                    self.quirks.initial_reboot,
                    self.expected_boot_event,
                    &self.config.firmware,
                ),
                (
                    Some(InitialRebootCondition::Always),
                    Some(FirmwareEvent::BootSuccess | FirmwareEvent::BootAttempt),
                    _,
                ) | (
                    Some(InitialRebootCondition::WithOpenHclUefi),
                    Some(FirmwareEvent::BootSuccess | FirmwareEvent::BootAttempt),
                    Firmware::OpenhclUefi { .. },
                )
            )
    }

    fn start_watchdog_tasks(
        resources: &PetriVmResources,
        runtime: &mut T::VmRuntime,
    ) -> anyhow::Result<Vec<Task<()>>> {
        // Our CI environment will kill tests after some time. We want to save
        // some information about the VM if it's still running at that point.
        const TIMEOUT_DURATION_MINUTES: u64 = 6;
        const TIMER_DURATION: Duration = Duration::from_secs(TIMEOUT_DURATION_MINUTES * 60 - 10);

        let mut tasks = Vec::new();

        if let Some(inspector) = runtime.inspector() {
            let mut timer = PolledTimer::new(&resources.driver);
            let log_source = resources.log_source.clone();

            tasks.push(resources.driver.spawn("petri-watchdog-inspect", {
                async move {
                    timer.sleep(TIMER_DURATION).await;
                    tracing::warn!(
                        "Test has been running for almost {TIMEOUT_DURATION_MINUTES} minutes,
                     saving inspect details."
                    );

                    let info = match inspector.inspect().await {
                        Ok(info) => info,
                        Err(e) => {
                            tracing::error!(?e, "Failed to get inspect contents");
                            return;
                        }
                    };

                    if let Err(e) = log_source.write_attachment("timeout_inspect.log", info) {
                        tracing::error!(?e, "Failed to save inspect log");
                        return;
                    }
                    tracing::info!("Watchdog inspect task finished.");
                }
            }));
        }

        if let Some(mut framebuffer_access) = runtime.take_framebuffer_access() {
            let mut timer = PolledTimer::new(&resources.driver);
            let log_source = resources.log_source.clone();

            tasks.push(
                resources
                    .driver
                    .spawn("petri-watchdog-screenshot", async move {
                        let mut image = Vec::new();
                        let mut last_image = Vec::new();
                        loop {
                            timer.sleep(Duration::from_secs(2)).await;
                            tracing::trace!("Taking screenshot.");

                            let VmScreenshotMeta {
                                color,
                                width,
                                height,
                            } = match framebuffer_access.screenshot(&mut image).await {
                                Ok(Some(meta)) => meta,
                                Ok(None) => {
                                    tracing::debug!("VM off, skipping screenshot.");
                                    continue;
                                }
                                Err(e) => {
                                    tracing::error!(?e, "Failed to take screenshot");
                                    continue;
                                }
                            };

                            if image == last_image {
                                tracing::debug!("No change in framebuffer, skipping screenshot.");
                                continue;
                            }

                            let r =
                                log_source
                                    .create_attachment("screenshot.png")
                                    .and_then(|mut f| {
                                        image::write_buffer_with_format(
                                            &mut f,
                                            &image,
                                            width.into(),
                                            height.into(),
                                            color,
                                            image::ImageFormat::Png,
                                        )
                                        .map_err(Into::into)
                                    });

                            if let Err(e) = r {
                                tracing::error!(?e, "Failed to save screenshot");
                            } else {
                                tracing::info!("Screenshot saved.");
                            }

                            std::mem::swap(&mut image, &mut last_image);
                        }
                    }),
            );
        }

        if let Some(openhcl_diag_handler) = runtime.openhcl_diag() {
            let mut timer = PolledTimer::new(&resources.driver);
            let driver = resources.driver.clone();
            let log_source = resources.log_source.clone();
            tasks.push(driver.spawn("petri-watchdog-inspect-vtl2", async move {
                timer.sleep(TIMER_DURATION).await;
                tracing::warn!(
                    "Test has been running for almost {TIMEOUT_DURATION_MINUTES} minutes, saving openhcl inspect details."
                );

                let output = match openhcl_diag_handler.inspect("", None, None).await {
                    Err(e) => {
                        tracing::error!(?e, "Failed to inspect vtl2");
                        return;
                    }
                    Ok(output) => output,
                };

                let formatted_output = format!("{output:#}");
                if let Err(e) = log_source.write_attachment("timeout_openhcl_inspect.log", formatted_output) {
                    tracing::error!(?e, "Failed to save ohcldiag-dev inspect log");
                    return;
                }

                tracing::info!("Watchdog OpenHCL inspect task finished.");
            }));
        }

        Ok(tasks)
    }

    /// Configure the test to expect a boot failure from the VM.
    /// Useful for negative tests.
    pub fn with_expect_boot_failure(mut self) -> Self {
        self.expected_boot_event = Some(FirmwareEvent::BootFailed);
        self
    }

    /// Configure the test to not expect any boot event.
    /// Useful for tests that do not boot a VTL0 guest.
    pub fn with_expect_no_boot_event(mut self) -> Self {
        self.expected_boot_event = None;
        self
    }

    /// Allow the VM to reset once at the beginning of the test. Should only be
    /// used if you are using a special VM configuration that causes the guest
    /// to reboot when it usually wouldn't.
    pub fn with_expect_reset(mut self) -> Self {
        self.override_expect_reset = true;
        self
    }

    /// Set the VM to enable secure boot and inject the templates per OS flavor.
    pub fn with_secure_boot(mut self) -> Self {
        self.config
            .firmware
            .uefi_config_mut()
            .expect("Secure boot is only supported for UEFI firmware.")
            .secure_boot_enabled = true;

        match self.os_flavor() {
            OsFlavor::Windows => self.with_windows_secure_boot_template(),
            OsFlavor::Linux => self.with_uefi_ca_secure_boot_template(),
            _ => panic!(
                "Secure boot unsupported for OS flavor {:?}",
                self.os_flavor()
            ),
        }
    }

    /// Inject Windows secure boot templates into the VM's UEFI.
    pub fn with_windows_secure_boot_template(mut self) -> Self {
        self.config
            .firmware
            .uefi_config_mut()
            .expect("Secure boot is only supported for UEFI firmware.")
            .secure_boot_template = Some(SecureBootTemplate::MicrosoftWindows);
        self
    }

    /// Inject UEFI CA secure boot templates into the VM's UEFI.
    pub fn with_uefi_ca_secure_boot_template(mut self) -> Self {
        self.config
            .firmware
            .uefi_config_mut()
            .expect("Secure boot is only supported for UEFI firmware.")
            .secure_boot_template = Some(SecureBootTemplate::MicrosoftUefiCertificateAuthority);
        self
    }

    /// Set the VM to use the specified processor topology.
    pub fn with_processor_topology(mut self, topology: ProcessorTopology) -> Self {
        self.config.proc_topology = topology;
        self
    }

    /// Set the VM to use the specified processor topology.
    pub fn with_memory(mut self, memory: MemoryConfig) -> Self {
        self.config.memory = memory;
        self
    }

    /// Sets a custom OpenHCL IGVM file to use.
    pub fn with_custom_openhcl(mut self, artifact: ResolvedArtifact<impl IsOpenhclIgvm>) -> Self {
        match &mut self.config.firmware {
            Firmware::OpenhclLinuxDirect { igvm_path, .. }
            | Firmware::OpenhclPcat { igvm_path, .. }
            | Firmware::OpenhclUefi { igvm_path, .. } => {
                *igvm_path = artifact.erase();
            }
            Firmware::LinuxDirect { .. } | Firmware::Uefi { .. } | Firmware::Pcat { .. } => {
                panic!("Custom OpenHCL is only supported for OpenHCL firmware.")
            }
        }
        self
    }

    /// Sets the command line for the paravisor.
    pub fn with_openhcl_command_line(mut self, additional_command_line: &str) -> Self {
        append_cmdline(
            &mut self
                .config
                .firmware
                .openhcl_config_mut()
                .expect("OpenHCL command line is only supported for OpenHCL firmware.")
                .command_line,
            additional_command_line,
        );
        self
    }

    /// Enable confidential filtering, even if the VM is not confidential.
    pub fn with_confidential_filtering(self) -> Self {
        if !self.config.firmware.is_openhcl() {
            panic!("Confidential filtering is only supported for OpenHCL");
        }
        self.with_openhcl_command_line(&format!(
            "{}=1 {}=0",
            underhill_confidentiality::OPENHCL_CONFIDENTIAL_ENV_VAR_NAME,
            underhill_confidentiality::OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME
        ))
    }

    /// Adds a file to the VM's pipette agent image.
    pub fn with_agent_file(mut self, name: &str, artifact: ResolvedArtifact) -> Self {
        self.config
            .agent_image
            .as_mut()
            .expect("no guest pipette")
            .add_file(name, artifact);
        self
    }

    /// Adds a file to the paravisor's pipette agent image.
    pub fn with_openhcl_agent_file(mut self, name: &str, artifact: ResolvedArtifact) -> Self {
        self.config
            .openhcl_agent_image
            .as_mut()
            .expect("no openhcl pipette")
            .add_file(name, artifact);
        self
    }

    /// Sets whether UEFI frontpage is enabled.
    pub fn with_uefi_frontpage(mut self, enable: bool) -> Self {
        self.config
            .firmware
            .uefi_config_mut()
            .expect("UEFI frontpage is only supported for UEFI firmware.")
            .disable_frontpage = !enable;
        self
    }

    /// Run the VM with Enable VMBus relay enabled
    pub fn with_vmbus_redirect(mut self, enable: bool) -> Self {
        self.config
            .firmware
            .openhcl_config_mut()
            .expect("VMBus redirection is only supported for OpenHCL firmware.")
            .vmbus_redirect = enable;
        self
    }

    /// Specify the guest state lifetime for the VM
    pub fn with_guest_state_lifetime(
        mut self,
        guest_state_lifetime: PetriGuestStateLifetime,
    ) -> Self {
        let disk = match self.config.vmgs {
            PetriVmgsResource::Disk(disk)
            | PetriVmgsResource::ReprovisionOnFailure(disk)
            | PetriVmgsResource::Reprovision(disk) => disk,
            PetriVmgsResource::Ephemeral => None,
        };
        self.config.vmgs = match guest_state_lifetime {
            PetriGuestStateLifetime::Disk => PetriVmgsResource::Disk(disk),
            PetriGuestStateLifetime::ReprovisionOnFailure => {
                PetriVmgsResource::ReprovisionOnFailure(disk)
            }
            PetriGuestStateLifetime::Reprovision => PetriVmgsResource::Reprovision(disk),
            PetriGuestStateLifetime::Ephemeral => {
                if disk.is_some() {
                    panic!("attempted to use ephemeral guest state after specifying backing vmgs")
                }
                PetriVmgsResource::Ephemeral
            }
        };
        self
    }

    /// Use the specified backing VMGS file
    pub fn with_backing_vmgs(mut self, disk: ResolvedArtifact<impl IsTestVmgs>) -> Self {
        match &mut self.config.vmgs {
            PetriVmgsResource::Disk(installed_disk)
            | PetriVmgsResource::ReprovisionOnFailure(installed_disk)
            | PetriVmgsResource::Reprovision(installed_disk) => {
                if installed_disk.is_some() {
                    panic!("already specified a backing vmgs file");
                }
                *installed_disk = Some(disk.erase());
            }
            PetriVmgsResource::Ephemeral => {
                panic!("attempted to specify a backing vmgs with ephemeral guest state")
            }
        }
        self
    }

    /// Get VM's guest OS flavor
    pub fn os_flavor(&self) -> OsFlavor {
        self.config.firmware.os_flavor()
    }

    /// Get whether the VM will use OpenHCL
    pub fn is_openhcl(&self) -> bool {
        self.config.firmware.is_openhcl()
    }

    /// Get the backend-specific config builder
    pub fn modify_backend(
        mut self,
        f: impl FnOnce(T::VmmConfig) -> T::VmmConfig + 'static + Send,
    ) -> Self {
        if self.modify_vmm_config.is_some() {
            panic!("only one modify_backend allowed");
        }
        self.modify_vmm_config = Some(Box::new(f));
        self
    }
}

impl<T: PetriVmmBackend> PetriVm<T> {
    /// Wait for the VM to halt, returning the reason for the halt.
    pub async fn wait_for_halt(&mut self) -> anyhow::Result<PetriHaltReason> {
        tracing::info!("Waiting for VM to halt...");
        let halt_reason = self.runtime.wait_for_halt(false).await?;
        tracing::info!("VM halted: {halt_reason:?}");
        Ok(halt_reason)
    }

    /// Wait for the VM to reset. Does not wait for pipette.
    pub async fn wait_for_reset_no_agent(&mut self) -> anyhow::Result<()> {
        self.wait_for_reset_core().await?;
        self.wait_for_expected_boot_event().await?;
        Ok(())
    }

    /// Wait for the VM to reset and pipette to connect.
    pub async fn wait_for_reset(&mut self) -> anyhow::Result<PipetteClient> {
        self.wait_for_reset_no_agent().await?;
        self.wait_for_agent().await
    }

    async fn wait_for_reset_core(&mut self) -> anyhow::Result<()> {
        tracing::info!("Waiting for VM to reset...");
        let halt_reason = self.runtime.wait_for_halt(true).await?;
        if halt_reason != PetriHaltReason::Reset {
            anyhow::bail!("Expected reset, got {halt_reason:?}");
        }
        tracing::info!("VM reset.");
        Ok(())
    }

    /// Wait for the VM to halt, returning the reason for the halt,
    /// and cleanly tear down the VM.
    pub async fn wait_for_teardown(mut self) -> anyhow::Result<PetriHaltReason> {
        let halt_reason = self.wait_for_halt().await?;
        tracing::info!("Cancelling watchdogs...");
        futures::future::join_all(self.watchdog_tasks.into_iter().map(|t| t.cancel())).await;
        tracing::info!("Tearing down VM...");
        self.runtime.teardown().await?;
        Ok(halt_reason)
    }

    /// Wait for the VM to reset
    pub async fn wait_for_clean_teardown(self) -> anyhow::Result<()> {
        let halt_reason = self.wait_for_teardown().await?;
        if halt_reason != PetriHaltReason::PowerOff {
            anyhow::bail!("Expected PowerOff, got {halt_reason:?}");
        }
        tracing::info!("VM was cleanly powered off and torn down.");
        Ok(())
    }

    /// Test that we are able to inspect OpenHCL.
    pub async fn test_inspect_openhcl(&mut self) -> anyhow::Result<()> {
        self.openhcl_diag()?
            .inspect("", None, None)
            .await
            .map(|_| ())
    }

    /// Wait for VTL 2 to report that it is ready to respond to commands.
    /// Will fail if the VM is not running OpenHCL.
    ///
    /// This should only be necessary if you're doing something manual. All
    /// Petri-provided methods will wait for VTL 2 to be ready automatically.
    pub async fn wait_for_vtl2_ready(&mut self) -> anyhow::Result<()> {
        self.openhcl_diag()?.wait_for_vtl2().await
    }

    /// Get the kmsg stream from OpenHCL.
    pub async fn kmsg(&self) -> anyhow::Result<diag_client::kmsg_stream::KmsgStream> {
        self.openhcl_diag()?.kmsg().await
    }

    /// Gets a live core dump of the OpenHCL process specified by 'name' and
    /// writes it to 'path'
    pub async fn openhcl_core_dump(&self, name: &str, path: &Path) -> anyhow::Result<()> {
        self.openhcl_diag()?.core_dump(name, path).await
    }

    /// Crashes the specified openhcl process
    pub async fn openhcl_crash(&self, name: &str) -> anyhow::Result<()> {
        self.openhcl_diag()?.crash(name).await
    }

    /// Wait for a connection from a pipette agent running in the guest.
    /// Useful if you've rebooted the vm or are otherwise expecting a fresh connection.
    async fn wait_for_agent(&mut self) -> anyhow::Result<PipetteClient> {
        self.runtime.wait_for_agent(false).await
    }

    /// Wait for a connection from a pipette agent running in VTL 2.
    /// Useful if you've reset VTL 2 or are otherwise expecting a fresh connection.
    /// Will fail if the VM is not running OpenHCL.
    pub async fn wait_for_vtl2_agent(&mut self) -> anyhow::Result<PipetteClient> {
        // VTL 2's pipette doesn't auto launch, only launch it on demand
        self.launch_vtl2_pipette().await?;
        self.runtime.wait_for_agent(true).await
    }

    /// Waits for an event emitted by the firmware about its boot status, and
    /// verifies that it is the expected success value.
    ///
    /// * Linux Direct guests do not emit a boot event, so this method immediately returns Ok.
    /// * PCAT guests may not emit an event depending on the PCAT version, this
    ///   method is best effort for them.
    async fn wait_for_expected_boot_event(&mut self) -> anyhow::Result<()> {
        if let Some(expected_event) = self.expected_boot_event {
            let event = self.wait_for_boot_event().await?;

            anyhow::ensure!(
                event == expected_event,
                "Did not receive expected boot event"
            );
        } else {
            tracing::warn!("Boot event not emitted for configured firmware or manually ignored.");
        }

        Ok(())
    }

    /// Waits for an event emitted by the firmware about its boot status, and
    /// returns that status.
    async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent> {
        tracing::info!("Waiting for boot event...");
        let boot_event = self.runtime.wait_for_boot_event().await?;
        tracing::info!("Got boot event: {boot_event:?}");
        Ok(boot_event)
    }

    /// Wait for the Hyper-V shutdown IC to be ready and use it to instruct
    /// the guest to shutdown.
    pub async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()> {
        tracing::info!("Waiting for enlightened shutdown to be ready");
        self.runtime.wait_for_enlightened_shutdown_ready().await?;

        // all guests used in testing have been observed to intermittently
        // drop shutdown requests if they are sent too soon after the shutdown
        // ic comes online. give them a little extra time.
        // TODO: use a different method of determining whether the VM has booted
        // or debug and fix the shutdown IC.
        let mut wait_time = Duration::from_secs(5);

        // some guests need even more time
        if let Some(duration) = self.quirks.hyperv_shutdown_ic_sleep {
            wait_time += duration;
        }

        tracing::info!(
            "Shutdown IC reported ready, waiting for an extra {}s",
            wait_time.as_secs()
        );
        PolledTimer::new(&self.resources.driver)
            .sleep(wait_time)
            .await;

        tracing::info!("Sending enlightened shutdown command");
        self.runtime.send_enlightened_shutdown(kind).await
    }

    /// Instruct the OpenHCL to restart the VTL2 paravisor. Will fail if the VM
    /// is not running OpenHCL. Will also fail if the VM is not running.
    pub async fn restart_openhcl(
        &mut self,
        new_openhcl: ResolvedArtifact<impl IsOpenhclIgvm>,
        flags: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        self.runtime
            .restart_openhcl(&new_openhcl.erase(), flags)
            .await
    }

    /// Get VM's guest OS flavor
    pub fn arch(&self) -> MachineArch {
        self.arch
    }

    /// Get the inner runtime backend to make backend-specific calls
    pub fn backend(&mut self) -> &mut T::VmRuntime {
        &mut self.runtime
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

    fn openhcl_diag(&self) -> anyhow::Result<&OpenHclDiagHandler> {
        if let Some(ohd) = self.openhcl_diag_handler.as_ref() {
            Ok(ohd)
        } else {
            anyhow::bail!("VM is not configured with OpenHCL")
        }
    }
}

/// A running VM that tests can interact with.
#[async_trait]
pub trait PetriVmRuntime {
    /// Interface for inspecting the VM
    type VmInspector: PetriVmInspector;
    /// Interface for accessing the framebuffer
    type VmFramebufferAccess: PetriVmFramebufferAccess;

    /// Cleanly tear down the VM immediately.
    async fn teardown(self) -> anyhow::Result<()>;
    /// Wait for the VM to halt, returning the reason for the halt. The VM
    /// should automatically restart the VM on reset if `allow_reset` is true.
    async fn wait_for_halt(&mut self, allow_reset: bool) -> anyhow::Result<PetriHaltReason>;
    /// Wait for a connection from a pipette agent
    async fn wait_for_agent(&mut self, set_high_vtl: bool) -> anyhow::Result<PipetteClient>;
    /// Get an OpenHCL diagnostics handler for the VM
    fn openhcl_diag(&self) -> Option<OpenHclDiagHandler>;
    /// Waits for an event emitted by the firmware about its boot status, and
    /// returns that status.
    async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent>;
    /// Waits for the Hyper-V shutdown IC to be ready
    // TODO: return a receiver that will be closed when it is no longer ready.
    async fn wait_for_enlightened_shutdown_ready(&mut self) -> anyhow::Result<()>;
    /// Instruct the guest to shutdown via the Hyper-V shutdown IC.
    async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()>;
    /// Instruct the OpenHCL to restart the VTL2 paravisor. Will fail if the VM
    /// is not running OpenHCL. Will also fail if the VM is not running.
    async fn restart_openhcl(
        &mut self,
        new_openhcl: &ResolvedArtifact,
        flags: OpenHclServicingFlags,
    ) -> anyhow::Result<()>;
    /// If the backend supports it, get an inspect interface
    fn inspector(&self) -> Option<Self::VmInspector> {
        None
    }
    /// If the backend supports it, take the screenshot interface
    /// (subsequent calls may return None).
    fn take_framebuffer_access(&mut self) -> Option<Self::VmFramebufferAccess> {
        None
    }
}

/// Interface for getting information about the state of the VM
#[async_trait]
pub trait PetriVmInspector: Send + 'static {
    /// Get information about the state of the VM
    async fn inspect(&self) -> anyhow::Result<String>;
}

/// Use this for the associated type if not supported
pub struct NoPetriVmInspector;
#[async_trait]
impl PetriVmInspector for NoPetriVmInspector {
    async fn inspect(&self) -> anyhow::Result<String> {
        unreachable!()
    }
}

/// Raw VM screenshot
pub struct VmScreenshotMeta {
    /// color encoding used by the image
    pub color: image::ExtendedColorType,
    /// x dimension
    pub width: u16,
    /// y dimension
    pub height: u16,
}

/// Interface for getting screenshots of the VM
#[async_trait]
pub trait PetriVmFramebufferAccess: Send + 'static {
    /// Populates the provided buffer with a screenshot of the VM,
    /// returning the dimensions and color type.
    async fn screenshot(&mut self, image: &mut Vec<u8>)
    -> anyhow::Result<Option<VmScreenshotMeta>>;
}

/// Use this for the associated type if not supported
pub struct NoPetriVmFramebufferAccess;
#[async_trait]
impl PetriVmFramebufferAccess for NoPetriVmFramebufferAccess {
    async fn screenshot(
        &mut self,
        _image: &mut Vec<u8>,
    ) -> anyhow::Result<Option<VmScreenshotMeta>> {
        unreachable!()
    }
}

/// Common processor topology information for the VM.
pub struct ProcessorTopology {
    /// The number of virtual processors.
    pub vp_count: u32,
    /// Whether SMT (hyperthreading) is enabled.
    pub enable_smt: Option<bool>,
    /// The number of virtual processors per socket.
    pub vps_per_socket: Option<u32>,
    /// The APIC configuration (x86-64 only).
    pub apic_mode: Option<ApicMode>,
}

impl Default for ProcessorTopology {
    fn default() -> Self {
        Self {
            vp_count: 2,
            enable_smt: None,
            vps_per_socket: None,
            apic_mode: None,
        }
    }
}

/// The APIC mode for the VM.
#[derive(Debug, Clone, Copy)]
pub enum ApicMode {
    /// xAPIC mode only.
    Xapic,
    /// x2APIC mode supported but not enabled at boot.
    X2apicSupported,
    /// x2APIC mode enabled at boot.
    X2apicEnabled,
}

/// Common memory configuration information for the VM.
pub struct MemoryConfig {
    /// Specifies the amount of memory, in bytes, to assign to the
    /// virtual machine.
    pub startup_bytes: u64,
    /// Specifies the minimum and maximum amount of dynamic memory, in bytes.
    ///
    /// Dynamic memory will be disabled if this is `None`.
    pub dynamic_memory_range: Option<(u64, u64)>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            startup_bytes: 0x1_0000_0000,
            dynamic_memory_range: None,
        }
    }
}

/// UEFI firmware configuration
#[derive(Debug)]
pub struct UefiConfig {
    /// Enable secure boot
    pub secure_boot_enabled: bool,
    /// Secure boot template
    pub secure_boot_template: Option<SecureBootTemplate>,
    /// Disable the UEFI frontpage which will cause the VM to shutdown instead when unable to boot.
    pub disable_frontpage: bool,
}

impl Default for UefiConfig {
    fn default() -> Self {
        Self {
            secure_boot_enabled: false,
            secure_boot_template: None,
            disable_frontpage: true,
        }
    }
}

/// OpenHCL configuration
#[derive(Debug, Default, Clone)]
pub struct OpenHclConfig {
    /// Emulate SCSI via NVME to VTL2, with the provided namespace ID on
    /// the controller with `BOOT_NVME_INSTANCE`.
    pub vtl2_nvme_boot: bool,
    /// Whether to enable VMBus redirection
    pub vmbus_redirect: bool,
    /// Command line to pass to OpenHCL
    pub command_line: Option<String>,
}

/// Firmware to load into the test VM.
#[derive(Debug)]
pub enum Firmware {
    /// Boot Linux directly, without any firmware.
    LinuxDirect {
        /// The kernel to boot.
        kernel: ResolvedArtifact,
        /// The initrd to use.
        initrd: ResolvedArtifact,
    },
    /// Boot Linux directly, without any firmware, with OpenHCL in VTL2.
    OpenhclLinuxDirect {
        /// The path to the IGVM file to use.
        igvm_path: ResolvedArtifact,
        /// OpenHCL configuration
        openhcl_config: OpenHclConfig,
    },
    /// Boot a PCAT-based VM.
    Pcat {
        /// The guest OS the VM will boot into.
        guest: PcatGuest,
        /// The firmware to use.
        bios_firmware: ResolvedOptionalArtifact,
        /// The SVGA firmware to use.
        svga_firmware: ResolvedOptionalArtifact,
    },
    /// Boot a PCAT-based VM with OpenHCL in VTL2.
    OpenhclPcat {
        /// The guest OS the VM will boot into.
        guest: PcatGuest,
        /// The path to the IGVM file to use.
        igvm_path: ResolvedArtifact,
        /// The firmware to use.
        bios_firmware: ResolvedOptionalArtifact,
        /// The SVGA firmware to use.
        svga_firmware: ResolvedOptionalArtifact,
        /// OpenHCL configuration
        openhcl_config: OpenHclConfig,
    },
    /// Boot a UEFI-based VM.
    Uefi {
        /// The guest OS the VM will boot into.
        guest: UefiGuest,
        /// The firmware to use.
        uefi_firmware: ResolvedArtifact,
        /// UEFI configuration
        uefi_config: UefiConfig,
    },
    /// Boot a UEFI-based VM with OpenHCL in VTL2.
    OpenhclUefi {
        /// The guest OS the VM will boot into.
        guest: UefiGuest,
        /// The isolation type of the VM.
        isolation: Option<IsolationType>,
        /// The path to the IGVM file to use.
        igvm_path: ResolvedArtifact,
        /// UEFI configuration
        uefi_config: UefiConfig,
        /// OpenHCL configuration
        openhcl_config: OpenHclConfig,
    },
}

impl Firmware {
    /// Constructs a standard [`Firmware::LinuxDirect`] configuration.
    pub fn linux_direct(resolver: &ArtifactResolver<'_>, arch: MachineArch) -> Self {
        use petri_artifacts_vmm_test::artifacts::loadable::*;
        match arch {
            MachineArch::X86_64 => Firmware::LinuxDirect {
                kernel: resolver.require(LINUX_DIRECT_TEST_KERNEL_X64).erase(),
                initrd: resolver.require(LINUX_DIRECT_TEST_INITRD_X64).erase(),
            },
            MachineArch::Aarch64 => Firmware::LinuxDirect {
                kernel: resolver.require(LINUX_DIRECT_TEST_KERNEL_AARCH64).erase(),
                initrd: resolver.require(LINUX_DIRECT_TEST_INITRD_AARCH64).erase(),
            },
        }
    }

    /// Constructs a standard [`Firmware::OpenhclLinuxDirect`] configuration.
    pub fn openhcl_linux_direct(resolver: &ArtifactResolver<'_>, arch: MachineArch) -> Self {
        use petri_artifacts_vmm_test::artifacts::openhcl_igvm::*;
        match arch {
            MachineArch::X86_64 => Firmware::OpenhclLinuxDirect {
                igvm_path: resolver.require(LATEST_LINUX_DIRECT_TEST_X64).erase(),
                openhcl_config: Default::default(),
            },
            MachineArch::Aarch64 => todo!("Linux direct not yet supported on aarch64"),
        }
    }

    /// Constructs a standard [`Firmware::Pcat`] configuration.
    pub fn pcat(resolver: &ArtifactResolver<'_>, guest: PcatGuest) -> Self {
        use petri_artifacts_vmm_test::artifacts::loadable::*;
        Firmware::Pcat {
            guest,
            bios_firmware: resolver.try_require(PCAT_FIRMWARE_X64).erase(),
            svga_firmware: resolver.try_require(SVGA_FIRMWARE_X64).erase(),
        }
    }

    /// Constructs a standard [`Firmware::Uefi`] configuration.
    pub fn uefi(resolver: &ArtifactResolver<'_>, arch: MachineArch, guest: UefiGuest) -> Self {
        use petri_artifacts_vmm_test::artifacts::loadable::*;
        let uefi_firmware = match arch {
            MachineArch::X86_64 => resolver.require(UEFI_FIRMWARE_X64).erase(),
            MachineArch::Aarch64 => resolver.require(UEFI_FIRMWARE_AARCH64).erase(),
        };
        Firmware::Uefi {
            guest,
            uefi_firmware,
            uefi_config: Default::default(),
        }
    }

    /// Constructs a standard [`Firmware::OpenhclUefi`] configuration.
    pub fn openhcl_uefi(
        resolver: &ArtifactResolver<'_>,
        arch: MachineArch,
        guest: UefiGuest,
        isolation: Option<IsolationType>,
        vtl2_nvme_boot: bool,
    ) -> Self {
        use petri_artifacts_vmm_test::artifacts::openhcl_igvm::*;
        let igvm_path = match arch {
            MachineArch::X86_64 if isolation.is_some() => resolver.require(LATEST_CVM_X64).erase(),
            MachineArch::X86_64 => resolver.require(LATEST_STANDARD_X64).erase(),
            MachineArch::Aarch64 => resolver.require(LATEST_STANDARD_AARCH64).erase(),
        };
        Firmware::OpenhclUefi {
            guest,
            isolation,
            igvm_path,
            uefi_config: Default::default(),
            openhcl_config: OpenHclConfig {
                vtl2_nvme_boot,
                ..Default::default()
            },
        }
    }

    fn is_openhcl(&self) -> bool {
        match self {
            Firmware::OpenhclLinuxDirect { .. }
            | Firmware::OpenhclUefi { .. }
            | Firmware::OpenhclPcat { .. } => true,
            Firmware::LinuxDirect { .. } | Firmware::Pcat { .. } | Firmware::Uefi { .. } => false,
        }
    }

    fn isolation(&self) -> Option<IsolationType> {
        match self {
            Firmware::OpenhclUefi { isolation, .. } => *isolation,
            Firmware::LinuxDirect { .. }
            | Firmware::Pcat { .. }
            | Firmware::Uefi { .. }
            | Firmware::OpenhclLinuxDirect { .. }
            | Firmware::OpenhclPcat { .. } => None,
        }
    }

    fn is_linux_direct(&self) -> bool {
        match self {
            Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => true,
            Firmware::Pcat { .. }
            | Firmware::Uefi { .. }
            | Firmware::OpenhclUefi { .. }
            | Firmware::OpenhclPcat { .. } => false,
        }
    }

    fn is_pcat(&self) -> bool {
        match self {
            Firmware::Pcat { .. } | Firmware::OpenhclPcat { .. } => true,
            Firmware::Uefi { .. }
            | Firmware::OpenhclUefi { .. }
            | Firmware::LinuxDirect { .. }
            | Firmware::OpenhclLinuxDirect { .. } => false,
        }
    }

    fn os_flavor(&self) -> OsFlavor {
        match self {
            Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => OsFlavor::Linux,
            Firmware::Uefi {
                guest: UefiGuest::GuestTestUefi { .. } | UefiGuest::None,
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::GuestTestUefi { .. } | UefiGuest::None,
                ..
            } => OsFlavor::Uefi,
            Firmware::Pcat {
                guest: PcatGuest::Vhd(cfg),
                ..
            }
            | Firmware::OpenhclPcat {
                guest: PcatGuest::Vhd(cfg),
                ..
            }
            | Firmware::Uefi {
                guest: UefiGuest::Vhd(cfg),
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::Vhd(cfg),
                ..
            } => cfg.os_flavor,
            Firmware::Pcat {
                guest: PcatGuest::Iso(cfg),
                ..
            }
            | Firmware::OpenhclPcat {
                guest: PcatGuest::Iso(cfg),
                ..
            } => cfg.os_flavor,
        }
    }

    fn quirks(&self) -> GuestQuirks {
        match self {
            Firmware::Pcat {
                guest: PcatGuest::Vhd(cfg),
                ..
            }
            | Firmware::Uefi {
                guest: UefiGuest::Vhd(cfg),
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::Vhd(cfg),
                ..
            } => cfg.quirks.clone(),
            Firmware::Pcat {
                guest: PcatGuest::Iso(cfg),
                ..
            } => cfg.quirks.clone(),
            _ => Default::default(),
        }
    }

    fn expected_boot_event(&self) -> Option<FirmwareEvent> {
        match self {
            Firmware::LinuxDirect { .. }
            | Firmware::OpenhclLinuxDirect { .. }
            | Firmware::Uefi {
                guest: UefiGuest::GuestTestUefi(_),
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::GuestTestUefi(_),
                ..
            } => None,
            Firmware::Pcat { .. } | Firmware::OpenhclPcat { .. } => {
                // TODO: Handle older PCAT versions that don't fire the event
                Some(FirmwareEvent::BootAttempt)
            }
            Firmware::Uefi {
                guest: UefiGuest::None,
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::None,
                ..
            } => Some(FirmwareEvent::NoBootDevice),
            Firmware::Uefi { .. } | Firmware::OpenhclUefi { .. } => {
                Some(FirmwareEvent::BootSuccess)
            }
        }
    }

    fn openhcl_config(&self) -> Option<&OpenHclConfig> {
        match self {
            Firmware::OpenhclLinuxDirect { openhcl_config, .. }
            | Firmware::OpenhclUefi { openhcl_config, .. }
            | Firmware::OpenhclPcat { openhcl_config, .. } => Some(openhcl_config),
            Firmware::LinuxDirect { .. } | Firmware::Pcat { .. } | Firmware::Uefi { .. } => None,
        }
    }

    fn openhcl_config_mut(&mut self) -> Option<&mut OpenHclConfig> {
        match self {
            Firmware::OpenhclLinuxDirect { openhcl_config, .. }
            | Firmware::OpenhclUefi { openhcl_config, .. }
            | Firmware::OpenhclPcat { openhcl_config, .. } => Some(openhcl_config),
            Firmware::LinuxDirect { .. } | Firmware::Pcat { .. } | Firmware::Uefi { .. } => None,
        }
    }

    fn uefi_config(&self) -> Option<&UefiConfig> {
        match self {
            Firmware::Uefi { uefi_config, .. } | Firmware::OpenhclUefi { uefi_config, .. } => {
                Some(uefi_config)
            }
            Firmware::LinuxDirect { .. }
            | Firmware::OpenhclLinuxDirect { .. }
            | Firmware::Pcat { .. }
            | Firmware::OpenhclPcat { .. } => None,
        }
    }

    fn uefi_config_mut(&mut self) -> Option<&mut UefiConfig> {
        match self {
            Firmware::Uefi { uefi_config, .. } | Firmware::OpenhclUefi { uefi_config, .. } => {
                Some(uefi_config)
            }
            Firmware::LinuxDirect { .. }
            | Firmware::OpenhclLinuxDirect { .. }
            | Firmware::Pcat { .. }
            | Firmware::OpenhclPcat { .. } => None,
        }
    }
}

/// The guest the VM will boot into. A boot drive with the chosen setup
/// will be automatically configured.
#[derive(Debug)]
pub enum PcatGuest {
    /// Mount a VHD as the boot drive.
    Vhd(BootImageConfig<boot_image_type::Vhd>),
    /// Mount an ISO as the CD/DVD drive.
    Iso(BootImageConfig<boot_image_type::Iso>),
}

impl PcatGuest {
    fn artifact(&self) -> &ResolvedArtifact {
        match self {
            PcatGuest::Vhd(disk) => &disk.artifact,
            PcatGuest::Iso(disk) => &disk.artifact,
        }
    }
}

/// The guest the VM will boot into. A boot drive with the chosen setup
/// will be automatically configured.
#[derive(Debug)]
pub enum UefiGuest {
    /// Mount a VHD as the boot drive.
    Vhd(BootImageConfig<boot_image_type::Vhd>),
    /// The UEFI test image produced by our guest-test infrastructure.
    GuestTestUefi(ResolvedArtifact),
    /// No guest, just the firmware.
    None,
}

impl UefiGuest {
    /// Construct a standard [`UefiGuest::GuestTestUefi`] configuration.
    pub fn guest_test_uefi(resolver: &ArtifactResolver<'_>, arch: MachineArch) -> Self {
        use petri_artifacts_vmm_test::artifacts::test_vhd::*;
        let artifact = match arch {
            MachineArch::X86_64 => resolver.require(GUEST_TEST_UEFI_X64).erase(),
            MachineArch::Aarch64 => resolver.require(GUEST_TEST_UEFI_AARCH64).erase(),
        };
        UefiGuest::GuestTestUefi(artifact)
    }

    fn artifact(&self) -> Option<&ResolvedArtifact> {
        match self {
            UefiGuest::Vhd(vhd) => Some(&vhd.artifact),
            UefiGuest::GuestTestUefi(p) => Some(p),
            UefiGuest::None => None,
        }
    }
}

/// Type-tags for [`BootImageConfig`](super::BootImageConfig)
pub mod boot_image_type {
    mod private {
        pub trait Sealed {}
        impl Sealed for super::Vhd {}
        impl Sealed for super::Iso {}
    }

    /// Private trait use to seal the set of artifact types BootImageType
    /// supports.
    pub trait BootImageType: private::Sealed {}

    /// BootImageConfig for a VHD file
    #[derive(Debug)]
    pub enum Vhd {}

    /// BootImageConfig for an ISO file
    #[derive(Debug)]
    pub enum Iso {}

    impl BootImageType for Vhd {}
    impl BootImageType for Iso {}
}

/// Configuration information for the boot drive of the VM.
#[derive(Debug)]
pub struct BootImageConfig<T: boot_image_type::BootImageType> {
    /// Artifact handle corresponding to the boot media.
    artifact: ResolvedArtifact,
    /// The OS flavor.
    os_flavor: OsFlavor,
    /// Any quirks needed to boot the guest.
    ///
    /// Most guests should not need any quirks, and can use `Default`.
    quirks: GuestQuirks,
    /// Marker denoting what type of media `artifact` corresponds to
    _type: core::marker::PhantomData<T>,
}

impl BootImageConfig<boot_image_type::Vhd> {
    /// Create a new BootImageConfig from a VHD artifact handle
    pub fn from_vhd<A>(artifact: ResolvedArtifact<A>) -> Self
    where
        A: petri_artifacts_common::tags::IsTestVhd,
    {
        BootImageConfig {
            artifact: artifact.erase(),
            os_flavor: A::OS_FLAVOR,
            quirks: A::quirks(),
            _type: std::marker::PhantomData,
        }
    }
}

impl BootImageConfig<boot_image_type::Iso> {
    /// Create a new BootImageConfig from an ISO artifact handle
    pub fn from_iso<A>(artifact: ResolvedArtifact<A>) -> Self
    where
        A: petri_artifacts_common::tags::IsTestIso,
    {
        BootImageConfig {
            artifact: artifact.erase(),
            os_flavor: A::OS_FLAVOR,
            quirks: A::quirks(),
            _type: std::marker::PhantomData,
        }
    }
}

/// Isolation type
#[derive(Debug, Clone, Copy)]
pub enum IsolationType {
    /// VBS
    Vbs,
    /// SNP
    Snp,
    /// TDX
    Tdx,
}

/// Flags controlling servicing behavior.
#[derive(Default, Debug, Clone, Copy)]
pub struct OpenHclServicingFlags {
    /// Preserve DMA memory for NVMe devices if supported.
    pub enable_nvme_keepalive: bool,
    /// Skip any logic that the vmm may have to ignore servicing updates if the supplied igvm file version is not different than the one currently running.
    pub override_version_checks: bool,
    /// Hint to the OpenHCL runtime how much time to wait when stopping / saving the OpenHCL.
    pub stop_timeout_hint_secs: Option<u16>,
}

/// Petri VM guest state resource
#[derive(Debug, Clone)]
pub enum PetriVmgsResource {
    /// Use disk to store guest state
    Disk(Option<ResolvedArtifact>),
    /// Use disk to store guest state, reformatting if corrupted.
    ReprovisionOnFailure(Option<ResolvedArtifact>),
    /// Format and use disk to store guest state
    Reprovision(Option<ResolvedArtifact>),
    /// Store guest state in memory
    Ephemeral,
}

/// Petri VM guest state lifetime
#[derive(Debug, Clone, Copy)]
pub enum PetriGuestStateLifetime {
    /// Use a differencing disk backed by a blank, tempory VMGS file
    /// or other artifact if one is provided
    Disk,
    /// Same as default, except reformat the backing disk if corrupted
    ReprovisionOnFailure,
    /// Same as default, except reformat the backing disk
    Reprovision,
    /// Store guest state in memory (no backing disk)
    Ephemeral,
}

/// UEFI secure boot template
#[derive(Debug, Clone, Copy)]
pub enum SecureBootTemplate {
    /// The Microsoft Windows template.
    MicrosoftWindows,
    /// The Microsoft UEFI certificate authority template.
    MicrosoftUefiCertificateAuthority,
}

/// Creates a VM-safe name that respects platform limitations.
///
/// Hyper-V limits VM names to 100 characters. For names that exceed this limit,
/// this function truncates to 96 characters and appends a 4-character hash
/// to ensure uniqueness while staying within the limit.
fn make_vm_safe_name(name: &str) -> String {
    const MAX_VM_NAME_LENGTH: usize = 100;
    const HASH_LENGTH: usize = 4;
    const MAX_PREFIX_LENGTH: usize = MAX_VM_NAME_LENGTH - HASH_LENGTH;

    if name.len() <= MAX_VM_NAME_LENGTH {
        name.to_owned()
    } else {
        // Create a hash of the full name for uniqueness
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        let hash = hasher.finish();

        // Format hash as a 4-character hex string
        let hash_suffix = format!("{:04x}", hash & 0xFFFF);

        // Truncate the name and append the hash
        let truncated = &name[..MAX_PREFIX_LENGTH];
        tracing::debug!(
            "VM name too long ({}), truncating '{}' to '{}{}'",
            name.len(),
            name,
            truncated,
            hash_suffix
        );

        format!("{}{}", truncated, hash_suffix)
    }
}

/// The reason that the VM halted
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PetriHaltReason {
    /// The vm powered off
    PowerOff,
    /// The vm reset
    Reset,
    /// The vm hibernated
    Hibernate,
    /// The vm triple faulted
    TripleFault,
    /// The vm halted for some other reason
    Other,
}

fn append_cmdline(cmd: &mut Option<String>, add_cmd: &str) {
    if let Some(cmd) = cmd.as_mut() {
        cmd.push(' ');
        cmd.push_str(add_cmd);
    } else {
        *cmd = Some(add_cmd.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::make_vm_safe_name;

    #[test]
    fn test_short_names_unchanged() {
        let short_name = "short_test_name";
        assert_eq!(make_vm_safe_name(short_name), short_name);
    }

    #[test]
    fn test_exactly_100_chars_unchanged() {
        let name_100 = "a".repeat(100);
        assert_eq!(make_vm_safe_name(&name_100), name_100);
    }

    #[test]
    fn test_long_name_truncated() {
        let long_name = "multiarch::openhcl_servicing::hyperv_openhcl_uefi_aarch64_ubuntu_2404_server_aarch64_openhcl_servicing";
        let result = make_vm_safe_name(long_name);

        // Should be exactly 100 characters
        assert_eq!(result.len(), 100);

        // Should start with the truncated prefix
        assert!(result.starts_with("multiarch::openhcl_servicing::hyperv_openhcl_uefi_aarch64_ubuntu_2404_server_aarch64_ope"));

        // Should end with a 4-character hash
        let suffix = &result[96..];
        assert_eq!(suffix.len(), 4);
        // Should be valid hex
        assert!(u16::from_str_radix(suffix, 16).is_ok());
    }

    #[test]
    fn test_deterministic_results() {
        let long_name = "very_long_test_name_that_exceeds_the_100_character_limit_and_should_be_truncated_consistently_every_time";
        let result1 = make_vm_safe_name(long_name);
        let result2 = make_vm_safe_name(long_name);

        assert_eq!(result1, result2);
        assert_eq!(result1.len(), 100);
    }

    #[test]
    fn test_different_names_different_hashes() {
        let name1 = "very_long_test_name_that_definitely_exceeds_the_100_character_limit_and_should_be_truncated_by_the_function_version_1";
        let name2 = "very_long_test_name_that_definitely_exceeds_the_100_character_limit_and_should_be_truncated_by_the_function_version_2";

        let result1 = make_vm_safe_name(name1);
        let result2 = make_vm_safe_name(name2);

        // Both should be 100 chars
        assert_eq!(result1.len(), 100);
        assert_eq!(result2.len(), 100);

        // Should have different suffixes since the full names are different
        assert_ne!(result1, result2);
        assert_ne!(&result1[96..], &result2[96..]);
    }
}
