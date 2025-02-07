// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Contains [`PetriVmConfigOpenVmm::new`], which builds a [`PetriVmConfigOpenVmm`] with all
//! default settings for a given [`Firmware`] and [`MachineArch`].

use super::PetriVmArtifactsOpenVmm;
use super::PetriVmConfigOpenVmm;
use super::PetriVmResourcesOpenVmm;
use super::BOOT_NVME_INSTANCE;
use super::BOOT_NVME_LUN;
use super::BOOT_NVME_NSID;
use super::SCSI_INSTANCE;
use crate::linux_direct_serial_agent::LinuxDirectSerialAgent;
use crate::openhcl_diag::OpenHclDiagHandler;
use crate::Firmware;
use crate::IsolationType;
use crate::PcatGuest;
use crate::PetriLogFile;
use crate::PetriLogSource;
use crate::PetriTestParams;
use crate::UefiGuest;
use crate::SIZE_1_GB;
use anyhow::Context;
use disk_backend_resources::layer::DiskLayerHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use disk_backend_resources::LayeredDiskHandle;
use framebuffer::Framebuffer;
use framebuffer::FramebufferAccess;
use framebuffer::FRAMEBUFFER_SIZE;
use fs_err::File;
use futures::io::BufReader;
use futures::AsyncBufReadExt;
use futures::AsyncRead;
use futures::AsyncReadExt;
use get_resources::ged::FirmwareEvent;
use guid::Guid;
use hvlite_defs::config::Config;
use hvlite_defs::config::DeviceVtl;
use hvlite_defs::config::HypervisorConfig;
use hvlite_defs::config::LateMapVtl0MemoryPolicy;
use hvlite_defs::config::LoadMode;
use hvlite_defs::config::MemoryConfig;
use hvlite_defs::config::ProcessorTopologyConfig;
use hvlite_defs::config::SerialInformation;
use hvlite_defs::config::VmbusConfig;
use hvlite_defs::config::VpciDeviceConfig;
use hvlite_defs::config::Vtl2BaseAddressType;
use hvlite_defs::config::Vtl2Config;
use hvlite_defs::config::DEFAULT_MMIO_GAPS;
use hvlite_defs::config::DEFAULT_MMIO_GAPS_WITH_VTL2;
use hvlite_defs::config::DEFAULT_PCAT_BOOT_ORDER;
use hvlite_helpers::crash_dump::spawn_dump_handler;
use hvlite_helpers::disk::open_disk_type;
use hvlite_pcat_locator::RomFileLocation;
use hyperv_ic_resources::shutdown::ShutdownIcHandle;
use ide_resources::GuestMedia;
use ide_resources::IdeDeviceConfig;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::DefaultDriver;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_core::ResolvedArtifact;
use pipette_client::PIPETTE_VSOCK_PORT;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use serial_16550_resources::ComPort;
use serial_core::resources::DisconnectedSerialBackendHandle;
use serial_socket::net::OpenSocketSerialConfig;
use sparse_mmap::alloc_shared_memory;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use uidevices_resources::SynthVideoHandle;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use video_core::SharedFramebufferHandle;
use vm_manifest_builder::VmManifestBuilder;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vmbus_serial_resources::VmbusSerialDeviceHandle;
use vmbus_serial_resources::VmbusSerialPort;
use vtl2_settings_proto::Vtl2Settings;

impl PetriVmConfigOpenVmm {
    /// Create a new VM configuration.
    pub fn new(
        params: &PetriTestParams<'_>,
        artifacts: PetriVmArtifactsOpenVmm,
        driver: &DefaultDriver,
    ) -> anyhow::Result<Self> {
        let PetriVmArtifactsOpenVmm {
            firmware,
            arch,
            agent_image,
            openhcl_agent_image,
            openvmm_path,
            openhcl_dump_directory,
        } = artifacts;

        let setup = PetriVmConfigSetupCore {
            arch,
            firmware: &firmware,
            driver,
            openhcl_dump_directory: &openhcl_dump_directory,
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
            },
            match arch {
                MachineArch::X86_64 => vm_manifest_builder::MachineArch::X86_64,
                MachineArch::Aarch64 => vm_manifest_builder::MachineArch::Aarch64,
            },
        );

