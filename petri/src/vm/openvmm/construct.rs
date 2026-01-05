// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Contains [`PetriVmConfigOpenVmm::new`], which builds a [`PetriVmConfigOpenVmm`] with all
//! default settings for a given [`Firmware`] and [`MachineArch`].

use super::BOOT_NVME_LUN;
use super::BOOT_NVME_NSID;
use super::PARAVISOR_BOOT_NVME_INSTANCE;
use super::PetriVmConfigOpenVmm;
use super::PetriVmResourcesOpenVmm;
use super::SCSI_INSTANCE;
use super::memdiff_disk;
use crate::BootDeviceType;
use crate::Firmware;
use crate::IsolationType;
use crate::MemoryConfig;
use crate::OpenHclConfig;
use crate::PcatGuest;
use crate::PetriLogSource;
use crate::PetriVmConfig;
use crate::PetriVmResources;
use crate::PetriVmgsResource;
use crate::ProcessorTopology;
use crate::SIZE_1_GB;
use crate::SecureBootTemplate;
use crate::TpmConfig;
use crate::UefiConfig;
use crate::UefiGuest;
use crate::linux_direct_serial_agent::LinuxDirectSerialAgent;
use crate::openvmm::BOOT_NVME_INSTANCE;
use crate::openvmm::memdiff_vmgs;
use crate::vm::append_cmdline;
use crate::vtl2_settings::ControllerType;
use crate::vtl2_settings::Vtl2LunBuilder;
use crate::vtl2_settings::Vtl2StorageBackingDeviceBuilder;
use crate::vtl2_settings::Vtl2StorageControllerBuilder;
use anyhow::Context;
use disk_backend_resources::FileDiskHandle;
use framebuffer::FRAMEBUFFER_SIZE;
use framebuffer::Framebuffer;
use framebuffer::FramebufferAccess;
use fs_err::File;
use futures::StreamExt;
use get_resources::crash::GuestCrashDeviceHandle;
use get_resources::ged::FirmwareEvent;
use guid::Guid;
use hyperv_ic_resources::shutdown::ShutdownIcHandle;
use ide_resources::GuestMedia;
use ide_resources::IdeDeviceConfig;
use mesh_process::Mesh;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use openvmm_defs::config::Config;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_AARCH64;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_AARCH64_WITH_VTL2;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_X86;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_X86_WITH_VTL2;
use openvmm_defs::config::DEFAULT_PCAT_BOOT_ORDER;
use openvmm_defs::config::DEFAULT_PCIE_ECAM_BASE;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::HypervisorConfig;
use openvmm_defs::config::LateMapVtl0MemoryPolicy;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::ProcessorTopologyConfig;
use openvmm_defs::config::SerialInformation;
use openvmm_defs::config::VmbusConfig;
use openvmm_defs::config::VpciDeviceConfig;
use openvmm_defs::config::Vtl2BaseAddressType;
use openvmm_defs::config::Vtl2Config;
use openvmm_helpers::disk::open_disk_type;
use openvmm_pcat_locator::RomFileLocation;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_core::ResolvedArtifact;
use pipette_client::PIPETTE_VSOCK_PORT;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use serial_16550_resources::ComPort;
use serial_core::resources::DisconnectedSerialBackendHandle;
use serial_socket::net::OpenSocketSerialConfig;
use sparse_mmap::alloc_shared_memory;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use tempfile::TempPath;
use tpm_resources::TpmDeviceHandle;
use tpm_resources::TpmRegisterLayout;
use uidevices_resources::SynthVideoHandle;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use video_core::SharedFramebufferHandle;
use vm_manifest_builder::VmChipsetResult;
use vm_manifest_builder::VmManifestBuilder;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_serial_resources::VmbusSerialDeviceHandle;
use vmbus_serial_resources::VmbusSerialPort;
use vmcore::non_volatile_store::resources::EphemeralNonVolatileStoreHandle;
use vmgs_resources::GuestStateEncryptionPolicy;
use vmgs_resources::VmgsFileHandle;
use vmotherboard::ChipsetDeviceHandle;
use vtl2_settings_proto::Vtl2Settings;

