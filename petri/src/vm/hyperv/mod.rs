// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod hvc;
pub mod powershell;
pub mod vm;
use vmsocket::VmAddress;
use vmsocket::VmSocket;

use super::ProcessorTopology;
use crate::BootDeviceType;
use crate::Firmware;
use crate::IsolationType;
use crate::NoPetriVmInspector;
use crate::OpenHclConfig;
use crate::OpenHclServicingFlags;
use crate::PetriDiskType;
use crate::PetriHaltReason;
use crate::PetriVmConfig;
use crate::PetriVmResources;
use crate::PetriVmRuntime;
use crate::PetriVmgsDisk;
use crate::PetriVmgsResource;
use crate::PetriVmmBackend;
use crate::SecureBootTemplate;
use crate::ShutdownKind;
use crate::TpmConfig;
use crate::UefiConfig;
use crate::VmmQuirks;
use crate::disk_image::AgentImage;
use crate::hyperv::powershell::HyperVSecureBootTemplate;
use crate::kmsg_log_task;
use crate::openhcl_diag::OpenHclDiagHandler;
use crate::vm::append_cmdline;
use anyhow::Context;
use async_trait::async_trait;
use disk_backend::sync_wrapper::BlockingDisk;
use disk_vhdmp::VhdmpDisk;
use get_resources::ged::FirmwareEvent;
use pal_async::DefaultDriver;
use pal_async::pipe::PolledPipe;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use petri_artifacts_common::tags::GuestQuirksInner;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use pipette_client::PipetteClient;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempPath;
use vm::HyperVVM;
use vmgs_resources::GuestStateEncryptionPolicy;

/// The Hyper-V Petri backend
pub struct HyperVPetriBackend {}

/// Represents a SCSI Controller addeded to a VM.
#[derive(Debug)]
pub struct HyperVScsiController {
    /// An identifier provided by the test to identify this controller.
    pub test_id: String,

    /// The controller number assigned by Hyper-V.
    pub controller_number: u32,

    /// The target VTL this controller is mapped to (supplied by test).
    pub target_vtl: u32,

    /// The VSID assigned by Hyper-V for this controller.
    pub vsid: guid::Guid,
}

/// Resources needed at runtime for a Hyper-V Petri VM
pub struct HyperVPetriRuntime {
    vm: HyperVVM,
    log_tasks: Vec<Task<anyhow::Result<()>>>,
    _temp_dir: tempfile::TempDir,
    output_dir: PathBuf,
    driver: DefaultDriver,

    is_openhcl: bool,
    is_isolated: bool,

    /// Last VTL2 settings set on this VM.
    vtl2_settings: Option<vtl2_settings_proto::Vtl2Settings>,

    /// Test-added SCSI controllers.
    /// TODO (future PR): push this into `PetriVmConfig` and use in
    /// openvmm as well.
    additional_scsi_controllers: Vec<HyperVScsiController>,
}

/// Additional configuration for a Hyper-V VM.
#[derive(Default, Debug)]
pub struct HyperVPetriConfig {
    /// VTL2 settings to configure on the VM before petri powers it on.
    initial_vtl2_settings: Option<vtl2_settings_proto::Vtl2Settings>,

    /// Test-added SCSI controllers (targeting specific VTLs).
    /// A tuple if test-identifier and targetvtl. Test-identifier
    /// is used so that the test can find a specific controller, if that
    /// is important to the test. These are resolved into a list of
    /// [`HyperVScsiController`] objects stored in the runtime.
    additional_scsi_controllers: Vec<(String, u32)>,
}

impl HyperVPetriConfig {
    /// Add custom VTL 2 settings.
    // TODO: At some point we want to replace uses of this with nicer with_disk,
    // with_nic, etc. methods. And unify this with the same function definition
    // as the openvmm backend.
    pub fn with_custom_vtl2_settings(
        mut self,
        f: impl FnOnce(&mut vtl2_settings_proto::Vtl2Settings),
    ) -> Self {
        if self.initial_vtl2_settings.is_none() {
            self.initial_vtl2_settings = Some(vtl2_settings_proto::Vtl2Settings {
                version: vtl2_settings_proto::vtl2_settings_base::Version::V1.into(),
                fixed: None,
                dynamic: Some(Default::default()),
                namespace_settings: Default::default(),
            });
        }

        f(self.initial_vtl2_settings.as_mut().unwrap());
        self
    }

    /// Add an additional SCSI controller to the VM.
    /// Will be added before the VM starts.
    pub fn with_additional_scsi_controller(mut self, test_id: String, target_vtl: u32) -> Self {
        self.additional_scsi_controllers.push((test_id, target_vtl));
        self
    }
}