        let load_mode = setup.load_firmware()?;

        let SerialData {
            mut emulated_serial_config,
            serial_tasks,
            linux_direct_serial_agent,
        } = setup.configure_serial(params.logger)?;

        let (video_dev, framebuffer, framebuffer_access) = match setup.config_video()? {
            Some((v, fb, fba)) => {
                chipset = chipset.with_framebuffer();
                (Some(v), Some(fb), Some(fba))
            }
            None => (None, None, None),
        };

        let mut devices = Vec::new();

        let (firmware_event_send, firmware_event_recv) = mesh::mpsc_channel();

        let mut vsock_temp_paths = Vec::new();
        let mut make_vsock_listener = || -> anyhow::Result<(UnixListener, PathBuf)> {
            let (listener, temppath) = tempfile::Builder::new()
                .make(|path| UnixListener::bind(path))?
                .into_parts();
            let path = temppath.to_path_buf();
            vsock_temp_paths.push(temppath);
            Ok((listener, path))
        };

        let (with_vtl2, vtl2_vmbus, openhcl_diag_handler, ged, ged_send, mut vtl2_settings) =
            if firmware.is_openhcl() {
                let (ged, ged_send) = setup.config_openhcl_vmbus_devices(
                    &mut emulated_serial_config,
                    &mut devices,
                    &firmware_event_send,
                    framebuffer.is_some(),
                )?;
                let (vtl2_vsock_listener, vtl2_vsock_path) = make_vsock_listener()?;
                let ged_send = Arc::new(ged_send);
                (
                    Some(Vtl2Config {
                        vtl0_alias_map: false, // TODO: enable when OpenVMM supports it for DMA
                        late_map_vtl0_memory: Some(LateMapVtl0MemoryPolicy::InjectException),
                    }),
                    Some(VmbusConfig {
                        vsock_listener: Some(vtl2_vsock_listener),
                        vsock_path: Some(vtl2_vsock_path.to_string_lossy().into_owned()),
                        vmbus_max_version: None,
                        vtl2_redirect: false,
                        #[cfg(windows)]
                        vmbusproxy_handle: None,
                    }),
                    Some(OpenHclDiagHandler {
                        client: diag_client::DiagClient::from_hybrid_vsock(
                            driver.clone(),
                            &vtl2_vsock_path,
                        ),
                        vtl2_vsock_path,
                    }),
                    Some(ged),
                    Some(ged_send),
                    // Basic sane default
                    Some(Vtl2Settings {
                        version: vtl2_settings_proto::vtl2_settings_base::Version::V1.into(),
                        dynamic: Some(Default::default()),
                        fixed: Some(Default::default()),
                        namespace_settings: Default::default(),
                    }),
                )
            } else {
                (None, None, None, None, None, None)
            };

        setup.load_boot_disk(&mut devices, vtl2_settings.as_mut())?;
        let expected_boot_event = setup.get_expected_boot_event();

        // Configure the serial ports now that they have been updated by the
        // OpenHCL configuration.
        chipset = chipset.with_serial(emulated_serial_config);
        // Set so that we don't pull serial data until the guest is
        // ready. Otherwise, Linux will drop the input serial data
        // on the floor during boot.
        if matches!(firmware, Firmware::LinuxDirect { .. }) {
            chipset = chipset.with_serial_wait_for_rts();
        }