impl PetriVmConfigOpenVmm {
    /// Create a new VM configuration.
    pub async fn new(
        openvmm_path: &ResolvedArtifact,
        petri_vm_config: PetriVmConfig,
        resources: &PetriVmResources,
    ) -> anyhow::Result<Self> {
        let PetriVmConfig {
            name: _,
            arch,
            host_log_levels,
            firmware,
            memory,
            proc_topology,
            agent_image,
            openhcl_agent_image,
            vmgs,
            boot_device_type,
            tpm: tpm_config,
            guest_crash_disk,
        } = petri_vm_config;

        let PetriVmResources { driver, log_source } = resources;

        let mesh = Mesh::new("petri_mesh".to_string())?;

        let setup = PetriVmConfigSetupCore {
            arch,
            firmware: &firmware,
            driver,
            logger: log_source,
            vmgs: &vmgs,
            boot_device_type,
            tpm_config: tpm_config.as_ref(),
            mesh: &mesh,
            openvmm_path,
        };

        let mut chipset = VmManifestBuilder::new(
            match firmware {
                Firmware::LinuxDirect { .. } => {
                    vm_manifest_builder::BaseChipsetType::HyperVGen2LinuxDirect
                }
                Firmware::OpenhclLinuxDirect { .. } => {
                    vm_manifest_builder::BaseChipsetType::HclHost
                }
                Firmware::OpenhclUefi { .. } => vm_manifest_builder::BaseChipsetType::HclHost,
                Firmware::Pcat { .. } => vm_manifest_builder::BaseChipsetType::HypervGen1,
                Firmware::Uefi { .. } => vm_manifest_builder::BaseChipsetType::HypervGen2Uefi,
                Firmware::OpenhclPcat { .. } => todo!("OpenVMM OpenHCL PCAT"),
            },
            match arch {
                MachineArch::X86_64 => vm_manifest_builder::MachineArch::X86_64,
                MachineArch::Aarch64 => vm_manifest_builder::MachineArch::Aarch64,
            },
        );

        let load_mode = setup.load_firmware()?;

        let SerialData {
            mut emulated_serial_config,
            serial_tasks: log_stream_tasks,
            linux_direct_serial_agent,
        } = setup.configure_serial(log_source)?;

        let (video_dev, framebuffer, framebuffer_view) = match setup.config_video()? {
            Some((v, fb, fba)) => {
                chipset = chipset.with_framebuffer();
                (Some(v), Some(fb), Some(fba.view()?))
            }
            None => (None, None, None),
        };

        let mut ide_disks = Vec::new();
        let mut vpci_devices = Vec::new();
        let mut vmbus_devices = Vec::new();

        let (firmware_event_send, firmware_event_recv) = mesh::mpsc_channel();

        let make_vsock_listener = || -> anyhow::Result<(UnixListener, TempPath)> {
            Ok(tempfile::Builder::new()
                .make(|path| UnixListener::bind(path))?
                .into_parts())
        };

        let (with_vtl2, vtl2_vmbus, ged, ged_send, mut vtl2_settings, vtl2_vsock_path) =
            if firmware.is_openhcl() {
                let (ged, ged_send) = setup.config_openhcl_vmbus_devices(
                    &mut emulated_serial_config,
                    &mut vmbus_devices,
                    &firmware_event_send,
                    framebuffer.is_some(),
                )?;

                let late_map_vtl0_memory = match load_mode {
                    LoadMode::Igvm {
                        vtl2_base_address: Vtl2BaseAddressType::Vtl2Allocate { .. },
                        ..
                    } => {
                        // Late Map VTL0 memory not supported when test supplies Vtl2Allocate
                        None
                    }
                    _ => Some(LateMapVtl0MemoryPolicy::InjectException),
                };

                let (vtl2_vsock_listener, vtl2_vsock_path) = make_vsock_listener()?;
                (
                    Some(Vtl2Config {
                        vtl0_alias_map: false, // TODO: enable when OpenVMM supports it for DMA
                        late_map_vtl0_memory,
                    }),
                    Some(VmbusConfig {
                        vsock_listener: Some(vtl2_vsock_listener),
                        vsock_path: Some(vtl2_vsock_path.to_string_lossy().into_owned()),
                        vmbus_max_version: None,
                        vtl2_redirect: false,
                        #[cfg(windows)]
                        vmbusproxy_handle: None,
                    }),
                    Some(ged),
                    Some(ged_send),
                    Some(crate::vm::default_vtl2_settings()),
                    Some(vtl2_vsock_path),
                )
            } else {
                (None, None, None, None, None, None)
            };

        let mut petri_vtl0_scsi = ScsiControllerHandle {
            instance_id: SCSI_INSTANCE,
            max_sub_channel_count: 1,
            io_queue_depth: None,
            devices: vec![],
            requests: None,
            poll_mode_queue_depth: None,
        };

        let boot_disk = setup.load_boot_disk(vtl2_settings.as_mut())?;
        match boot_disk {
            Some(BootDisk::Ide(c)) => {
                ide_disks.push(c);
            }
            Some(BootDisk::Vpci(c)) => {
                vpci_devices.push(c);
            }
            Some(BootDisk::Scsi(d)) => {
                petri_vtl0_scsi.devices.push(d);
            }
            None => {}
        }

        if let Some(guest_crash_disk) = guest_crash_disk.as_ref() {
            petri_vtl0_scsi.devices.push(ScsiDeviceAndPath {
                path: ScsiPath {
                    path: 0,
                    target: 0,
                    lun: crate::vm::PETRI_VTL0_SCSI_CRASH_LUN,
                },
                device: SimpleScsiDiskHandle {
                    read_only: false,
                    parameters: Default::default(),
                    disk: FileDiskHandle(File::open(guest_crash_disk.as_ref())?.into())
                        .into_resource(),
                }
                .into_resource(),
            });
        }

        // Configure the serial ports now that they have been updated by the
        // OpenHCL configuration.
        chipset = chipset.with_serial(emulated_serial_config);
        // Set so that we don't pull serial data until the guest is
        // ready. Otherwise, Linux will drop the input serial data
        // on the floor during boot.
        if matches!(firmware, Firmware::LinuxDirect { .. }) {
            chipset = chipset.with_serial_wait_for_rts();
        }

        // Extract video configuration
        let vga_firmware = match video_dev {
            Some(VideoDevice::Vga(firmware)) => Some(firmware),
            Some(VideoDevice::Synth(vtl, resource)) => {
                vmbus_devices.push((vtl, resource));
                None
            }
            None => None,
        };

        // Add the Hyper-V Shutdown IC
        let (shutdown_ic_send, shutdown_ic_recv) = mesh::channel();
        vmbus_devices.push((
            DeviceVtl::Vtl0,
            ShutdownIcHandle {
                recv: shutdown_ic_recv,
            }
            .into_resource(),
        ));

        // Add the Hyper-V KVP IC
        let (kvp_ic_send, kvp_ic_recv) = mesh::channel();
        vmbus_devices.push((
            DeviceVtl::Vtl0,
            hyperv_ic_resources::kvp::KvpIcHandle { recv: kvp_ic_recv }.into_resource(),
        ));

        // Add the Hyper-V timesync IC
        vmbus_devices.push((
            DeviceVtl::Vtl0,
            hyperv_ic_resources::timesync::TimesyncIcHandle.into_resource(),
        ));

        // Make a vmbus vsock path for pipette connections
        let (vmbus_vsock_listener, vmbus_vsock_path) = make_vsock_listener()?;

        let chipset = chipset
            .build()
            .context("failed to build chipset configuration")?;

        let memory = {
            let MemoryConfig {
                startup_bytes,
                dynamic_memory_range,
            } = memory;

            if dynamic_memory_range.is_some() {
                anyhow::bail!("dynamic memory not supported in OpenVMM");
            }

            openvmm_defs::config::MemoryConfig {
                mem_size: startup_bytes,
                mmio_gaps: if firmware.is_openhcl() {
                    match arch {
                        MachineArch::X86_64 => DEFAULT_MMIO_GAPS_X86_WITH_VTL2.into(),
                        MachineArch::Aarch64 => DEFAULT_MMIO_GAPS_AARCH64_WITH_VTL2.into(),
                    }
                } else {
                    match arch {
                        MachineArch::X86_64 => DEFAULT_MMIO_GAPS_X86.into(),
                        MachineArch::Aarch64 => DEFAULT_MMIO_GAPS_AARCH64.into(),
                    }
                },
                prefetch_memory: false,
                pcie_ecam_base: DEFAULT_PCIE_ECAM_BASE,
            }
        };

        let processor_topology = {
            let ProcessorTopology {
                vp_count,
                enable_smt,
                vps_per_socket,
                apic_mode,
            } = proc_topology;

            ProcessorTopologyConfig {
                proc_count: vp_count,
                vps_per_socket,
                enable_smt,
                arch: Some(match arch {
                    MachineArch::X86_64 => openvmm_defs::config::ArchTopologyConfig::X86(
                        openvmm_defs::config::X86TopologyConfig {
                            x2apic: match apic_mode {
                                None => openvmm_defs::config::X2ApicConfig::Auto,
                                Some(x) => match x {
                                    crate::ApicMode::Xapic => {
                                        openvmm_defs::config::X2ApicConfig::Unsupported
                                    }
                                    crate::ApicMode::X2apicSupported => {
                                        openvmm_defs::config::X2ApicConfig::Supported
                                    }
                                    crate::ApicMode::X2apicEnabled => {
                                        openvmm_defs::config::X2ApicConfig::Enabled
                                    }
                                },
                            },
                            ..Default::default()
                        },
                    ),
                    MachineArch::Aarch64 => openvmm_defs::config::ArchTopologyConfig::Aarch64(
                        openvmm_defs::config::Aarch64TopologyConfig::default(),
                    ),
                }),
            }
        };

        let (secure_boot_enabled, custom_uefi_vars) = firmware.uefi_config().map_or_else(
            || (false, Default::default()),
            |c| {
                (
                    c.secure_boot_enabled,
                    match (arch, c.secure_boot_template) {
                        (MachineArch::X86_64, Some(SecureBootTemplate::MicrosoftWindows)) => {
                            hyperv_secure_boot_templates::x64::microsoft_windows()
                        }
                        (
                            MachineArch::X86_64,
                            Some(SecureBootTemplate::MicrosoftUefiCertificateAuthority),
                        ) => hyperv_secure_boot_templates::x64::microsoft_uefi_ca(),
                        (MachineArch::Aarch64, Some(SecureBootTemplate::MicrosoftWindows)) => {
                            hyperv_secure_boot_templates::aarch64::microsoft_windows()
                        }
                        (
                            MachineArch::Aarch64,
                            Some(SecureBootTemplate::MicrosoftUefiCertificateAuthority),
                        ) => hyperv_secure_boot_templates::aarch64::microsoft_uefi_ca(),
                        (_, None) => Default::default(),
                    },
                )
            },
        );

        let vmgs = if firmware.is_openhcl() {
            None
        } else {
            Some(memdiff_vmgs(&vmgs)?)
        };

        let VmChipsetResult {
            chipset,
            mut chipset_devices,
        } = chipset;

        // Add the TPM
        if let Some(tpm) = setup.config_tpm().await? {
            chipset_devices.push(tpm);
        }

        let config = Config {
            // Firmware
            load_mode,
            firmware_event_send: Some(firmware_event_send),

            // CPU and RAM
            memory,
            processor_topology,

            // Base chipset
            chipset,
            chipset_devices,

            // Basic virtualization device support
            hypervisor: HypervisorConfig {
                with_hv: true,
                user_mode_hv_enlightenments: false,
                user_mode_apic: false,
                with_vtl2,
                with_isolation: match firmware.isolation() {
                    Some(IsolationType::Vbs) => Some(openvmm_defs::config::IsolationType::Vbs),
                    None => None,
                    _ => anyhow::bail!("unsupported isolation type"),
                },
            },
            vmbus: Some(VmbusConfig {
                vsock_listener: Some(vmbus_vsock_listener),
                vsock_path: Some(vmbus_vsock_path.to_string_lossy().into_owned()),
                vmbus_max_version: None,
                vtl2_redirect: firmware.openhcl_config().is_some_and(|c| c.vmbus_redirect),
                #[cfg(windows)]
                vmbusproxy_handle: None,
            }),
            vtl2_vmbus,

            // Devices
            floppy_disks: vec![],
            ide_disks,
            pcie_root_complexes: vec![],
            pcie_devices: vec![],
            pcie_switches: vec![],
            vpci_devices,
            vmbus_devices,

            // Video support
            framebuffer,
            vga_firmware,

            secure_boot_enabled,
            custom_uefi_vars,
            vmgs,

            // Don't automatically reset the guest by default
            automatic_guest_reset: false,

            // Disabled for VMM tests by default
            #[cfg(windows)]
            kernel_vmnics: vec![],
            input: mesh::Receiver::new(),
            vtl2_gfx: false,
            virtio_console_pci: false,
            virtio_serial: None,
            virtio_devices: vec![],
            #[cfg(windows)]
            vpci_resources: vec![],
            debugger_rpc: None,
            generation_id_recv: None,
            rtc_delta_milliseconds: 0,
            efi_diagnostics_log_level: Default::default(), // TODO: Add config for tests
        };

        // Make the pipette connection listener.
        let path = config.vmbus.as_ref().unwrap().vsock_path.as_ref().unwrap();
        let path = format!("{path}_{PIPETTE_VSOCK_PORT}");
        let pipette_listener = PolledSocket::new(
            driver,
            UnixListener::bind(path).context("failed to bind to pipette listener")?,
        )?;

        // Make the vtl2 pipette connection listener.
        let vtl2_pipette_listener = if let Some(vtl2_vmbus) = &config.vtl2_vmbus {
            let path = vtl2_vmbus.vsock_path.as_ref().unwrap();
            let path = format!("{path}_{PIPETTE_VSOCK_PORT}");
            Some(PolledSocket::new(
                driver,
                UnixListener::bind(path).context("failed to bind to vtl2 pipette listener")?,
            )?)
        } else {
            None
        };

        Ok(Self {
            firmware,
            arch,
            host_log_levels,
            config,
            boot_device_type,
            mesh,

            resources: PetriVmResourcesOpenVmm {
                log_stream_tasks,
                firmware_event_recv,
                shutdown_ic_send,
                kvp_ic_send,
                ged_send,
                pipette_listener,
                vtl2_pipette_listener,
                linux_direct_serial_agent,
                driver: driver.clone(),
                output_dir: log_source.output_dir().to_owned(),
                agent_image,
                openhcl_agent_image,
                openvmm_path: openvmm_path.clone(),
                vtl2_vsock_path,
                _vmbus_vsock_path: vmbus_vsock_path,
            },

            openvmm_log_file: log_source.log_file("openvmm")?,

            petri_vtl0_scsi,
            ged,
            framebuffer_view,

            vtl2_settings,
        })
    }
}