#[async_trait]
impl PetriVmmBackend for HyperVPetriBackend {
    type VmmConfig = HyperVPetriConfig;
    type VmRuntime = HyperVPetriRuntime;

    fn check_compat(firmware: &Firmware, arch: MachineArch) -> bool {
        arch == MachineArch::host()
            && !firmware.is_linux_direct()
            && !(firmware.is_pcat() && arch == MachineArch::Aarch64)
    }

    fn quirks(firmware: &Firmware) -> (GuestQuirksInner, VmmQuirks) {
        (firmware.quirks().hyperv, VmmQuirks::default())
    }

    fn default_servicing_flags() -> OpenHclServicingFlags {
        OpenHclServicingFlags {
            enable_nvme_keepalive: false, // TODO: Support NVMe KA in the Hyper-V Petri Backend
            enable_mana_keepalive: false,
            override_version_checks: false,
            stop_timeout_hint_secs: None,
        }
    }

    fn create_guest_dump_disk() -> anyhow::Result<
        Option<(
            Arc<TempPath>,
            Box<dyn FnOnce() -> anyhow::Result<Box<dyn fatfs::ReadWriteSeek>>>,
        )>,
    > {
        // Make a 16 GiB dynamic VHD for guest crash dumps.
        let crash_disk = tempfile::Builder::new()
            .suffix(".vhdx")
            .make(|path| disk_vhdmp::Vhd::create_dynamic(path, 16 * 1024, true))
            .context("error creating crash dump vhdx")?;
        let (crash_disk, crash_disk_path) = crash_disk.into_parts();
        crash_disk
            .attach_for_raw_access(false)
            .context("error attaching crash dump vhdx")?;
        let mut crash_disk = BlockingDisk::new(
            disk_backend::Disk::new(
                VhdmpDisk::new(crash_disk, false).context("failed opening vhdmp")?,
            )
            .unwrap(),
        );

        // Format the VHD with FAT32.
        crate::disk_image::build_fat32_disk_image(
            &mut crash_disk,
            "CRASHDUMP",
            b"crashdump  ",
            &[],
        )
        .context("error writing empty crash disk filesystem")?;

        // Prepare the hook to extract crash dumps after the test.
        let crash_disk_path = Arc::new(crash_disk_path);
        let hook_crash_disk = crash_disk_path.clone();
        let disk_opener = Box::new(move || {
            let mut vhd = Err(anyhow::Error::msg("haven't tried to open the vhd yet"));
            // The VM may not be fully shut down immediately, do some retries
            for _ in 0..5 {
                vhd = VhdmpDisk::open_vhd(hook_crash_disk.as_ref(), true)
                    .context("failed opening vhd");
                if vhd.is_ok() {
                    break;
                } else {
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
            let vhdmp = VhdmpDisk::new(vhd?, true).context("failed opening vhdmp")?;
            Ok(Box::new(BlockingDisk::new(disk_backend::Disk::new(vhdmp).unwrap())) as _)
        });
        Ok(Some((crash_disk_path, disk_opener)))
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
        let PetriVmConfig {
            name,
            arch,
            firmware,
            memory,
            proc_topology,
            agent_image,
            openhcl_agent_image,
            boot_device_type,
            vmgs,
            tpm,
            guest_crash_disk,
        } = config;

        let PetriVmResources { driver, log_source } = resources;

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
                    // Older hosts don't support OpenHCL isolation, so use Trusted Launch
                    None => powershell::HyperVGuestStateIsolationType::TrustedLaunch,
                },
                powershell::HyperVGeneration::Two,
                guest.artifact(),
                Some(uefi_config),
                Some((igvm_path, openhcl_config.clone())),
            ),
        };