        // Partition the devices by type.
        let mut vmbus_devices = Vec::new();
        let mut ide_disks = Vec::new();
        let floppy_disks = Vec::new();
        let mut vpci_devices = Vec::new();
        for d in devices {
            match d {
                Device::Vmbus(vtl, resource) => vmbus_devices.push((vtl, resource)),
                Device::Vpci(c) => vpci_devices.push(c),
                Device::Ide(c) => ide_disks.push(c),
            }
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

        // Make a vmbus vsock path for pipette connections
        let (vmbus_vsock_listener, vmbus_vsock_path) = make_vsock_listener()?;

        let chipset = chipset
            .build()
            .context("failed to build chipset configuration")?;

        let config = Config {
            // Firmware
            load_mode,
            firmware_event_send: Some(firmware_event_send),

            // CPU and RAM
            memory: MemoryConfig {
                mem_size: if firmware.is_openhcl() {
                    4 * SIZE_1_GB
                } else {
                    SIZE_1_GB
                },
                mmio_gaps: if firmware.is_openhcl() {
                    DEFAULT_MMIO_GAPS_WITH_VTL2.into()
                } else {
                    DEFAULT_MMIO_GAPS.into()
                },
                prefetch_memory: false,
            },
            processor_topology: ProcessorTopologyConfig {
                proc_count: 2,
                vps_per_socket: None,
                enable_smt: None,
                arch: Default::default(),
            },

            // Base chipset
            chipset: chipset.chipset,
            chipset_devices: chipset.chipset_devices,

            // Basic virtualization device support
            hypervisor: HypervisorConfig {
                with_hv: true,
                user_mode_hv_enlightenments: false,
                user_mode_apic: false,
                with_vtl2,
                with_isolation: match firmware.isolation() {
                    Some(IsolationType::Vbs) => Some(hvlite_defs::config::IsolationType::Vbs),
                    None => None,
                    _ => anyhow::bail!("unsupported isolation type"),
                },
            },
            vmbus: Some(VmbusConfig {
                vsock_listener: Some(vmbus_vsock_listener),
                vsock_path: Some(vmbus_vsock_path.to_string_lossy().into_owned()),
                vmbus_max_version: None,
                vtl2_redirect: false,
                #[cfg(windows)]
                vmbusproxy_handle: None,
            }),
            vtl2_vmbus,

            // Devices
            floppy_disks,
            ide_disks,
            vpci_devices,
            vmbus_devices,

            // Video support
            framebuffer,
            vga_firmware,

            // Reasonable defaults
            custom_uefi_vars: Default::default(),

            // Disabled for VMM tests by default
            #[cfg(windows)]
            kernel_vmnics: vec![],
            input: mesh::MpscReceiver::new(),
            vtl2_gfx: false,
            virtio_console_pci: false,
            virtio_serial: None,
            virtio_devices: vec![],
            #[cfg(windows)]
            vpci_resources: vec![],
            vmgs_disk: None,
            format_vmgs: false,
            secure_boot_enabled: false,
            debugger_rpc: None,
            generation_id_recv: None,
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
            config,

            resources: PetriVmResourcesOpenVmm {
                serial_tasks,
                firmware_event_recv,
                shutdown_ic_send,
                expected_boot_event,
                ged_send,
                pipette_listener,
                vtl2_pipette_listener,
                openhcl_diag_handler,
                linux_direct_serial_agent,
                driver: driver.clone(),
                output_dir: params.output_dir.to_owned(),
                agent_image,
                openhcl_agent_image,
                openvmm_path,
                log_source: params.logger.clone(),
                _vsock_temp_paths: vsock_temp_paths,
            },

            openvmm_log_file: params.logger.log_file("openvmm")?,

            ged,
            vtl2_settings,
            framebuffer_access,
        })
    }
}

struct PetriVmConfigSetupCore<'a> {
    arch: MachineArch,
    firmware: &'a Firmware,
    driver: &'a DefaultDriver,
    openhcl_dump_directory: &'a ResolvedArtifact,
}

struct SerialData {
    emulated_serial_config: [Option<Resource<SerialBackendHandle>>; 4],
    serial_tasks: Vec<Task<anyhow::Result<()>>>,
    linux_direct_serial_agent: Option<LinuxDirectSerialAgent>,
}