struct PetriVmConfigSetupCore<'a> {
    arch: MachineArch,
    firmware: &'a Firmware,
    driver: &'a DefaultDriver,
    logger: &'a PetriLogSource,
    vmgs: &'a PetriVmgsResource,
    boot_device_type: BootDeviceType,
    tpm_config: Option<&'a TpmConfig>,
    mesh: &'a Mesh,
    openvmm_path: &'a ResolvedArtifact,
}

struct SerialData {
    emulated_serial_config: [Option<Resource<SerialBackendHandle>>; 4],
    serial_tasks: Vec<Task<anyhow::Result<()>>>,
    linux_direct_serial_agent: Option<LinuxDirectSerialAgent>,
}

enum BootDisk {
    Scsi(ScsiDeviceAndPath),
    Vpci(VpciDeviceConfig),
    Ide(IdeDeviceConfig),
}

enum VideoDevice {
    Vga(RomFileLocation),
    Synth(DeviceVtl, Resource<VmbusDeviceHandleKind>),
}

impl PetriVmConfigSetupCore<'_> {
    fn configure_serial(&self, logger: &PetriLogSource) -> anyhow::Result<SerialData> {
        let mut serial_tasks = Vec::new();

        let serial0_log_file = logger.log_file(match self.firmware {
            Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => "linux",
            Firmware::Pcat { .. } | Firmware::OpenhclPcat { .. } => "pcat",
            Firmware::Uefi { .. } | Firmware::OpenhclUefi { .. } => "uefi",
        })?;

        let (serial0_host, serial0) = self
            .create_serial_stream()
            .context("failed to create serial0 stream")?;
        let (serial0_read, serial0_write) = serial0_host.split();
        let serial0_task = self.driver.spawn(
            "serial0-console",
            crate::log_task(serial0_log_file, serial0_read, "serial0-console"),
        );
        serial_tasks.push(serial0_task);

        let serial2 = if self.firmware.is_openhcl() {
            let (serial2_host, serial2) = self
                .create_serial_stream()
                .context("failed to create serial2 stream")?;
            let serial2_task = self.driver.spawn(
                "serial2-openhcl",
                crate::log_task(logger.log_file("openhcl")?, serial2_host, "serial2-openhcl"),
            );
            serial_tasks.push(serial2_task);
            serial2
        } else {
            None
        };

        if self.firmware.is_linux_direct() {
            let (serial1_host, serial1) = self.create_serial_stream()?;
            let (serial1_read, _serial1_write) = serial1_host.split();
            let linux_direct_serial_agent =
                LinuxDirectSerialAgent::new(serial1_read, serial0_write);
            Ok(SerialData {
                emulated_serial_config: [serial0, serial1, serial2, None],
                serial_tasks,
                linux_direct_serial_agent: Some(linux_direct_serial_agent),
            })
        } else {
            Ok(SerialData {
                emulated_serial_config: [serial0, None, serial2, None],
                serial_tasks,
                linux_direct_serial_agent: None,
            })
        }
    }

    fn create_serial_stream(
        &self,
    ) -> anyhow::Result<(
        PolledSocket<UnixStream>,
        Option<Resource<SerialBackendHandle>>,
    )> {
        let (host_side, guest_side) = UnixStream::pair()?;
        let host_side = PolledSocket::new(self.driver, host_side)?;
        let serial = OpenSocketSerialConfig::from(guest_side).into_resource();
        Ok((host_side, Some(serial)))
    }

    fn load_firmware(&self) -> anyhow::Result<LoadMode> {
        Ok(match (self.arch, &self.firmware) {
            (MachineArch::X86_64, Firmware::LinuxDirect { kernel, initrd }) => {
                let kernel = File::open(kernel.clone())
                    .context("Failed to open kernel")?
                    .into();
                let initrd = File::open(initrd.clone())
                    .context("Failed to open initrd")?
                    .into();
                LoadMode::Linux {
                    kernel,
                    initrd: Some(initrd),
                    cmdline: "console=ttyS0 debug panic=-1 rdinit=/bin/sh".into(),
                    custom_dsdt: None,
                    enable_serial: true,
                }
            }
            (MachineArch::Aarch64, Firmware::LinuxDirect { kernel, initrd }) => {
                let kernel = File::open(kernel.clone())
                    .context("Failed to open kernel")?
                    .into();
                let initrd = File::open(initrd.clone())
                    .context("Failed to open initrd")?
                    .into();
                LoadMode::Linux {
                    kernel,
                    initrd: Some(initrd),
                    cmdline: "console=ttyAMA0 earlycon debug panic=-1 rdinit=/bin/sh".into(),
                    custom_dsdt: None,
                    enable_serial: true,
                }
            }
            (
                MachineArch::X86_64,
                Firmware::Pcat {
                    bios_firmware: firmware,
                    guest: _,         // load_boot_disk
                    svga_firmware: _, // config_video
                },
            ) => {
                let firmware = openvmm_pcat_locator::find_pcat_bios(firmware.get())
                    .context("Failed to load packaged PCAT binary")?;
                LoadMode::Pcat {
                    firmware,
                    boot_order: DEFAULT_PCAT_BOOT_ORDER,
                }
            }
            (
                _,
                Firmware::Uefi {
                    uefi_firmware: firmware,
                    guest: _, // load_boot_disk
                    uefi_config:
                        UefiConfig {
                            secure_boot_enabled: _,  // new
                            secure_boot_template: _, // new
                            disable_frontpage,
                            default_boot_always_attempt,
                        },
                },
            ) => {
                let firmware = File::open(firmware.clone())
                    .context("Failed to open uefi firmware file")?
                    .into();
                LoadMode::Uefi {
                    firmware,
                    enable_debugging: false,
                    enable_memory_protections: false,
                    disable_frontpage: *disable_frontpage,
                    enable_tpm: self.tpm_config.is_some(),
                    enable_battery: false,
                    enable_serial: true,
                    enable_vpci_boot: matches!(self.boot_device_type, BootDeviceType::Nvme),
                    uefi_console_mode: Some(openvmm_defs::config::UefiConsoleMode::Com1),
                    default_boot_always_attempt: *default_boot_always_attempt,
                    bios_guid: Guid::new_random(),
                }
            }
            (
                MachineArch::X86_64,
                Firmware::OpenhclLinuxDirect {
                    igvm_path,
                    openhcl_config,
                }
                | Firmware::OpenhclUefi {
                    igvm_path,
                    guest: _,       // load_boot_disk
                    isolation: _,   // new via Firmware::isolation
                    uefi_config: _, // config_openhcl_vmbus_devices
                    openhcl_config,
                },
            ) => {
                let OpenHclConfig {
                    vtl2_nvme_boot: _, // load_boot_disk
                    vmbus_redirect: _, // config_openhcl_vmbus_devices
                    custom_command_line: _,
                    log_levels: _,
                    vtl2_base_address_type,
                    modify_vtl2_settings: _, // run_core
                } = openhcl_config;

                let mut cmdline = Some(openhcl_config.command_line());

                append_cmdline(&mut cmdline, "panic=-1 reboot=triple");

                let isolated = match self.firmware {
                    Firmware::OpenhclLinuxDirect { .. } => {
                        // Set UNDERHILL_SERIAL_WAIT_FOR_RTS=1 so that we don't pull serial data
                        // until the guest is ready. Otherwise, Linux will drop the input serial
                        // data on the floor during boot.
                        append_cmdline(
                            &mut cmdline,
                            "UNDERHILL_SERIAL_WAIT_FOR_RTS=1 UNDERHILL_CMDLINE_APPEND=\"rdinit=/bin/sh\"",
                        );
                        false
                    }
                    Firmware::OpenhclUefi { isolation, .. } if isolation.is_some() => true,
                    _ => false,
                };

                // For certain configurations, we need to override the override
                // in new_underhill_vm.
                //
                // TODO: remove this (and OpenHCL override) once host changes
                // are saturated.
                if let Firmware::OpenhclUefi {
                    uefi_config:
                        UefiConfig {
                            default_boot_always_attempt,
                            secure_boot_enabled,
                            ..
                        },
                    ..
                } = self.firmware
                {
                    if !isolated
                        && !secure_boot_enabled
                        && self.tpm_config.is_none()
                        && !default_boot_always_attempt
                    {
                        append_cmdline(&mut cmdline, "HCL_DEFAULT_BOOT_ALWAYS_ATTEMPT=0");
                    }
                }

                let vtl2_base_address = vtl2_base_address_type.unwrap_or_else(|| {
                    if isolated {
                        // Isolated VMs must load at the location specified by
                        // the file, as they do not support relocation.
                        Vtl2BaseAddressType::File
                    } else {
                        // By default, utilize IGVM relocation and tell OpenVMM
                        // to place VTL2 at 2GB. This tests both relocation
                        // support in OpenVMM, and relocation support within
                        // OpenHCL.
                        Vtl2BaseAddressType::Absolute(2 * SIZE_1_GB)
                    }
                });

                let file = File::open(igvm_path.clone())
                    .context("failed to open openhcl firmware file")?
                    .into();
                LoadMode::Igvm {
                    file,
                    cmdline: cmdline.unwrap_or_default(),
                    vtl2_base_address,
                    com_serial: Some(SerialInformation {
                        io_port: ComPort::Com3.io_port(),
                        irq: ComPort::Com3.irq().into(),
                    }),
                }
            }
            (a, f) => anyhow::bail!("Unsupported firmware {f:?} for arch {a:?}"),
        })
    }

    fn load_boot_disk(
        &self,
        vtl2_settings: Option<&mut Vtl2Settings>,
    ) -> anyhow::Result<Option<BootDisk>> {
        let emulate_storage_in_openhcl = matches!(
            self.firmware,
            Firmware::OpenhclUefi {
                openhcl_config: OpenHclConfig {
                    vtl2_nvme_boot: true,
                    ..
                },
                ..
            }
        );
        enum Media {
            Disk(Resource<DiskHandleKind>),
            Dvd(Resource<DiskHandleKind>),
        }
        let media = match &self.firmware {
            Firmware::LinuxDirect { .. }
            | Firmware::OpenhclLinuxDirect { .. }
            | Firmware::Uefi {
                guest: UefiGuest::None,
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::None,
                ..
            } => return Ok(None),
            Firmware::Pcat { guest, .. } | Firmware::OpenhclPcat { guest, .. } => {
                let disk_path = guest.artifact();
                match guest {
                    PcatGuest::Vhd(_) => Media::Disk(memdiff_disk(disk_path.as_ref())?),
                    PcatGuest::Iso(_) => Media::Dvd(open_disk_type(disk_path.as_ref(), true)?),
                }
            }
            Firmware::Uefi { guest, .. } | Firmware::OpenhclUefi { guest, .. } => {
                let disk_path = guest.artifact();
                Media::Disk(memdiff_disk(
                    disk_path.expect("not uefi guest none").as_ref(),
                )?)
            }
        };

        if emulate_storage_in_openhcl {
            match self.boot_device_type {
                BootDeviceType::None => {}
                BootDeviceType::Ide => todo!("support IDE emulation testing"),
                BootDeviceType::Nvme => todo!("support NVMe emulation testing"),
                BootDeviceType::Scsi => vtl2_settings
                    .expect("openhcl config should have vtl2settings")
                    .dynamic
                    .as_mut()
                    .unwrap()
                    .storage_controllers
                    .push(
                        Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                            .with_instance_id(PARAVISOR_BOOT_NVME_INSTANCE)
                            .add_lun(
                                Vtl2LunBuilder::disk()
                                    .with_location(BOOT_NVME_LUN)
                                    .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                                        ControllerType::Nvme,
                                        PARAVISOR_BOOT_NVME_INSTANCE,
                                        BOOT_NVME_NSID,
                                    )),
                            )
                            .build(),
                    ),
            }
            match media {
                Media::Dvd(_) => todo!("support DVD to VTL2"),
                Media::Disk(disk) => Ok(Some(BootDisk::Vpci(VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: PARAVISOR_BOOT_NVME_INSTANCE,
                    resource: NvmeControllerHandle {
                        subsystem_id: PARAVISOR_BOOT_NVME_INSTANCE,
                        max_io_queues: 64,
                        msix_count: 64,
                        namespaces: vec![NamespaceDefinition {
                            nsid: BOOT_NVME_NSID,
                            disk,
                            read_only: false,
                        }],
                    }
                    .into_resource(),
                }))),
            }
        } else {
            match self.boot_device_type {
                BootDeviceType::None => Ok(None),
                BootDeviceType::Ide => {
                    let guest_media = match media {
                        Media::Disk(disk_type) => GuestMedia::Disk {
                            read_only: false,
                            disk_parameters: None,
                            disk_type,
                        },
                        Media::Dvd(media) => GuestMedia::Dvd(
                            SimpleScsiDvdHandle {
                                media: Some(media),
                                requests: None,
                            }
                            .into_resource(),
                        ),
                    };
                    Ok(Some(BootDisk::Ide(IdeDeviceConfig {
                        path: ide_resources::IdePath {
                            channel: 0,
                            drive: 0,
                        },
                        guest_media,
                    })))
                }
                BootDeviceType::Scsi => {
                    let disk = match media {
                        Media::Disk(disk) => disk,
                        Media::Dvd(_) => todo!("support SCSI DVD boot disks"),
                    };
                    Ok(Some(BootDisk::Scsi(ScsiDeviceAndPath {
                        path: ScsiPath {
                            path: 0,
                            target: 0,
                            lun: 0,
                        },
                        device: SimpleScsiDiskHandle {
                            read_only: false,
                            parameters: Default::default(),
                            disk,
                        }
                        .into_resource(),
                    })))
                }
                BootDeviceType::Nvme => {
                    let disk = match media {
                        Media::Disk(disk) => disk,
                        Media::Dvd(_) => anyhow::bail!("dvd not supported on nvme"),
                    };
                    Ok(Some(BootDisk::Vpci(VpciDeviceConfig {
                        vtl: DeviceVtl::Vtl0,
                        instance_id: BOOT_NVME_INSTANCE,
                        resource: NvmeControllerHandle {
                            subsystem_id: BOOT_NVME_INSTANCE,
                            max_io_queues: 64,
                            msix_count: 64,
                            namespaces: vec![NamespaceDefinition {
                                nsid: BOOT_NVME_NSID,
                                disk,
                                read_only: false,
                            }],
                        }
                        .into_resource(),
                    })))
                }
            }
        }
    }

    fn config_openhcl_vmbus_devices(
        &self,
        serial: &mut [Option<Resource<SerialBackendHandle>>],
        devices: &mut impl Extend<(DeviceVtl, Resource<VmbusDeviceHandleKind>)>,
        firmware_event_send: &mesh::Sender<FirmwareEvent>,
        framebuffer: bool,
    ) -> anyhow::Result<(
        get_resources::ged::GuestEmulationDeviceHandle,
        mesh::Sender<get_resources::ged::GuestEmulationRequest>,
    )> {
        let serial0 = serial[0].take();
        devices.extend([(
            DeviceVtl::Vtl2,
            VmbusSerialDeviceHandle {
                port: VmbusSerialPort::Com1,
                backend: serial0.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
            }
            .into_resource(),
        )]);
        let serial1 = serial[1].take();
        devices.extend([(
            DeviceVtl::Vtl2,
            VmbusSerialDeviceHandle {
                port: VmbusSerialPort::Com2,
                backend: serial1.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
            }
            .into_resource(),
        )]);

        let crash = spawn_dump_handler(self.driver, self.logger).into_resource();
        devices.extend([(DeviceVtl::Vtl2, crash)]);

        let (guest_request_send, guest_request_recv) = mesh::channel();

        let (
            UefiConfig {
                secure_boot_enabled,
                secure_boot_template,
                disable_frontpage,
                default_boot_always_attempt,
            },
            OpenHclConfig { vmbus_redirect, .. },
        ) = match self.firmware {
            Firmware::OpenhclUefi {
                uefi_config,
                openhcl_config,
                ..
            } => (uefi_config, openhcl_config),
            Firmware::OpenhclLinuxDirect { openhcl_config, .. } => {
                (&UefiConfig::default(), openhcl_config)
            }
            _ => anyhow::bail!("not a supported openhcl firmware config"),
        };

        let test_gsp_by_id = self
            .vmgs
            .disk()
            .is_some_and(|x| matches!(x.encryption_policy, GuestStateEncryptionPolicy::GspById(_)));

        // Save the GED handle to add later after configuration is complete.
        let ged = get_resources::ged::GuestEmulationDeviceHandle {
            firmware: get_resources::ged::GuestFirmwareConfig::Uefi {
                firmware_debug: false,
                disable_frontpage: *disable_frontpage,
                enable_vpci_boot: matches!(self.boot_device_type, BootDeviceType::Nvme),
                console_mode: get_resources::ged::UefiConsoleMode::COM1,
                default_boot_always_attempt: *default_boot_always_attempt,
            },
            com1: true,
            com2: true,
            vmbus_redirection: *vmbus_redirect,
            vtl2_settings: None, // Will be added at startup to allow tests to modify
            vmgs: memdiff_vmgs(self.vmgs)?,
            framebuffer: framebuffer.then(|| SharedFramebufferHandle.into_resource()),
            guest_request_recv,
            enable_tpm: self.tpm_config.is_some(),
            firmware_event_send: Some(firmware_event_send.clone()),
            secure_boot_enabled: *secure_boot_enabled,
            secure_boot_template: match secure_boot_template {
                Some(SecureBootTemplate::MicrosoftWindows) => {
                    get_resources::ged::GuestSecureBootTemplateType::MicrosoftWindows
                }
                Some(SecureBootTemplate::MicrosoftUefiCertificateAuthority) => {
                    get_resources::ged::GuestSecureBootTemplateType::MicrosoftUefiCertificateAuthority
                }
                None => get_resources::ged::GuestSecureBootTemplateType::None,
            },
            enable_battery: false,
            no_persistent_secrets: self.tpm_config.as_ref().is_some_and(|c| c.no_persistent_secrets),
            igvm_attest_test_config: None,
            test_gsp_by_id,
            efi_diagnostics_log_level: Default::default(), // TODO: make configurable
        };

        Ok((ged, guest_request_send))
    }

    fn config_video(
        &self,
    ) -> anyhow::Result<Option<(VideoDevice, Framebuffer, FramebufferAccess)>> {
        if self.firmware.isolation().is_some() {
            return Ok(None);
        }

        let video_dev = match self.firmware {
            Firmware::Pcat { svga_firmware, .. } | Firmware::OpenhclPcat { svga_firmware, .. } => {
                Some(VideoDevice::Vga(
                    openvmm_pcat_locator::find_svga_bios(svga_firmware.get())
                        .context("Failed to load VGA BIOS")?,
                ))
            }
            Firmware::Uefi { .. } | Firmware::OpenhclUefi { .. } => Some(VideoDevice::Synth(
                DeviceVtl::Vtl0,
                SynthVideoHandle {
                    framebuffer: SharedFramebufferHandle.into_resource(),
                }
                .into_resource(),
            )),
            Firmware::OpenhclLinuxDirect { .. } | Firmware::LinuxDirect { .. } => None,
        };

        Ok(if let Some(vdev) = video_dev {
            let vram = alloc_shared_memory(FRAMEBUFFER_SIZE).context("allocating framebuffer")?;
            let (fb, fba) = framebuffer::framebuffer(vram, FRAMEBUFFER_SIZE, 0)
                .context("creating framebuffer")?;
            Some((vdev, fb, fba))
        } else {
            None
        })
    }

    async fn config_tpm(&self) -> anyhow::Result<Option<ChipsetDeviceHandle>> {
        if !self.firmware.is_openhcl()
            && let Some(TpmConfig {
                no_persistent_secrets,
            }) = self.tpm_config
        {
            let register_layout = match self.arch {
                MachineArch::X86_64 => TpmRegisterLayout::IoPort,
                MachineArch::Aarch64 => TpmRegisterLayout::Mmio,
            };

            let (ppi_store, nvram_store) = if self.vmgs.disk().is_none() || *no_persistent_secrets {
                (
                    EphemeralNonVolatileStoreHandle.into_resource(),
                    EphemeralNonVolatileStoreHandle.into_resource(),
                )
            } else {
                (
                    VmgsFileHandle::new(vmgs_format::FileId::TPM_PPI, true).into_resource(),
                    VmgsFileHandle::new(vmgs_format::FileId::TPM_NVRAM, true).into_resource(),
                )
            };

            Ok(Some(ChipsetDeviceHandle {
                name: "tpm".to_string(),
                resource: chipset_device_worker_defs::RemoteChipsetDeviceHandle {
                    device: TpmDeviceHandle {
                        ppi_store,
                        nvram_store,
                        refresh_tpm_seeds: false,
                        ak_cert_type: tpm_resources::TpmAkCertTypeResource::None,
                        register_layout,
                        guest_secret_key: None,
                        logger: None,
                        is_confidential_vm: self.firmware.isolation().is_some(),
                        // TODO: generate an actual BIOS GUID and put it here
                        bios_guid: Guid::ZERO,
                    }
                    .into_resource(),
                    worker_host: self.make_device_worker("tpm").await?,
                }
                .into_resource(),
            }))
        } else {
            Ok(None)
        }
    }

    async fn make_device_worker(&self, name: &str) -> anyhow::Result<mesh_worker::WorkerHost> {
        let (host, runner) = mesh_worker::worker_host();
        self.mesh
            .launch_host(
                mesh_process::ProcessConfig::new(name).process_name(self.openvmm_path),
                openvmm_defs::entrypoint::MeshHostParams { runner },
            )
            .await?;
        Ok(host)
    }
}

fn spawn_dump_handler(driver: &DefaultDriver, logger: &PetriLogSource) -> GuestCrashDeviceHandle {
    let (send, mut recv) = mesh::channel();
    let handle = GuestCrashDeviceHandle {
        request_dump: send,
        max_dump_size: 256 * 1024 * 1024,
    };
    driver
        .spawn("openhcl-dump-handler", {
            let logger = logger.clone();
            let driver = driver.clone();
            async move {
                while let Some(rpc) = recv.next().await {
                    rpc.handle_failable_sync(|done| {
                        let (file, path) = logger.create_attachment("openhcl.core")?.into_parts();
                        driver
                            .spawn("crash-waiter", async move {
                                let filename = path.file_name().unwrap().to_str().unwrap();
                                if done.await.is_ok() {
                                    tracing::warn!(filename, "openhcl crash dump complete");
                                } else {
                                    tracing::error!(
                                        filename,
                                        "openhcl crash dump incomplete, may be corrupted"
                                    );
                                }
                            })
                            .detach();
                        anyhow::Ok(file)
                    })
                }
            }
        })
        .detach();
    handle
}