        let vmgs_path = {
            // TODO: add support for configuring the TPM in Hyper-V
            // For now, use a persistent vmgs, since Ubuntu VMs with TPM
            // try to install a boot entry and reboot.
            let vmgs = match vmgs {
                PetriVmgsResource::Ephemeral => PetriVmgsResource::Disk(PetriVmgsDisk::default()),
                vmgs => vmgs,
            };

            let lifetime_cli = match &vmgs {
                PetriVmgsResource::Disk(_) => "DEFAULT",
                PetriVmgsResource::ReprovisionOnFailure(_) => "REPROVISION_ON_FAILURE",
                PetriVmgsResource::Reprovision(_) => "REPROVISION",
                PetriVmgsResource::Ephemeral => "EPHEMERAL",
            };

            let (disk, encryption) = match vmgs {
                PetriVmgsResource::Disk(vmgs)
                | PetriVmgsResource::ReprovisionOnFailure(vmgs)
                | PetriVmgsResource::Reprovision(vmgs) => (Some(vmgs.disk), vmgs.encryption_policy),
                PetriVmgsResource::Ephemeral => (None, GuestStateEncryptionPolicy::None(true)),
            };

            let strict = encryption.is_strict();

            let encryption_cli = match encryption {
                GuestStateEncryptionPolicy::Auto => "AUTO",
                GuestStateEncryptionPolicy::None(_) => "NONE",
                GuestStateEncryptionPolicy::GspById(_) => "GSP_BY_ID",
                GuestStateEncryptionPolicy::GspKey(_) => "GSP_KEY",
            };

            // TODO: Error for non-OpenHCL Hyper-V VMs if not supported
            // TODO: Use WMI interfaces when possible
            if let Some((_, config)) = openhcl_config.as_mut() {
                append_cmdline(
                    &mut config.command_line,
                    format!("HCL_GUEST_STATE_LIFETIME={lifetime_cli}"),
                );
                append_cmdline(
                    &mut config.command_line,
                    format!("HCL_GUEST_STATE_ENCRYPTION_POLICY={encryption_cli}"),
                );
                if strict {
                    append_cmdline(&mut config.command_line, "HCL_STRICT_ENCRYPTION_POLICY=1");
                }
            };

            match disk {
                None | Some(PetriDiskType::Memory) => None,
                Some(PetriDiskType::Differencing(parent_path)) => {
                    let diff_disk_path = temp_dir
                        .path()
                        .join(parent_path.file_name().context("path has no filename")?);
                    make_temp_diff_disk(&diff_disk_path, &parent_path).await?;
                    Some(diff_disk_path)
                }
                Some(PetriDiskType::Persistent(path)) => Some(path),
            }
        };

        let mut log_tasks = Vec::new();

        let mut vm = HyperVVM::new(
            &name,
            generation,
            guest_state_isolation_type,
            memory.startup_bytes,
            vmgs_path.as_deref(),
            log_source.clone(),
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
                .or((arch == MachineArch::X86_64
                    && generation == powershell::HyperVGeneration::Two)
                    .then_some({
                        // This is necessary for some tests to pass. TODO: fix.
                        powershell::HyperVApicMode::X2Apic
                    }));
            vm.set_processor(&powershell::HyperVSetVMProcessorArgs {
                count: Some(vp_count),
                apic_mode,
                hw_thread_count_per_core: enable_smt.map(|smt| if smt { 2 } else { 1 }),
                maximum_count_per_numa_node: vps_per_socket,
            })
            .await?;
        }