enum Device {
    Vmbus(DeviceVtl, Resource<VmbusDeviceHandleKind>),
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
            Firmware::Pcat { .. } => "pcat",
            Firmware::Uefi { .. } | Firmware::OpenhclUefi { .. } => "uefi",
        })?;

        let (serial0_host, serial0) = self
            .create_serial_stream()
            .context("failed to create serial0 stream")?;
        let (serial0_read, serial0_write) = serial0_host.split();
        let serial0_task = self
            .spawn_serial_task("serial0-console", serial0_log_file, serial0_read)
            .context("failed to spawn serial0 task")?;
        serial_tasks.push(serial0_task);

        let serial2 = if self.firmware.is_openhcl() {
            let (serial2_host, serial2) = self
                .create_serial_stream()
                .context("failed to create serial2 stream")?;
            let serial2_task = self
                .spawn_serial_task("serial2-openhcl", logger.log_file("openhcl")?, serial2_host)
                .context("failed to spawn serial2 task")?;
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

    fn spawn_serial_task(
        &self,
        task_name: &str,
        log_file: PetriLogFile,
        reader: impl AsyncRead + Unpin + Send + 'static,
    ) -> anyhow::Result<Task<anyhow::Result<()>>> {
        Ok(self.driver.spawn(task_name, async move {
            let mut buf = Vec::new();
            let mut reader = BufReader::new(reader);
            loop {
                buf.clear();
                let n = (&mut reader).take(256).read_until(b'\n', &mut buf).await?;
                if n == 0 {
                    break;
                }

                let string_buf = String::from_utf8_lossy(&buf);
                let string_buf_trimmed = string_buf.trim_end();
                log_file.write_entry(string_buf_trimmed);
            }
            Ok(())
        }))
    }

    fn load_firmware(&self) -> anyhow::Result<LoadMode> {
        // Forward OPENVMM_LOG and OPENVMM_SHOW_SPANS to OpenHCL if they're set.
        let openhcl_tracing =
            if let Ok(x) = std::env::var("OPENVMM_LOG").or_else(|_| std::env::var("HVLITE_LOG")) {
                format!("OPENVMM_LOG={x}")
            } else {
                "OPENVMM_LOG=debug".to_owned()
            };
        let openhcl_show_spans = if let Ok(x) = std::env::var("OPENVMM_SHOW_SPANS") {
            format!("OPENVMM_SHOW_SPANS={x}")
        } else {
            "OPENVMM_SHOW_SPANS=true".to_owned()
        };

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
                    ..
                },
            ) => {
                let firmware = hvlite_pcat_locator::find_pcat_bios(firmware.get())
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
                    ..
                },
            ) => {
                let firmware = File::open(firmware.clone())
                    .context("Failed to open uefi firmware file")?
                    .into();
                LoadMode::Uefi {
                    firmware,
                    enable_debugging: false,
                    enable_memory_protections: false,
                    disable_frontpage: true,
                    enable_tpm: false,
                    enable_battery: false,
                    enable_serial: true,
                    enable_vpci_boot: false,
                    uefi_console_mode: Some(hvlite_defs::config::UefiConsoleMode::Com1),
                }
            }
            (
                MachineArch::X86_64,
                Firmware::OpenhclLinuxDirect { igvm_path, .. }
                | Firmware::OpenhclUefi { igvm_path, .. },
            ) => {
                let mut cmdline =
                    format!("panic=-1 reboot=triple {openhcl_tracing} {openhcl_show_spans}");

                let isolated = match self.firmware {
                    Firmware::OpenhclLinuxDirect { .. } => {
                        // Set UNDERHILL_SERIAL_WAIT_FOR_RTS=1 so that we don't pull serial data
                        // until the guest is ready. Otherwise, Linux will drop the input serial
                        // data on the floor during boot.
                        write!(cmdline, " UNDERHILL_SERIAL_WAIT_FOR_RTS=1 UNDERHILL_CMDLINE_APPEND=\"rdinit=/bin/sh\"").unwrap();
                        false
                    }
                    Firmware::OpenhclUefi { isolation, .. } if isolation.is_some() => true,
                    _ => false,
                };
                let file = File::open(igvm_path.clone())
                    .context("failed to open openhcl firmware file")?
                    .into();
                LoadMode::Igvm {
                    file,
                    cmdline,
                    vtl2_base_address: if isolated {
                        // Isolated VMs must load at the location specified by
                        // the file, as they do not support relocation.
                        Vtl2BaseAddressType::File
                    } else {
                        // By default, utilize IGVM relocation and tell hvlite
                        // to place VTL2 at 2GB. This tests both relocation
                        // support in hvlite, and relocation support within
                        // underhill.
                        Vtl2BaseAddressType::Absolute(2 * SIZE_1_GB)
                    },
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
        devices: &mut impl Extend<Device>,
        vtl2_settings: Option<&mut Vtl2Settings>,
    ) -> anyhow::Result<()> {
        match &self.firmware {
            Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => {
                // Nothing to do, everything is contained in LoadMode
            }
            Firmware::Uefi {
                guest: UefiGuest::None,
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::None,
                ..
            } => {
                // Nothing to do, no guest
            }
            Firmware::Pcat { guest, .. } => {
                let disk_path = guest.artifact();
                let inner_disk = open_disk_type(disk_path.as_ref(), true)?;
                let guest_media = match guest {
                    PcatGuest::Vhd(_) => GuestMedia::Disk {
                        read_only: false,
                        disk_parameters: None,
                        disk_type: LayeredDiskHandle {
                            layers: vec![
                                RamDiskLayerHandle { len: None }.into_resource().into(),
                                DiskLayerHandle(inner_disk).into_resource().into(),
                            ],
                        }
                        .into_resource(),
                    },
                    PcatGuest::Iso(_) => GuestMedia::Dvd(
                        SimpleScsiDvdHandle {
                            media: Some(inner_disk),
                            requests: None,
                        }
                        .into_resource(),
                    ),
                };
                devices.extend([Device::Ide(IdeDeviceConfig {
                    path: ide_resources::IdePath {
                        channel: 0,
                        drive: 0,
                    },
                    guest_media,
                })]);
            }
            Firmware::Uefi { guest, .. }
            | Firmware::OpenhclUefi {
                guest,
                vtl2_nvme_boot: false,
                ..
            } => {
                let disk_path = guest.artifact();
                devices.extend([Device::Vmbus(
                    DeviceVtl::Vtl0,
                    ScsiControllerHandle {
                        instance_id: SCSI_INSTANCE,
                        max_sub_channel_count: 1,
                        io_queue_depth: None,
                        devices: vec![ScsiDeviceAndPath {
                            path: ScsiPath {
                                path: 0,
                                target: 0,
                                lun: 0,
                            },
                            device: SimpleScsiDiskHandle {
                                read_only: false,
                                parameters: Default::default(),
                                disk: LayeredDiskHandle {
                                    layers: vec![
                                        RamDiskLayerHandle { len: None }.into_resource().into(),
                                        DiskLayerHandle(open_disk_type(disk_path.as_ref(), true)?)
                                            .into_resource()
                                            .into(),
                                    ],
                                }
                                .into_resource(),
                            }
                            .into_resource(),
                        }],
                        requests: None,
                    }
                    .into_resource(),
                )]);
            }
            Firmware::OpenhclUefi {
                guest,
                vtl2_nvme_boot: true,
                ..
            } => {
                let disk_path = guest.artifact();
                devices.extend([Device::Vpci(VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: BOOT_NVME_INSTANCE,
                    resource: NvmeControllerHandle {
                        subsystem_id: BOOT_NVME_INSTANCE,
                        max_io_queues: 64,
                        msix_count: 64,
                        namespaces: vec![NamespaceDefinition {
                            nsid: BOOT_NVME_NSID,
                            disk: LayeredDiskHandle {
                                layers: vec![
                                    RamDiskLayerHandle { len: None }.into_resource().into(),
                                    DiskLayerHandle(open_disk_type(disk_path.as_ref(), true)?)
                                        .into_resource()
                                        .into(),
                                ],
                            }
                            .into_resource(),
                            read_only: false,
                        }],
                    }
                    .into_resource(),
                })]);
                vtl2_settings
                    .expect("openhcl config should have vtl2settings")
                    .dynamic
                    .as_mut()
                    .unwrap()
                    .storage_controllers
                    .push(vtl2_settings_proto::StorageController {
                        instance_id: SCSI_INSTANCE.to_string(),
                        protocol: vtl2_settings_proto::storage_controller::StorageProtocol::Scsi
                            .into(),
                        luns: vec![vtl2_settings_proto::Lun {
                            location: BOOT_NVME_LUN,
                            device_id: Guid::new_random().to_string(),
                            vendor_id: "OpenVMM".to_string(),
                            product_id: "Disk".to_string(),
                            product_revision_level: "1.0".to_string(),
                            serial_number: "0".to_string(),
                            model_number: "1".to_string(),
                            physical_devices: Some(vtl2_settings_proto::PhysicalDevices {
                                r#type: vtl2_settings_proto::physical_devices::BackingType::Single
                                    .into(),
                                device: Some(vtl2_settings_proto::PhysicalDevice {
                                    device_type:
                                        vtl2_settings_proto::physical_device::DeviceType::Nvme
                                            .into(),
                                    device_path: BOOT_NVME_INSTANCE.to_string(),
                                    sub_device_path: BOOT_NVME_NSID,
                                }),
                                devices: Vec::new(),
                            }),
                            ..Default::default()
                        }],
                        io_queue_depth: None,
                    });
            }
        }

        Ok(())
    }

    fn config_openhcl_vmbus_devices(
        &self,
        serial: &mut [Option<Resource<SerialBackendHandle>>],
        devices: &mut impl Extend<Device>,
        firmware_event_send: &mesh::MpscSender<FirmwareEvent>,
        framebuffer: bool,
    ) -> anyhow::Result<(
        get_resources::ged::GuestEmulationDeviceHandle,
        mesh::Sender<get_resources::ged::GuestEmulationRequest>,
    )> {
        let serial0 = serial[0].take();
        devices.extend([Device::Vmbus(
            DeviceVtl::Vtl2,
            VmbusSerialDeviceHandle {
                port: VmbusSerialPort::Com1,
                backend: serial0.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
            }
            .into_resource(),
        )]);
        let serial1 = serial[1].take();
        devices.extend([Device::Vmbus(
            DeviceVtl::Vtl2,
            VmbusSerialDeviceHandle {
                port: VmbusSerialPort::Com2,
                backend: serial1.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
            }
            .into_resource(),
        )]);

        let gel = get_resources::gel::GuestEmulationLogHandle.into_resource();

        let (crash, task) = spawn_dump_handler(
            self.driver,
            self.openhcl_dump_directory.clone().into(),
            None,
        );
        task.detach();

        devices.extend([
            Device::Vmbus(DeviceVtl::Vtl2, crash),
            Device::Vmbus(DeviceVtl::Vtl2, gel),
        ]);

        let (guest_request_send, guest_request_recv) = mesh::channel();

        // Save the GED handle to add later after configuration is complete.
        let ged = get_resources::ged::GuestEmulationDeviceHandle {
            firmware: get_resources::ged::GuestFirmwareConfig::Uefi {
                firmware_debug: false,
                disable_frontpage: true,
                enable_vpci_boot: false,
                console_mode: get_resources::ged::UefiConsoleMode::COM1,
            },
            com1: true,
            com2: true,
            vmbus_redirection: false,
            vtl2_settings: None, // Will be added at startup to allow tests to modify
            vmgs_disk: Some(
                LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                    len: Some(vmgs_format::VMGS_DEFAULT_CAPACITY),
                })
                .into_resource(),
            ),
            framebuffer: framebuffer.then(|| SharedFramebufferHandle.into_resource()),
            guest_request_recv,
            enable_tpm: false,
            firmware_event_send: Some(firmware_event_send.clone()),
            secure_boot_enabled: false,
            secure_boot_template: get_resources::ged::GuestSecureBootTemplateType::None,
            enable_battery: false,
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
            Firmware::Pcat { svga_firmware, .. } => Some(VideoDevice::Vga(
                hvlite_pcat_locator::find_svga_bios(svga_firmware.get())
                    .context("Failed to load VGA BIOS")?,
            )),
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

    fn get_expected_boot_event(&self) -> Option<FirmwareEvent> {
        match &self.firmware {
            Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => None,
            Firmware::Pcat { .. } => {
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
}