        if let Some(UefiConfig {
            secure_boot_enabled,
            secure_boot_template,
            disable_frontpage,
            default_boot_always_attempt,
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

            if let Some((_, config)) = openhcl_config.as_mut() {
                append_cmdline(
                    &mut config.command_line,
                    format!(
                        "HCL_DEFAULT_BOOT_ALWAYS_ATTEMPT={}",
                        if *default_boot_always_attempt { 1 } else { 0 }
                    ),
                );
            };
        }

        // Share a single scsi controller for all petri-added drives.
        let petri_vtl0_scsi = vm.add_scsi_controller(0).await?.0;

        if let Some((controller_type, controller_number)) = match boot_device_type {
            BootDeviceType::None => None,
            BootDeviceType::Ide => Some((powershell::ControllerType::Ide, 0)),
            BootDeviceType::Scsi => Some((powershell::ControllerType::Scsi, petri_vtl0_scsi)),
            BootDeviceType::Nvme => todo!("NVMe boot device not yet supported for Hyper-V"),
        } {
            if let Some(artifact) = guest_artifact {
                let controller_location = super::PETRI_VTL0_SCSI_BOOT_LUN;
                let vhd = artifact.get();
                let diff_disk_path = temp_dir.path().join(format!(
                    "{}_{}_{}",
                    controller_number,
                    controller_location,
                    vhd.file_name()
                        .context("path has no filename")?
                        .to_string_lossy()
                ));

                make_temp_diff_disk(&diff_disk_path, vhd).await?;

                vm.add_vhd(
                    &diff_disk_path,
                    controller_type,
                    Some(controller_location),
                    Some(controller_number),
                )
                .await?;
            }
        }

        if let Some(agent_image) = agent_image {
            // Construct the agent disk.
            let agent_disk_path = temp_dir.path().join("cidata.vhd");

            if build_and_persist_agent_image(&agent_image, &agent_disk_path)
                .context("vtl0 agent disk")?
            {
                if agent_image.contains_pipette()
                    && matches!(firmware.os_flavor(), OsFlavor::Windows)
                    && firmware.isolation().is_none()
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

                vm.add_vhd(
                    &agent_disk_path,
                    powershell::ControllerType::Scsi,
                    Some(super::PETRI_VTL0_SCSI_PIPETTE_LUN),
                    Some(petri_vtl0_scsi),
                )
                .await?;
            }
        }

        if let Some((
            src_igvm_file,
            OpenHclConfig {
                vtl2_nvme_boot: _, // TODO, see #1649.
                vmbus_redirect,
                command_line: _,
                log_levels: _,
                vtl2_base_address_type,
            },
        )) = &openhcl_config
        {
            if vtl2_base_address_type.is_some() {
                todo!("custom VTL2 base address type not yet supported for Hyper-V")
            }

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

            let command_line = openhcl_config.as_ref().unwrap().1.command_line();

            vm.set_vm_firmware_command_line(&command_line).await?;

            vm.set_vmbus_redirect(*vmbus_redirect).await?;

            if let Some(agent_image) = openhcl_agent_image {
                let agent_disk_path = temp_dir.path().join("paravisor_cidata.vhd");

                if build_and_persist_agent_image(&agent_image, &agent_disk_path)
                    .context("vtl2 agent disk")?
                {
                    let controller_number = vm.add_scsi_controller(2).await?.0;
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

        if let Some(guest_crash_disk) = guest_crash_disk {
            vm.add_vhd(
                &guest_crash_disk,
                powershell::ControllerType::Scsi,
                Some(super::PETRI_VTL0_SCSI_CRASH_LUN),
                Some(petri_vtl0_scsi),
            )
            .await?;
        }

        let serial_pipe_path = vm.set_vm_com_port(1).await?;
        let serial_log_file = log_source.log_file("guest")?;
        log_tasks.push(driver.spawn(
            "guest-log",
            hyperv_serial_log_task(driver.clone(), serial_pipe_path, serial_log_file),
        ));

        let mut added_controllers = Vec::new();
        let mut vtl2_settings = None;

        // TODO: If OpenHCL is being used, then translate storage through it.
        // (requires changes above where VHDs are added)
        if let Some(modify_vmm_config) = modify_vmm_config {
            let config = modify_vmm_config(HyperVPetriConfig::default());

            tracing::debug!(?config, "additional hyper-v config");

            for (test_id, target_vtl) in config.additional_scsi_controllers {
                let (controller_number, vsid) = vm.add_scsi_controller(target_vtl).await?;
                added_controllers.push(HyperVScsiController {
                    test_id,
                    controller_number,
                    target_vtl,
                    vsid,
                });
            }

            if let Some(settings) = &config.initial_vtl2_settings {
                vm.set_base_vtl2_settings(settings).await?;
                vtl2_settings = Some(settings.clone());
            }
        }

        // Configure the TPM
        if let Some(TpmConfig {
            no_persistent_secrets,
        }) = tpm
        {
            if firmware.is_pcat() {
                anyhow::bail!("hyper-v gen 1 VMs do not support a TPM");
            }
            vm.enable_tpm().await?;

            if firmware.is_openhcl() {
                vm.set_guest_state_isolation_mode(if no_persistent_secrets {
                    powershell::HyperVGuestStateIsolationMode::NoPersistentSecrets
                } else {
                    powershell::HyperVGuestStateIsolationMode::Default
                })
                .await?;
            } else if no_persistent_secrets {
                anyhow::bail!("no persistent secrets requires an hcl");
            }
        } else {
            vm.disable_tpm().await?;
        }

        vm.start().await?;

        Ok(HyperVPetriRuntime {
            vm,
            log_tasks,
            _temp_dir: temp_dir,
            output_dir: log_source.output_dir().to_owned(),
            driver: driver.clone(),
            is_openhcl: openhcl_config.is_some(),
            is_isolated: firmware.isolation().is_some(),
            additional_scsi_controllers: added_controllers,
            vtl2_settings,
        })
    }
}

impl HyperVPetriRuntime {
    /// Gets the VTL2 settings in the `Base` namespace.
    ///
    /// TODO: For now, this is just a copy of whatever was last set via petri.
    /// The function signature (`async`, return `Result`) is to allow for
    /// future changes to actually query the VM for the current settings.
    pub async fn get_base_vtl2_settings(
        &self,
    ) -> anyhow::Result<Option<vtl2_settings_proto::Vtl2Settings>> {
        Ok(self.vtl2_settings.clone())
    }

    /// Get the list of additional SCSI controllers added to this VM (those
    /// configured to be added by the test, as opposed to the petri framework).
    pub fn get_additional_scsi_controllers(&self) -> &[HyperVScsiController] {
        &self.additional_scsi_controllers
    }

    /// Set the VTL2 settings in the `Base` namespace (fixed settings, storage
    /// settings, etc).
    pub async fn set_base_vtl2_settings(
        &mut self,
        settings: &vtl2_settings_proto::Vtl2Settings,
    ) -> anyhow::Result<()> {
        let r = self.vm.set_base_vtl2_settings(settings).await;
        if r.is_ok() {
            self.vtl2_settings = Some(settings.clone());
        }
        r
    }

    /// Adds a VHD with the optionally specified location (a.k.a LUN) to the
    /// optionally specified controller.
    pub async fn add_vhd(
        &mut self,
        vhd: impl AsRef<Path>,
        controller_type: powershell::ControllerType,
        controller_location: Option<u8>,
        controller_number: Option<u32>,
    ) -> anyhow::Result<()> {
        self.vm
            .add_vhd(
                vhd.as_ref(),
                controller_type,
                controller_location,
                controller_number,
            )
            .await
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
        let driver = self.driver.clone();
        let client_core = async move |vm: &HyperVVM| {
            let socket = VmSocket::new().context("failed to create AF_HYPERV socket")?;
            // Extend the default timeout of 2 seconds, as tests are often run
            // in parallel on a host, causing very heavy load on the overall
            // system.
            //
            // TODO: Until #2470 is fixed, extend the timeout even longer to 10
            // seconds to workaround a Windows vmbus bug.
            socket
                .set_connect_timeout(Duration::from_secs(10))
                .context("failed to set connect timeout")?;
            socket
                .set_high_vtl(set_high_vtl)
                .context("failed to set socket for VTL0")?;

            let mut socket = PolledSocket::new(&driver, socket)
                .context("failed to create polled client socket")?
                .convert();
            socket
                .connect(
                    &VmAddress::hyperv_vsock(*vm.vmid(), pipette_client::PIPETTE_VSOCK_PORT).into(),
                )
                .await
                .context("failed to connect")
                .map(|()| socket)
        };

        let driver = self.driver.clone();
        let output_dir = self.output_dir.clone();
        self.vm
            .wait_for_off_or_internal(async move |vm: &HyperVVM| {
                tracing::debug!(set_high_vtl, "attempting to connect to pipette server");
                match client_core(vm).await {
                    Ok(socket) => {
                        tracing::info!(set_high_vtl, "handshaking with pipette");
                        let c = PipetteClient::new(&driver, socket, &output_dir)
                            .await
                            .context("failed to handshake with pipette");
                        tracing::info!(set_high_vtl, "completed pipette handshake");
                        Ok(Some(c?))
                    }
                    Err(err) => {
                        tracing::debug!(
                            err = err.as_ref() as &dyn std::error::Error,
                            "failed to connect to pipette server, retrying",
                        );
                        Ok(None)
                    }
                }
            })
            .await
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

    async fn save_openhcl(
        &mut self,
        _new_openhcl: &ResolvedArtifact,
        _flags: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        anyhow::bail!("saving OpenHCL firmware separately is not yet supported on Hyper-V");
    }

    async fn restore_openhcl(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("restoring OpenHCL firmware separately is not yet supported on Hyper-V");
    }

    async fn update_command_line(&mut self, _command_line: &str) -> anyhow::Result<()> {
        anyhow::bail!("updating command line is not yet supported on Hyper-V");
    }

    fn take_framebuffer_access(&mut self) -> Option<vm::HyperVFramebufferAccess> {
        (!self.is_isolated).then(|| self.vm.get_framebuffer_access())
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.vm.reset().await
    }

    async fn get_guest_state_file(&self) -> anyhow::Result<Option<PathBuf>> {
        Ok(Some(self.vm.get_guest_state_file().await?))
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

async fn make_temp_diff_disk(
    path: impl AsRef<Path>,
    parent_path: impl AsRef<Path>,
) -> anyhow::Result<()> {
    let path = path.as_ref().to_path_buf();
    let parent_path = parent_path.as_ref().to_path_buf();
    tracing::debug!(?path, ?parent_path, "creating differencing vhd");
    blocking::unblock(move || disk_vhdmp::Vhd::create_diff(&path, &parent_path)).await?;
    Ok(())
}
