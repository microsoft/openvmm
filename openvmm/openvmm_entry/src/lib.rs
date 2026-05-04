// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the interactive control process and the entry point
//! for the worker process.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod cli_args;
mod crash_dump;
mod kvp;
mod meshworker;
mod repl;
mod serial_io;
mod storage_builder;
mod tracing_init;
mod ttrpc;
mod vm_controller;

// `pub` so that the missing_docs warning fires for options without
// documentation.
pub use cli_args::Options;
use console_relay::ConsoleLaunchOptions;

use crate::cli_args::SecureBootTemplateCli;
use anyhow::Context;
use anyhow::bail;
use chipset_resources::battery::HostBatteryUpdate;
use clap::Parser;
use cli_args::DiskCliKind;
use cli_args::EfiDiagnosticsLogLevelCli;
use cli_args::EndpointConfigCli;
use cli_args::NicConfigCli;
use cli_args::ProvisionVmgs;
use cli_args::SerialConfigCli;
use cli_args::UefiConsoleModeCli;
use cli_args::VirtioBusCli;
use cli_args::VmgsCli;
use crash_dump::spawn_dump_handler;
use disk_backend_resources::DelayDiskHandle;
use disk_backend_resources::DiskLayerDescription;
use disk_backend_resources::layer::DiskLayerHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use disk_backend_resources::layer::SqliteAutoCacheDiskLayerHandle;
use disk_backend_resources::layer::SqliteDiskLayerHandle;
use floppy_resources::FloppyDiskConfig;
use framebuffer::FRAMEBUFFER_SIZE;
use framebuffer::FramebufferAccess;
use futures::AsyncReadExt;
use futures::AsyncWrite;
use futures::StreamExt;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use gdma_resources::GdmaDeviceHandle;
use gdma_resources::VportDefinition;
use guid::Guid;
use input_core::MultiplexedInputHandle;
use inspect::InspectMut;
use io::Read;
use memory_range::MemoryRange;
use mesh::CancelContext;
use mesh::CellUpdater;
use mesh::rpc::RpcSend;
use meshworker::VmmMesh;
use net_backend_resources::mac_address::MacAddress;
use nvme_resources::NvmeControllerRequest;
use openvmm_defs::config::Config;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_AARCH64;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_AARCH64_WITH_VTL2;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_X86;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_X86_WITH_VTL2;
use openvmm_defs::config::DEFAULT_PCAT_BOOT_ORDER;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::EfiDiagnosticsLogLevelType;
use openvmm_defs::config::HypervisorConfig;
use openvmm_defs::config::LateMapVtl0MemoryPolicy;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::MemoryConfig;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::PcieRootPortConfig;
use openvmm_defs::config::PcieSwitchConfig;
use openvmm_defs::config::ProcessorTopologyConfig;
use openvmm_defs::config::SerialInformation;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::config::VmbusConfig;
use openvmm_defs::config::VpciDeviceConfig;
use openvmm_defs::config::Vtl2BaseAddressType;
use openvmm_defs::config::Vtl2Config;
use openvmm_defs::rpc::VmRpc;
use openvmm_defs::worker::VM_WORKER;
use openvmm_defs::worker::VmWorkerParameters;
use openvmm_helpers::disk::OpenDiskOptions;
use openvmm_helpers::disk::create_disk_type;
use openvmm_helpers::disk::open_disk_type;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use serial_16550_resources::ComPort;
use serial_core::resources::DisconnectedSerialBackendHandle;
use sparse_mmap::alloc_shared_memory;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::pending;
use std::io;
#[cfg(unix)]
use std::io::IsTerminal;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use storvsp_resources::ScsiControllerRequest;
use tpm_resources::TpmDeviceHandle;
use tpm_resources::TpmRegisterLayout;
use uidevices_resources::SynthKeyboardHandle;
use uidevices_resources::SynthMouseHandle;
use uidevices_resources::SynthVideoHandle;
use video_core::SharedFramebufferHandle;
use virtio_resources::VirtioPciDeviceHandle;
use vm_manifest_builder::BaseChipsetType;
use vm_manifest_builder::MachineArch;
use vm_manifest_builder::VmChipsetResult;
use vm_manifest_builder::VmManifestBuilder;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::DiskLayerHandleKind;
use vm_resource::kind::NetEndpointHandleKind;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_serial_resources::VmbusSerialDeviceHandle;
use vmbus_serial_resources::VmbusSerialPort;
use vmcore::non_volatile_store::resources::EphemeralNonVolatileStoreHandle;
use vmgs_resources::GuestStateEncryptionPolicy;
use vmgs_resources::VmgsDisk;
use vmgs_resources::VmgsFileHandle;
use vmgs_resources::VmgsResource;
use vmotherboard::ChipsetDeviceHandle;
use vnc_worker_defs::VncParameters;

pub fn openvmm_main() {
    // Save the current state of the terminal so we can restore it back to
    // normal before exiting.
    #[cfg(unix)]
    let orig_termios = io::stderr().is_terminal().then(term::get_termios);

    let mut pidfile_path = None;
    let exit_code = match do_main(&mut pidfile_path) {
        Ok(_) => 0,
        Err(err) => {
            eprintln!("fatal error: {:?}", err);
            1
        }
    };

    // Restore the terminal to its initial state.
    #[cfg(unix)]
    if let Some(orig_termios) = orig_termios {
        term::set_termios(orig_termios);
    }

    // Clean up the pidfile before terminating, since pal::process::terminate
    // skips destructors.
    if let Some(ref path) = pidfile_path {
        let _ = std::fs::remove_file(path);
    }

    // Terminate the process immediately without graceful shutdown of DLLs or
    // C++ destructors or anything like that. This is all unnecessary and saves
    // time on Windows.
    //
    // Do flush stdout, though, since there may be buffered data.
    let _ = io::stdout().flush();
    pal::process::terminate(exit_code);
}

#[derive(Default)]
struct VmResources {
    console_in: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    framebuffer_access: Option<FramebufferAccess>,
    shutdown_ic: Option<mesh::Sender<hyperv_ic_resources::shutdown::ShutdownRpc>>,
    kvp_ic: Option<mesh::Sender<hyperv_ic_resources::kvp::KvpConnectRpc>>,
    scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,
    nvme_vtl2_rpc: Option<mesh::Sender<NvmeControllerRequest>>,
    ged_rpc: Option<mesh::Sender<get_resources::ged::GuestEmulationRequest>>,
    vtl2_settings: Option<vtl2_settings_proto::Vtl2Settings>,
    #[cfg(windows)]
    switch_ports: Vec<vmswitch::kernel::SwitchPort>,
}

struct ConsoleState<'a> {
    device: &'a str,
    input: Box<dyn AsyncWrite + Unpin + Send>,
}

/// Build a flat list of switches with their parent port assignments.
///
/// This function converts hierarchical CLI switch definitions into a flat list
/// where each switch specifies its parent port directly.
fn build_switch_list(all_switches: &[cli_args::GenericPcieSwitchCli]) -> Vec<PcieSwitchConfig> {
    all_switches
        .iter()
        .map(|switch_cli| PcieSwitchConfig {
            name: switch_cli.name.clone(),
            num_downstream_ports: switch_cli.num_downstream_ports,
            parent_port: switch_cli.port_name.clone(),
            hotplug: switch_cli.hotplug,
        })
        .collect()
}

async fn vm_config_from_command_line(
    spawner: impl Spawn,
    mesh: &VmmMesh,
    opt: &Options,
) -> anyhow::Result<(Config, VmResources)> {
    let (_, serial_driver) = DefaultPool::spawn_on_thread("serial");
    // Ensure the serial driver stays alive with no tasks.
    serial_driver.spawn("leak", pending::<()>()).detach();

    let openhcl_vtl = if opt.vtl2 {
        DeviceVtl::Vtl2
    } else {
        DeviceVtl::Vtl0
    };

    let console_state: RefCell<Option<ConsoleState<'_>>> = RefCell::new(None);
    let setup_serial = |name: &str, cli_cfg, device| -> anyhow::Result<_> {
        Ok(match cli_cfg {
            SerialConfigCli::Console => {
                if let Some(console_state) = console_state.borrow().as_ref() {
                    bail!("console already set by {}", console_state.device);
                }
                let (config, serial) = serial_io::anonymous_serial_pair(&serial_driver)?;
                let (serial_read, serial_write) = AsyncReadExt::split(serial);
                *console_state.borrow_mut() = Some(ConsoleState {
                    device,
                    input: Box::new(serial_write),
                });
                thread::Builder::new()
                    .name(name.to_owned())
                    .spawn(move || {
                        let _ = block_on(futures::io::copy(
                            serial_read,
                            &mut AllowStdIo::new(term::raw_stdout()),
                        ));
                    })
                    .unwrap();
                Some(config)
            }
            SerialConfigCli::Stderr => {
                let (config, serial) = serial_io::anonymous_serial_pair(&serial_driver)?;
                thread::Builder::new()
                    .name(name.to_owned())
                    .spawn(move || {
                        let _ = block_on(futures::io::copy(
                            serial,
                            &mut AllowStdIo::new(term::raw_stderr()),
                        ));
                    })
                    .unwrap();
                Some(config)
            }
            SerialConfigCli::File(path) => {
                let (config, serial) = serial_io::anonymous_serial_pair(&serial_driver)?;
                let file = fs_err::File::create(path).context("failed to create file")?;

                thread::Builder::new()
                    .name(name.to_owned())
                    .spawn(move || {
                        let _ = block_on(futures::io::copy(serial, &mut AllowStdIo::new(file)));
                    })
                    .unwrap();
                Some(config)
            }
            SerialConfigCli::None => None,
            SerialConfigCli::Pipe(path) => {
                Some(serial_io::bind_serial(&path).context("failed to bind serial")?)
            }
            SerialConfigCli::Tcp(addr) => {
                Some(serial_io::bind_tcp_serial(&addr).context("failed to bind serial")?)
            }
            SerialConfigCli::NewConsole(app, window_title) => {
                let path = console_relay::random_console_path();
                let config =
                    serial_io::bind_serial(&path).context("failed to bind console serial")?;
                let window_title =
                    window_title.unwrap_or_else(|| name.to_uppercase() + " [OpenVMM]");

                console_relay::launch_console(
                    app.or_else(openvmm_terminal_app).as_deref(),
                    &path,
                    ConsoleLaunchOptions {
                        window_title: Some(window_title),
                    },
                )
                .context("failed to launch console")?;

                Some(config)
            }
        })
    };

    let mut vmbus_devices = Vec::new();

    let serial0_cfg = setup_serial(
        "com1",
        opt.com1.clone().unwrap_or(SerialConfigCli::Console),
        if cfg!(guest_arch = "x86_64") {
            "ttyS0"
        } else {
            "ttyAMA0"
        },
    )?;
    let serial1_cfg = setup_serial(
        "com2",
        opt.com2.clone().unwrap_or(SerialConfigCli::None),
        if cfg!(guest_arch = "x86_64") {
            "ttyS1"
        } else {
            "ttyAMA1"
        },
    )?;
    let serial2_cfg = setup_serial(
        "com3",
        opt.com3.clone().unwrap_or(SerialConfigCli::None),
        if cfg!(guest_arch = "x86_64") {
            "ttyS2"
        } else {
            "ttyAMA2"
        },
    )?;
    let serial3_cfg = setup_serial(
        "com4",
        opt.com4.clone().unwrap_or(SerialConfigCli::None),
        if cfg!(guest_arch = "x86_64") {
            "ttyS3"
        } else {
            "ttyAMA3"
        },
    )?;
    let with_vmbus_com1_serial = if let Some(vmbus_com1_cfg) = setup_serial(
        "vmbus_com1",
        opt.vmbus_com1_serial
            .clone()
            .unwrap_or(SerialConfigCli::None),
        "vmbus_com1",
    )? {
        vmbus_devices.push((
            openhcl_vtl,
            VmbusSerialDeviceHandle {
                port: VmbusSerialPort::Com1,
                backend: vmbus_com1_cfg,
            }
            .into_resource(),
        ));
        true
    } else {
        false
    };
    let with_vmbus_com2_serial = if let Some(vmbus_com2_cfg) = setup_serial(
        "vmbus_com2",
        opt.vmbus_com2_serial
            .clone()
            .unwrap_or(SerialConfigCli::None),
        "vmbus_com2",
    )? {
        vmbus_devices.push((
            openhcl_vtl,
            VmbusSerialDeviceHandle {
                port: VmbusSerialPort::Com2,
                backend: vmbus_com2_cfg,
            }
            .into_resource(),
        ));
        true
    } else {
        false
    };
    let debugcon_cfg = setup_serial(
        "debugcon",
        opt.debugcon
            .clone()
            .map(|cfg| cfg.serial)
            .unwrap_or(SerialConfigCli::None),
        "debugcon",
    )?;

    let virtio_console_backend = if let Some(serial_cfg) = opt.virtio_console.clone() {
        setup_serial("virtio-console", serial_cfg, "hvc0")?
    } else {
        None
    };

    let mut resources = VmResources::default();
    let mut console_str = "";
    if let Some(ConsoleState { device, input }) = console_state.into_inner() {
        resources.console_in = Some(input);
        console_str = device;
    }

    if opt.shared_memory {
        tracing::warn!("--shared-memory/-M flag has no effect and will be removed");
    }
    if opt.deprecated_prefetch {
        tracing::warn!("--prefetch is deprecated; use --memory prefetch=on");
    }
    if opt.deprecated_private_memory {
        tracing::warn!("--private-memory is deprecated; use --memory shared=off");
    }
    if opt.deprecated_thp {
        tracing::warn!("--thp is deprecated; use --memory shared=off,thp=on");
    }
    if opt.deprecated_memory_backing_file.is_some() {
        tracing::warn!("--memory-backing-file is deprecated; use --memory file=<path>");
    }

    opt.validate_memory_options()?;

    const MAX_PROCESSOR_COUNT: u32 = 1024;

    if opt.processors == 0 || opt.processors > MAX_PROCESSOR_COUNT {
        bail!("invalid proc count: {}", opt.processors);
    }

    // Total SCSI channel count should not exceed the processor count
    // (at most, one channel per VP).
    if opt.scsi_sub_channels > (MAX_PROCESSOR_COUNT - 1) as u16 {
        bail!(
            "invalid SCSI sub-channel count: requested {}, max {}",
            opt.scsi_sub_channels,
            MAX_PROCESSOR_COUNT - 1
        );
    }

    let with_get = opt.get || (opt.vtl2 && !opt.no_get);

    let mut storage = storage_builder::StorageBuilder::new(with_get.then_some(openhcl_vtl));
    for &cli_args::DiskCli {
        vtl,
        ref kind,
        read_only,
        is_dvd,
        underhill,
        ref pcie_port,
    } in &opt.disk
    {
        if pcie_port.is_some() {
            anyhow::bail!("`--disk` is incompatible with PCIe");
        }

        storage
            .add(
                vtl,
                underhill,
                storage_builder::DiskLocation::Scsi(None),
                kind,
                is_dvd,
                read_only,
            )
            .await?;
    }

    for &cli_args::IdeDiskCli {
        ref kind,
        read_only,
        channel,
        device,
        is_dvd,
    } in &opt.ide
    {
        storage
            .add(
                DeviceVtl::Vtl0,
                None,
                storage_builder::DiskLocation::Ide(channel, device),
                kind,
                is_dvd,
                read_only,
            )
            .await?;
    }

    for &cli_args::DiskCli {
        vtl,
        ref kind,
        read_only,
        is_dvd,
        underhill,
        ref pcie_port,
    } in &opt.nvme
    {
        storage
            .add(
                vtl,
                underhill,
                storage_builder::DiskLocation::Nvme(None, pcie_port.clone()),
                kind,
                is_dvd,
                read_only,
            )
            .await?;
    }

    for &cli_args::DiskCli {
        vtl,
        ref kind,
        read_only,
        is_dvd,
        ref underhill,
        ref pcie_port,
    } in &opt.virtio_blk
    {
        if underhill.is_some() {
            anyhow::bail!("underhill not supported with virtio-blk");
        }
        storage
            .add(
                vtl,
                None,
                storage_builder::DiskLocation::VirtioBlk(pcie_port.clone()),
                kind,
                is_dvd,
                read_only,
            )
            .await?;
    }

    let mut floppy_disks = Vec::new();
    for disk in &opt.floppy {
        let &cli_args::FloppyDiskCli {
            ref kind,
            read_only,
        } = disk;
        floppy_disks.push(FloppyDiskConfig {
            disk_type: disk_open(kind, read_only).await?,
            read_only,
        });
    }

    let mut vpci_mana_nics = [(); 3].map(|()| None);
    let mut pcie_mana_nics = BTreeMap::<String, GdmaDeviceHandle>::new();
    let mut underhill_nics = Vec::new();
    let mut vpci_devices = Vec::new();

    let mut nic_index = 0;
    for cli_cfg in &opt.net {
        if cli_cfg.pcie_port.is_some() {
            anyhow::bail!("`--net` does not support PCIe");
        }
        let vport = parse_endpoint(cli_cfg, &mut nic_index, &mut resources)?;
        if cli_cfg.underhill {
            if !opt.no_alias_map {
                anyhow::bail!("must specify --no-alias-map to offer NICs to VTL2");
            }
            let mana = vpci_mana_nics[openhcl_vtl as usize].get_or_insert_with(|| {
                let vpci_instance_id = Guid::new_random();
                underhill_nics.push(vtl2_settings_proto::NicDeviceLegacy {
                    instance_id: vpci_instance_id.to_string(),
                    subordinate_instance_id: None,
                    max_sub_channels: None,
                });
                (vpci_instance_id, GdmaDeviceHandle { vports: Vec::new() })
            });
            mana.1.vports.push(VportDefinition {
                mac_address: vport.mac_address,
                endpoint: vport.endpoint,
            });
        } else {
            vmbus_devices.push(vport.into_netvsp_handle());
        }
    }

    if opt.nic {
        let nic_config = parse_endpoint(
            &NicConfigCli {
                vtl: DeviceVtl::Vtl0,
                endpoint: EndpointConfigCli::Consomme {
                    cidr: None,
                    host_fwd: Vec::new(),
                },
                max_queues: None,
                underhill: false,
                pcie_port: None,
            },
            &mut nic_index,
            &mut resources,
        )?;
        vmbus_devices.push(nic_config.into_netvsp_handle());
    }

    // Build initial PCIe devices list from CLI options. Storage devices
    // (e.g., NVMe controllers on PCIe ports) are added later by storage_builder.
    let mut pcie_devices = Vec::new();
    for (index, cli_cfg) in opt.pcie_remote.iter().enumerate() {
        tracing::info!(
            port_name = %cli_cfg.port_name,
            socket_addr = ?cli_cfg.socket_addr,
            "instantiating PCIe remote device"
        );

        // Generate a deterministic instance ID based on index
        const PCIE_REMOTE_BASE_INSTANCE_ID: Guid =
            guid::guid!("28ed784d-c059-429f-9d9a-46bea02562c0");
        let instance_id = Guid {
            data1: index as u32,
            ..PCIE_REMOTE_BASE_INSTANCE_ID
        };

        pcie_devices.push(PcieDeviceConfig {
            port_name: cli_cfg.port_name.clone(),
            resource: pcie_remote_resources::PcieRemoteHandle {
                instance_id,
                socket_addr: cli_cfg.socket_addr.clone(),
                hu: cli_cfg.hu,
                controller: cli_cfg.controller,
            }
            .into_resource(),
        });
    }

    #[cfg(windows)]
    let mut kernel_vmnics = Vec::new();
    #[cfg(windows)]
    for (index, switch_id) in opt.kernel_vmnic.iter().enumerate() {
        // Pick a random MAC address.
        let mut mac_address = [0x00, 0x15, 0x5D, 0, 0, 0];
        getrandom::fill(&mut mac_address[3..]).expect("rng failure");

        // Pick a fixed instance ID based on the index.
        const BASE_INSTANCE_ID: Guid = guid::guid!("00000000-435d-11ee-9f59-00155d5016fc");
        let instance_id = Guid {
            data1: index as u32,
            ..BASE_INSTANCE_ID
        };

        let switch_id = if switch_id == "default" {
            DEFAULT_SWITCH
        } else {
            switch_id
        };
        let (port_id, port) = new_switch_port(switch_id)?;
        resources.switch_ports.push(port);

        kernel_vmnics.push(openvmm_defs::config::KernelVmNicConfig {
            instance_id,
            mac_address: mac_address.into(),
            switch_port_id: port_id,
        });
    }

    for vport in &opt.mana {
        let vport = parse_endpoint(vport, &mut nic_index, &mut resources)?;
        let vport_array = match (vport.vtl as usize, vport.pcie_port) {
            (vtl, None) => {
                &mut vpci_mana_nics[vtl]
                    .get_or_insert_with(|| {
                        (Guid::new_random(), GdmaDeviceHandle { vports: Vec::new() })
                    })
                    .1
                    .vports
            }
            (0, Some(pcie_port)) => {
                &mut pcie_mana_nics
                    .entry(pcie_port)
                    .or_insert(GdmaDeviceHandle { vports: Vec::new() })
                    .vports
            }
            _ => anyhow::bail!("PCIe NICs only supported to VTL0"),
        };
        vport_array.push(VportDefinition {
            mac_address: vport.mac_address,
            endpoint: vport.endpoint,
        });
    }

    vpci_devices.extend(
        vpci_mana_nics
            .into_iter()
            .enumerate()
            .filter_map(|(vtl, nic)| {
                nic.map(|(instance_id, handle)| VpciDeviceConfig {
                    vtl: match vtl {
                        0 => DeviceVtl::Vtl0,
                        1 => DeviceVtl::Vtl1,
                        2 => DeviceVtl::Vtl2,
                        _ => unreachable!(),
                    },
                    instance_id,
                    resource: handle.into_resource(),
                })
            }),
    );

    pcie_devices.extend(
        pcie_mana_nics
            .into_iter()
            .map(|(pcie_port, handle)| PcieDeviceConfig {
                port_name: pcie_port,
                resource: handle.into_resource(),
            }),
    );

    // If VTL2 is enabled, and we are not in VTL2 self allocate mode, provide an
    // mmio gap for VTL2.
    let use_vtl2_gap = opt.vtl2
        && !matches!(
            opt.igvm_vtl2_relocation_type,
            Vtl2BaseAddressType::Vtl2Allocate { .. },
        );

    #[cfg(guest_arch = "aarch64")]
    let arch = MachineArch::Aarch64;
    #[cfg(guest_arch = "x86_64")]
    let arch = MachineArch::X86_64;

    let mmio_gaps: Vec<MemoryRange> = match (use_vtl2_gap, arch) {
        (true, MachineArch::X86_64) => DEFAULT_MMIO_GAPS_X86_WITH_VTL2.into(),
        (true, MachineArch::Aarch64) => DEFAULT_MMIO_GAPS_AARCH64_WITH_VTL2.into(),
        (false, MachineArch::X86_64) => DEFAULT_MMIO_GAPS_X86.into(),
        (false, MachineArch::Aarch64) => DEFAULT_MMIO_GAPS_AARCH64.into(),
    };

    let mut pci_ecam_gaps = Vec::new();
    let mut pci_mmio_gaps = Vec::new();

    let mut low_mmio_start = mmio_gaps.first().context("expected mmio gap")?.start();
    let mut high_mmio_end = mmio_gaps.last().context("expected second mmio gap")?.end();

    let mut pcie_root_complexes = Vec::new();
    for (i, rc_cli) in opt.pcie_root_complex.iter().enumerate() {
        let ports = opt
            .pcie_root_port
            .iter()
            .filter(|port_cli| port_cli.root_complex_name == rc_cli.name)
            .map(|port_cli| PcieRootPortConfig {
                name: port_cli.name.clone(),
                hotplug: port_cli.hotplug,
            })
            .collect();

        const ONE_MB: u64 = 1024 * 1024;
        let low_mmio_size = (rc_cli.low_mmio as u64).next_multiple_of(ONE_MB);
        let high_mmio_size = rc_cli
            .high_mmio
            .checked_next_multiple_of(ONE_MB)
            .context("high mmio rounding error")?;
        let ecam_size = (((rc_cli.end_bus - rc_cli.start_bus) as u64) + 1) * 256 * 4096;

        let low_pci_mmio_start = low_mmio_start
            .checked_sub(low_mmio_size)
            .context("pci low mmio underflow")?;
        let ecam_start = low_pci_mmio_start
            .checked_sub(ecam_size)
            .context("pci ecam underflow")?;
        low_mmio_start = ecam_start;
        high_mmio_end = high_mmio_end
            .checked_add(high_mmio_size)
            .context("pci high mmio overflow")?;

        let ecam_range = MemoryRange::new(ecam_start..ecam_start + ecam_size);
        let low_mmio = MemoryRange::new(low_pci_mmio_start..low_pci_mmio_start + low_mmio_size);
        let high_mmio = MemoryRange::new(high_mmio_end - high_mmio_size..high_mmio_end);

        pci_ecam_gaps.push(ecam_range);
        pci_mmio_gaps.push(low_mmio);
        pci_mmio_gaps.push(high_mmio);

        pcie_root_complexes.push(PcieRootComplexConfig {
            index: i as u32,
            name: rc_cli.name.clone(),
            segment: rc_cli.segment,
            start_bus: rc_cli.start_bus,
            end_bus: rc_cli.end_bus,
            ecam_range,
            low_mmio,
            high_mmio,
            ports,
        });
    }

    pci_ecam_gaps.sort();
    pci_mmio_gaps.sort();

    let pcie_switches = build_switch_list(&opt.pcie_switch);

    #[cfg(target_os = "linux")]
    let vfio_pcie_devices: Vec<PcieDeviceConfig> = opt
        .vfio
        .iter()
        .map(|cli_cfg| {
            use vm_resource::IntoResource;

            let sysfs_path = Path::new("/sys/bus/pci/devices").join(&cli_cfg.pci_id);
            let iommu_group_link = std::fs::read_link(sysfs_path.join("iommu_group"))
                .with_context(|| format!("failed to read IOMMU group for {}", cli_cfg.pci_id))?;
            let group_id: u64 = iommu_group_link
                .file_name()
                .and_then(|s| s.to_str())
                .context("invalid iommu_group symlink")?
                .parse()
                .context("failed to parse IOMMU group ID")?;
            let group = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("/dev/vfio/{group_id}"))
                .with_context(|| format!("failed to open /dev/vfio/{group_id}"))?;

            Ok(PcieDeviceConfig {
                port_name: cli_cfg.port_name.clone(),
                resource: vfio_assigned_device_resources::VfioDeviceHandle {
                    pci_id: cli_cfg.pci_id.clone(),
                    group,
                }
                .into_resource(),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    #[cfg(windows)]
    let vpci_resources: Vec<_> = opt
        .device
        .iter()
        .map(|path| -> anyhow::Result<_> {
            Ok(virt_whp::device::DeviceHandle(
                whp::VpciResource::new(
                    None,
                    Default::default(),
                    &whp::VpciResourceDescriptor::Sriov(path, 0, 0),
                )
                .with_context(|| format!("opening PCI device {}", path))?,
            ))
        })
        .collect::<Result<_, _>>()?;

    // Create a vmbusproxy handle if needed by any devices.
    #[cfg(windows)]
    let vmbusproxy_handle = if !kernel_vmnics.is_empty() {
        Some(vmbus_proxy::ProxyHandle::new().context("failed to open vmbusproxy handle")?)
    } else {
        None
    };

    let framebuffer = if opt.gfx || opt.vtl2_gfx || opt.vnc || opt.pcat {
        let vram = alloc_shared_memory(FRAMEBUFFER_SIZE, "vram")?;
        let (fb, fba) =
            framebuffer::framebuffer(vram, FRAMEBUFFER_SIZE, 0).context("creating framebuffer")?;
        resources.framebuffer_access = Some(fba);
        Some(fb)
    } else {
        None
    };

    let load_mode;
    let with_hv;

    let any_serial_configured = serial0_cfg.is_some()
        || serial1_cfg.is_some()
        || serial2_cfg.is_some()
        || serial3_cfg.is_some();

    let has_com3 = serial2_cfg.is_some();

    let mut chipset = VmManifestBuilder::new(
        if opt.igvm.is_some() {
            BaseChipsetType::HclHost
        } else if opt.pcat {
            BaseChipsetType::HypervGen1
        } else if opt.uefi {
            BaseChipsetType::HypervGen2Uefi
        } else if opt.hv {
            BaseChipsetType::HyperVGen2LinuxDirect
        } else {
            BaseChipsetType::UnenlightenedLinuxDirect
        },
        arch,
    );

    if framebuffer.is_some() {
        chipset = chipset.with_framebuffer();
    }
    if opt.guest_watchdog {
        chipset = chipset.with_guest_watchdog();
    }
    if any_serial_configured {
        chipset = chipset.with_serial([serial0_cfg, serial1_cfg, serial2_cfg, serial3_cfg]);
    }
    if opt.battery {
        let (tx, rx) = mesh::channel();
        tx.send(HostBatteryUpdate::default_present());
        chipset = chipset.with_battery(rx);
    }
    if let Some(cfg) = &opt.debugcon {
        chipset = chipset.with_debugcon(
            debugcon_cfg.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
            cfg.port,
        );
    }

    // TODO: load from VMGS file if it exists
    let bios_guid = Guid::new_random();

    let VmChipsetResult {
        chipset,
        mut chipset_devices,
        pci_chipset_devices,
        capabilities,
    } = chipset
        .build()
        .context("failed to build chipset configuration")?;

    if opt.restore_snapshot.is_some() {
        // Snapshot restore: skip firmware loading entirely. Device state and
        // memory come from the snapshot directory.
        load_mode = LoadMode::None;
        with_hv = true;
    } else if let Some(path) = &opt.igvm {
        let file = fs_err::File::open(path)
            .context("failed to open igvm file")?
            .into();
        let cmdline = opt.cmdline.join(" ");
        with_hv = true;

        load_mode = LoadMode::Igvm {
            file,
            cmdline,
            vtl2_base_address: opt.igvm_vtl2_relocation_type,
            com_serial: has_com3.then(|| SerialInformation {
                io_port: ComPort::Com3.io_port(),
                irq: ComPort::Com3.irq().into(),
            }),
        };
    } else if opt.pcat {
        // Emit a nice error early instead of complaining about missing firmware.
        if arch != MachineArch::X86_64 {
            anyhow::bail!("pcat not supported on this architecture");
        }
        with_hv = true;

        let firmware = openvmm_pcat_locator::find_pcat_bios(opt.pcat_firmware.as_deref())?;
        load_mode = LoadMode::Pcat {
            firmware,
            boot_order: opt
                .pcat_boot_order
                .map(|x| x.0)
                .unwrap_or(DEFAULT_PCAT_BOOT_ORDER),
        };
    } else if opt.uefi {
        use openvmm_defs::config::UefiConsoleMode;

        with_hv = true;

        let firmware = fs_err::File::open(
            (opt.uefi_firmware.0)
                .as_ref()
                .context("must provide uefi firmware when booting with uefi")?,
        )
        .context("failed to open uefi firmware")?;

        // TODO: It would be better to default memory protections to on, but currently Linux does not boot via UEFI due to what
        //       appears to be a GRUB memory protection fault. Memory protections are therefore only enabled if configured.
        load_mode = LoadMode::Uefi {
            firmware: firmware.into(),
            enable_debugging: opt.uefi_debug,
            enable_memory_protections: opt.uefi_enable_memory_protections,
            disable_frontpage: opt.disable_frontpage,
            enable_tpm: opt.tpm,
            enable_battery: opt.battery,
            enable_serial: any_serial_configured,
            enable_vpci_boot: false,
            uefi_console_mode: opt.uefi_console_mode.map(|m| match m {
                UefiConsoleModeCli::Default => UefiConsoleMode::Default,
                UefiConsoleModeCli::Com1 => UefiConsoleMode::Com1,
                UefiConsoleModeCli::Com2 => UefiConsoleMode::Com2,
                UefiConsoleModeCli::None => UefiConsoleMode::None,
            }),
            default_boot_always_attempt: opt.default_boot_always_attempt,
            bios_guid,
        };
    } else {
        // Linux Direct
        let mut cmdline = "panic=-1 debug".to_string();

        with_hv = opt.hv;
        if with_hv && opt.pcie_root_complex.is_empty() {
            cmdline += " pci=off";
        }

        if !console_str.is_empty() {
            let _ = write!(&mut cmdline, " console={}", console_str);
        }

        if opt.gfx {
            cmdline += " console=tty";
        }
        for extra in &opt.cmdline {
            let _ = write!(&mut cmdline, " {}", extra);
        }

        let kernel = fs_err::File::open(
            (opt.kernel.0)
                .as_ref()
                .context("must provide kernel when booting with linux direct")?,
        )
        .context("failed to open kernel")?;
        let initrd = (opt.initrd.0)
            .as_ref()
            .map(fs_err::File::open)
            .transpose()
            .context("failed to open initrd")?;

        let custom_dsdt = match &opt.custom_dsdt {
            Some(path) => {
                let mut v = Vec::new();
                fs_err::File::open(path)
                    .context("failed to open custom dsdt")?
                    .read_to_end(&mut v)
                    .context("failed to read custom dsdt")?;
                Some(v)
            }
            None => None,
        };

        load_mode = LoadMode::Linux {
            kernel: kernel.into(),
            initrd: initrd.map(Into::into),
            cmdline,
            custom_dsdt,
            enable_serial: any_serial_configured,
            boot_mode: if opt.device_tree {
                openvmm_defs::config::LinuxDirectBootMode::DeviceTree
            } else {
                openvmm_defs::config::LinuxDirectBootMode::Acpi
            },
        };
    }

    let mut vmgs = Some(if let Some(VmgsCli { kind, provision }) = &opt.vmgs {
        let disk = VmgsDisk {
            disk: disk_open(kind, false)
                .await
                .context("failed to open vmgs disk")?,
            encryption_policy: if opt.test_gsp_by_id {
                GuestStateEncryptionPolicy::GspById(true)
            } else {
                GuestStateEncryptionPolicy::None(true)
            },
        };
        match provision {
            ProvisionVmgs::OnEmpty => VmgsResource::Disk(disk),
            ProvisionVmgs::OnFailure => VmgsResource::ReprovisionOnFailure(disk),
            ProvisionVmgs::True => VmgsResource::Reprovision(disk),
        }
    } else {
        VmgsResource::Ephemeral
    });

    if with_get && with_hv {
        let vtl2_settings = vtl2_settings_proto::Vtl2Settings {
            version: vtl2_settings_proto::vtl2_settings_base::Version::V1.into(),
            fixed: Some(Default::default()),
            dynamic: Some(vtl2_settings_proto::Vtl2SettingsDynamic {
                storage_controllers: storage.build_underhill(opt.vmbus_redirect),
                nic_devices: underhill_nics,
            }),
            namespace_settings: Vec::default(),
        };

        // Cache the VTL2 settings for later modification via the interactive console.
        resources.vtl2_settings = Some(vtl2_settings.clone());

        let (send, guest_request_recv) = mesh::channel();
        resources.ged_rpc = Some(send);

        let vmgs = vmgs.take().unwrap();

        vmbus_devices.extend([
            (
                openhcl_vtl,
                get_resources::gel::GuestEmulationLogHandle.into_resource(),
            ),
            (
                openhcl_vtl,
                get_resources::ged::GuestEmulationDeviceHandle {
                    firmware: if opt.pcat {
                        get_resources::ged::GuestFirmwareConfig::Pcat {
                            boot_order: opt
                                .pcat_boot_order
                                .map_or(DEFAULT_PCAT_BOOT_ORDER, |x| x.0)
                                .map(|x| match x {
                                    openvmm_defs::config::PcatBootDevice::Floppy => {
                                        get_resources::ged::PcatBootDevice::Floppy
                                    }
                                    openvmm_defs::config::PcatBootDevice::HardDrive => {
                                        get_resources::ged::PcatBootDevice::HardDrive
                                    }
                                    openvmm_defs::config::PcatBootDevice::Optical => {
                                        get_resources::ged::PcatBootDevice::Optical
                                    }
                                    openvmm_defs::config::PcatBootDevice::Network => {
                                        get_resources::ged::PcatBootDevice::Network
                                    }
                                }),
                        }
                    } else {
                        use get_resources::ged::UefiConsoleMode;

                        get_resources::ged::GuestFirmwareConfig::Uefi {
                            enable_vpci_boot: storage.has_vtl0_nvme(),
                            firmware_debug: opt.uefi_debug,
                            disable_frontpage: opt.disable_frontpage,
                            console_mode: match opt.uefi_console_mode.unwrap_or(UefiConsoleModeCli::Default) {
                                UefiConsoleModeCli::Default => UefiConsoleMode::Default,
                                UefiConsoleModeCli::Com1 => UefiConsoleMode::COM1,
                                UefiConsoleModeCli::Com2 => UefiConsoleMode::COM2,
                                UefiConsoleModeCli::None => UefiConsoleMode::None,
                            },
                            default_boot_always_attempt: opt.default_boot_always_attempt,
                        }
                    },
                    com1: with_vmbus_com1_serial,
                    com2: with_vmbus_com2_serial,
                    serial_tx_only: opt.serial_tx_only,
                    vtl2_settings: Some(prost::Message::encode_to_vec(&vtl2_settings)),
                    vmbus_redirection: opt.vmbus_redirect,
                    vmgs,
                    framebuffer: opt
                        .vtl2_gfx
                        .then(|| SharedFramebufferHandle.into_resource()),
                    guest_request_recv,
                    enable_tpm: opt.tpm,
                    firmware_event_send: None,
                    secure_boot_enabled: opt.secure_boot,
                    secure_boot_template: match opt.secure_boot_template {
                        Some(SecureBootTemplateCli::Windows) => {
                            get_resources::ged::GuestSecureBootTemplateType::MicrosoftWindows
                        },
                        Some(SecureBootTemplateCli::UefiCa) => {
                            get_resources::ged::GuestSecureBootTemplateType::MicrosoftUefiCertificateAuthority
                        }
                        None => {
                            get_resources::ged::GuestSecureBootTemplateType::None
                        },
                    },
                    enable_battery: opt.battery,
                    no_persistent_secrets: true,
                    igvm_attest_test_config: None,
                    test_gsp_by_id: opt.test_gsp_by_id,
                    efi_diagnostics_log_level: {
                        match opt.efi_diagnostics_log_level.unwrap_or_default() {
                            EfiDiagnosticsLogLevelCli::Default => get_resources::ged::EfiDiagnosticsLogLevelType::Default,
                            EfiDiagnosticsLogLevelCli::Info => get_resources::ged::EfiDiagnosticsLogLevelType::Info,
                            EfiDiagnosticsLogLevelCli::Full => get_resources::ged::EfiDiagnosticsLogLevelType::Full,
                        }
                    },
                    hv_sint_enabled: false,
                }
                .into_resource(),
            ),
        ]);
    }

    if opt.tpm && !opt.vtl2 {
        let register_layout = if cfg!(guest_arch = "x86_64") {
            TpmRegisterLayout::IoPort
        } else {
            TpmRegisterLayout::Mmio
        };

        let (ppi_store, nvram_store) = if opt.vmgs.is_some() {
            (
                VmgsFileHandle::new(vmgs_format::FileId::TPM_PPI, true).into_resource(),
                VmgsFileHandle::new(vmgs_format::FileId::TPM_NVRAM, true).into_resource(),
            )
        } else {
            (
                EphemeralNonVolatileStoreHandle.into_resource(),
                EphemeralNonVolatileStoreHandle.into_resource(),
            )
        };

        chipset_devices.push(ChipsetDeviceHandle {
            name: "tpm".to_string(),
            resource: chipset_device_worker_defs::RemoteChipsetDeviceHandle {
                device: TpmDeviceHandle {
                    ppi_store,
                    nvram_store,
                    nvram_size: None,
                    refresh_tpm_seeds: false,
                    ak_cert_type: tpm_resources::TpmAkCertTypeResource::None,
                    register_layout,
                    guest_secret_key: None,
                    logger: None,
                    is_confidential_vm: false,
                    bios_guid,
                }
                .into_resource(),
                worker_host: mesh.make_host("tpm", None).await?,
            }
            .into_resource(),
        });
    }

    let custom_uefi_vars = {
        use firmware_uefi_custom_vars::CustomVars;

        // load base vars from specified template, or use an empty set of base
        // vars if none was specified.
        let base_vars = match opt.secure_boot_template {
            Some(template) => match (arch, template) {
                (MachineArch::X86_64, SecureBootTemplateCli::Windows) => {
                    hyperv_secure_boot_templates::x64::microsoft_windows()
                }
                (MachineArch::X86_64, SecureBootTemplateCli::UefiCa) => {
                    hyperv_secure_boot_templates::x64::microsoft_uefi_ca()
                }
                (MachineArch::Aarch64, SecureBootTemplateCli::Windows) => {
                    hyperv_secure_boot_templates::aarch64::microsoft_windows()
                }
                (MachineArch::Aarch64, SecureBootTemplateCli::UefiCa) => {
                    hyperv_secure_boot_templates::aarch64::microsoft_uefi_ca()
                }
            },
            None => CustomVars::default(),
        };

        // TODO: fallback to VMGS read if no command line flag was given

        let custom_uefi_json_data = match &opt.custom_uefi_json {
            Some(file) => Some(fs_err::read(file).context("opening custom uefi json file")?),
            None => None,
        };

        // obtain the final custom uefi vars by applying the delta onto the base vars
        match custom_uefi_json_data {
            Some(data) => {
                let delta = hyperv_uefi_custom_vars_json::load_delta_from_json(&data)?;
                base_vars.apply_delta(delta)?
            }
            None => base_vars,
        }
    };

    let vga_firmware = if opt.pcat {
        Some(openvmm_pcat_locator::find_svga_bios(
            opt.vga_firmware.as_deref(),
        )?)
    } else {
        None
    };

    if opt.gfx {
        vmbus_devices.extend([
            (
                DeviceVtl::Vtl0,
                SynthVideoHandle {
                    framebuffer: SharedFramebufferHandle.into_resource(),
                }
                .into_resource(),
            ),
            (
                DeviceVtl::Vtl0,
                SynthKeyboardHandle {
                    source: MultiplexedInputHandle {
                        // Save 0 for PS/2
                        elevation: 1,
                    }
                    .into_resource(),
                }
                .into_resource(),
            ),
            (
                DeviceVtl::Vtl0,
                SynthMouseHandle {
                    source: MultiplexedInputHandle {
                        // Save 0 for PS/2
                        elevation: 1,
                    }
                    .into_resource(),
                }
                .into_resource(),
            ),
        ]);
    }

    let vsock_listener = |path: Option<&str>| -> anyhow::Result<_> {
        if let Some(path) = path {
            cleanup_socket(path.as_ref());
            let listener = unix_socket::UnixListener::bind(path)
                .with_context(|| format!("failed to bind to hybrid vsock path: {}", path))?;
            Ok(Some(listener))
        } else {
            Ok(None)
        }
    };

    let vtl0_vsock_listener = vsock_listener(opt.vmbus_vsock_path.as_deref())?;
    let vtl2_vsock_listener = vsock_listener(opt.vmbus_vtl2_vsock_path.as_deref())?;

    if let Some(path) = &opt.openhcl_dump_path {
        let (resource, task) = spawn_dump_handler(&spawner, path.clone(), None);
        task.detach();
        vmbus_devices.push((openhcl_vtl, resource));
    }

    #[cfg(guest_arch = "aarch64")]
    let topology_arch = openvmm_defs::config::ArchTopologyConfig::Aarch64(
        openvmm_defs::config::Aarch64TopologyConfig {
            // TODO: allow this to be configured from the command line
            gic_config: None,
            pmu_gsiv: openvmm_defs::config::PmuGsivConfig::Platform,
        },
    );
    #[cfg(guest_arch = "x86_64")]
    let topology_arch =
        openvmm_defs::config::ArchTopologyConfig::X86(openvmm_defs::config::X86TopologyConfig {
            apic_id_offset: opt.apic_id_offset,
            x2apic: opt.x2apic,
        });

    let with_isolation = if let Some(isolation) = &opt.isolation {
        // TODO: For now, isolation is only supported with VTL2.
        if !opt.vtl2 {
            anyhow::bail!("isolation is only currently supported with vtl2");
        }

        // TODO: Alias map support is not yet implement with isolation.
        if !opt.no_alias_map {
            anyhow::bail!("alias map not supported with isolation");
        }

        match isolation {
            cli_args::IsolationCli::Vbs => Some(openvmm_defs::config::IsolationType::Vbs),
        }
    } else {
        None
    };

    if with_hv {
        let (shutdown_send, shutdown_recv) = mesh::channel();
        resources.shutdown_ic = Some(shutdown_send);
        let (kvp_send, kvp_recv) = mesh::channel();
        resources.kvp_ic = Some(kvp_send);
        vmbus_devices.extend(
            [
                hyperv_ic_resources::shutdown::ShutdownIcHandle {
                    recv: shutdown_recv,
                }
                .into_resource(),
                hyperv_ic_resources::kvp::KvpIcHandle { recv: kvp_recv }.into_resource(),
                hyperv_ic_resources::timesync::TimesyncIcHandle.into_resource(),
            ]
            .map(|r| (DeviceVtl::Vtl0, r)),
        );
    }

    if let Some(hive_path) = &opt.imc {
        let file = fs_err::File::open(hive_path).context("failed to open imc hive")?;
        vmbus_devices.push((
            DeviceVtl::Vtl0,
            vmbfs_resources::VmbfsImcDeviceHandle { file: file.into() }.into_resource(),
        ));
    }

    let mut virtio_devices = Vec::new();
    let mut add_virtio_device = |bus, resource: Resource<VirtioDeviceHandle>| {
        let bus = match bus {
            VirtioBusCli::Auto => {
                // Use VPCI when possible (currently only on Windows and macOS due
                // to KVM backend limitations).
                if with_hv && (cfg!(windows) || cfg!(target_os = "macos")) {
                    None
                } else {
                    Some(VirtioBus::Pci)
                }
            }
            VirtioBusCli::Mmio => Some(VirtioBus::Mmio),
            VirtioBusCli::Pci => Some(VirtioBus::Pci),
            VirtioBusCli::Vpci => None,
        };
        if let Some(bus) = bus {
            virtio_devices.push((bus, resource));
        } else {
            vpci_devices.push(VpciDeviceConfig {
                vtl: DeviceVtl::Vtl0,
                instance_id: Guid::new_random(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        }
    };

    for cli_cfg in &opt.virtio_net {
        if cli_cfg.underhill {
            anyhow::bail!("use --net uh:[...] to add underhill NICs")
        }
        let vport = parse_endpoint(cli_cfg, &mut nic_index, &mut resources)?;
        let resource = virtio_resources::net::VirtioNetHandle {
            max_queues: vport.max_queues,
            mac_address: vport.mac_address,
            endpoint: vport.endpoint,
        }
        .into_resource();
        if let Some(pcie_port) = &cli_cfg.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    for args in &opt.virtio_fs {
        let resource: Resource<VirtioDeviceHandle> = virtio_resources::fs::VirtioFsHandle {
            tag: args.tag.clone(),
            fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                root_path: args.path.clone(),
                mount_options: args.options.clone(),
            },
        }
        .into_resource();
        if let Some(pcie_port) = &args.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(opt.virtio_fs_bus, resource);
        }
    }

    for args in &opt.virtio_fs_shmem {
        let resource: Resource<VirtioDeviceHandle> = virtio_resources::fs::VirtioFsHandle {
            tag: args.tag.clone(),
            fs: virtio_resources::fs::VirtioFsBackend::SectionFs {
                root_path: args.path.clone(),
            },
        }
        .into_resource();
        if let Some(pcie_port) = &args.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(opt.virtio_fs_bus, resource);
        }
    }

    for args in &opt.virtio_9p {
        let resource: Resource<VirtioDeviceHandle> = virtio_resources::p9::VirtioPlan9Handle {
            tag: args.tag.clone(),
            root_path: args.path.clone(),
            debug: opt.virtio_9p_debug,
        }
        .into_resource();
        if let Some(pcie_port) = &args.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    if let Some(pmem_args) = &opt.virtio_pmem {
        let resource: Resource<VirtioDeviceHandle> = virtio_resources::pmem::VirtioPmemHandle {
            path: pmem_args.path.clone(),
        }
        .into_resource();
        if let Some(pcie_port) = &pmem_args.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    if opt.virtio_rng {
        let resource: Resource<VirtioDeviceHandle> =
            virtio_resources::rng::VirtioRngHandle.into_resource();
        if let Some(pcie_port) = &opt.virtio_rng_pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(opt.virtio_rng_bus, resource);
        }
    }

    if let Some(backend) = virtio_console_backend {
        let resource: Resource<VirtioDeviceHandle> =
            virtio_resources::console::VirtioConsoleHandle { backend }.into_resource();
        if let Some(pcie_port) = &opt.virtio_console_pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    // Handle --vhost-user arguments.
    #[cfg(target_os = "linux")]
    for vhost_cli in &opt.vhost_user {
        let stream =
            unix_socket::UnixStream::connect(&vhost_cli.socket_path).with_context(|| {
                format!(
                    "failed to connect to vhost-user socket: {}",
                    vhost_cli.socket_path
                )
            })?;

        use crate::cli_args::VhostUserDeviceTypeCli;
        let resource: Resource<VirtioDeviceHandle> = match vhost_cli.device_type {
            VhostUserDeviceTypeCli::Fs {
                ref tag,
                num_queues,
                queue_size,
            } => virtio_resources::vhost_user::VhostUserFsHandle {
                socket: stream.into(),
                tag: tag.clone(),
                num_queues,
                queue_size,
            }
            .into_resource(),
            VhostUserDeviceTypeCli::Blk {
                num_queues,
                queue_size,
            } => virtio_resources::vhost_user::VhostUserBlkHandle {
                socket: stream.into(),
                num_queues,
                queue_size,
            }
            .into_resource(),
            VhostUserDeviceTypeCli::Other {
                device_id,
                ref queue_sizes,
            } => virtio_resources::vhost_user::VhostUserGenericHandle {
                socket: stream.into(),
                device_id,
                queue_sizes: queue_sizes.clone(),
            }
            .into_resource(),
        };
        if let Some(pcie_port) = &vhost_cli.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    if let Some(vsock_path) = &opt.virtio_vsock_path {
        let listener = vsock_listener(Some(vsock_path))?.unwrap();
        add_virtio_device(
            VirtioBusCli::Auto,
            virtio_resources::vsock::VirtioVsockHandle {
                // The guest CID does not matter since the UDS relay does not use it. It just needs
                // to be some non-reserved value for the guest to use.
                guest_cid: 0x3,
                base_path: vsock_path.clone(),
                listener,
            }
            .into_resource(),
        );
    }

    let mut cfg = Config {
        chipset,
        load_mode,
        floppy_disks,
        pcie_root_complexes,
        #[cfg(target_os = "linux")]
        pcie_devices: {
            let mut devs = pcie_devices;
            devs.extend(vfio_pcie_devices);
            devs
        },
        #[cfg(not(target_os = "linux"))]
        pcie_devices,
        pcie_switches,
        vpci_devices,
        ide_disks: Vec::new(),
        memory: MemoryConfig {
            mem_size: if let Some(ref sizes) = opt.numa_memory {
                sizes
                    .iter()
                    .try_fold(0u64, |acc, &s| acc.checked_add(s))
                    .context("numa memory sizes overflow")?
            } else {
                opt.memory_size()
            },
            mmio_gaps,
            prefetch_memory: opt.prefetch_memory(),
            private_memory: opt.private_memory(),
            transparent_hugepages: opt.transparent_hugepages(),
            hugepages: opt.memory.hugepages,
            hugepage_size: opt.memory.hugepage_size,
            pci_ecam_gaps,
            pci_mmio_gaps,
            numa_mem_sizes: opt.numa_memory.clone(),
        },
        processor_topology: ProcessorTopologyConfig {
            proc_count: opt.processors,
            vps_per_socket: opt.vps_per_socket,
            enable_smt: match opt.smt {
                cli_args::SmtConfigCli::Auto => None,
                cli_args::SmtConfigCli::Force => Some(true),
                cli_args::SmtConfigCli::Off => Some(false),
            },
            arch: Some(topology_arch),
        },
        hypervisor: HypervisorConfig {
            with_hv,
            with_vtl2: opt.vtl2.then_some(Vtl2Config {
                vtl0_alias_map: !opt.no_alias_map,
                late_map_vtl0_memory: match opt.late_map_vtl0_policy {
                    cli_args::Vtl0LateMapPolicyCli::Off => None,
                    cli_args::Vtl0LateMapPolicyCli::Log => Some(LateMapVtl0MemoryPolicy::Log),
                    cli_args::Vtl0LateMapPolicyCli::Halt => Some(LateMapVtl0MemoryPolicy::Halt),
                    cli_args::Vtl0LateMapPolicyCli::Exception => {
                        Some(LateMapVtl0MemoryPolicy::InjectException)
                    }
                },
            }),
            with_isolation,
        },
        #[cfg(windows)]
        kernel_vmnics,
        input: mesh::Receiver::new(),
        framebuffer,
        vga_firmware,
        vtl2_gfx: opt.vtl2_gfx,
        virtio_devices,
        vmbus: with_hv.then_some(VmbusConfig {
            vsock_listener: vtl0_vsock_listener,
            vsock_path: opt.vmbus_vsock_path.clone(),
            vtl2_redirect: opt.vmbus_redirect,
            vmbus_max_version: opt.vmbus_max_version,
            #[cfg(windows)]
            vmbusproxy_handle,
        }),
        vtl2_vmbus: (with_hv && opt.vtl2).then_some(VmbusConfig {
            vsock_listener: vtl2_vsock_listener,
            vsock_path: opt.vmbus_vtl2_vsock_path.clone(),
            ..Default::default()
        }),
        vmbus_devices,
        chipset_devices,
        pci_chipset_devices,
        chipset_capabilities: capabilities,
        #[cfg(windows)]
        vpci_resources,
        vmgs,
        secure_boot_enabled: opt.secure_boot,
        custom_uefi_vars,
        firmware_event_send: None,
        debugger_rpc: None,
        generation_id_recv: None,
        rtc_delta_milliseconds: 0,
        automatic_guest_reset: !opt.halt_on_reset,
        efi_diagnostics_log_level: {
            match opt.efi_diagnostics_log_level.unwrap_or_default() {
                EfiDiagnosticsLogLevelCli::Default => EfiDiagnosticsLogLevelType::Default,
                EfiDiagnosticsLogLevelCli::Info => EfiDiagnosticsLogLevelType::Info,
                EfiDiagnosticsLogLevelCli::Full => EfiDiagnosticsLogLevelType::Full,
            }
        },
    };

    storage.build_config(&mut cfg, &mut resources, opt.scsi_sub_channels)?;
    Ok((cfg, resources))
}

/// Gets the terminal to use for externally launched console windows.
pub(crate) fn openvmm_terminal_app() -> Option<PathBuf> {
    std::env::var_os("OPENVMM_TERM")
        .or_else(|| std::env::var_os("HVLITE_TERM"))
        .map(Into::into)
}

// Tries to remove `path` if it is confirmed to be a Unix socket.
fn cleanup_socket(path: &Path) {
    #[cfg(windows)]
    let is_socket = pal::windows::fs::is_unix_socket(path).unwrap_or(false);
    #[cfg(not(windows))]
    let is_socket = path
        .metadata()
        .is_ok_and(|meta| std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()));

    if is_socket {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
const DEFAULT_SWITCH: &str = "C08CB7B8-9B3C-408E-8E30-5E16A3AEB444";

#[cfg(windows)]
fn new_switch_port(
    switch_id: &str,
) -> anyhow::Result<(
    openvmm_defs::config::SwitchPortId,
    vmswitch::kernel::SwitchPort,
)> {
    let id = vmswitch::kernel::SwitchPortId {
        switch: switch_id.parse().context("invalid switch id")?,
        port: Guid::new_random(),
    };
    let _ = vmswitch::hcn::Network::open(&id.switch)
        .with_context(|| format!("could not find switch {}", id.switch))?;

    let port = vmswitch::kernel::SwitchPort::new(&id).context("failed to create switch port")?;

    let id = openvmm_defs::config::SwitchPortId {
        switch: id.switch,
        port: id.port,
    };
    Ok((id, port))
}

fn parse_endpoint(
    cli_cfg: &NicConfigCli,
    index: &mut usize,
    resources: &mut VmResources,
) -> anyhow::Result<NicConfig> {
    let _ = resources;
    let endpoint = match &cli_cfg.endpoint {
        EndpointConfigCli::Consomme { cidr, host_fwd } => {
            let ports = host_fwd
                .iter()
                .map(|fwd| {
                    use net_backend_resources::consomme::HostPortProtocol;
                    net_backend_resources::consomme::HostPortConfig {
                        protocol: match fwd.protocol {
                            cli_args::HostPortProtocolCli::Tcp => HostPortProtocol::Tcp,
                            cli_args::HostPortProtocolCli::Udp => HostPortProtocol::Udp,
                        },
                        host_address: fwd
                            .host_address
                            .map(net_backend_resources::consomme::HostIpAddress::from),
                        host_port: fwd.host_port,
                        guest_port: fwd.guest_port,
                    }
                })
                .collect();
            net_backend_resources::consomme::ConsommeHandle {
                cidr: cidr.clone(),
                ports,
            }
            .into_resource()
        }
        EndpointConfigCli::None => net_backend_resources::null::NullHandle.into_resource(),
        EndpointConfigCli::Dio { id } => {
            #[cfg(windows)]
            {
                let (port_id, port) = new_switch_port(id.as_deref().unwrap_or(DEFAULT_SWITCH))?;
                resources.switch_ports.push(port);
                net_backend_resources::dio::WindowsDirectIoHandle {
                    switch_port_id: net_backend_resources::dio::SwitchPortId {
                        switch: port_id.switch,
                        port: port_id.port,
                    },
                }
                .into_resource()
            }

            #[cfg(not(windows))]
            {
                let _ = id;
                bail!("cannot use dio on non-windows platforms")
            }
        }
        EndpointConfigCli::Tap { name } => {
            #[cfg(target_os = "linux")]
            {
                let fd = net_tap::tap::open_tap(name)
                    .with_context(|| format!("failed to open TAP device '{name}'"))?;
                net_backend_resources::tap::TapHandle { fd }.into_resource()
            }

            #[cfg(not(target_os = "linux"))]
            {
                let _ = name;
                bail!("TAP backend is only supported on Linux")
            }
        }
    };

    // Pick a random MAC address.
    let mut mac_address = [0x00, 0x15, 0x5D, 0, 0, 0];
    getrandom::fill(&mut mac_address[3..]).expect("rng failure");

    // Pick a fixed instance ID based on the index.
    const BASE_INSTANCE_ID: Guid = guid::guid!("00000000-da43-11ed-936a-00155d6db52f");
    let instance_id = Guid {
        data1: *index as u32,
        ..BASE_INSTANCE_ID
    };
    *index += 1;

    Ok(NicConfig {
        vtl: cli_cfg.vtl,
        instance_id,
        endpoint,
        mac_address: mac_address.into(),
        max_queues: cli_cfg.max_queues,
        pcie_port: cli_cfg.pcie_port.clone(),
    })
}

#[derive(Debug)]
struct NicConfig {
    vtl: DeviceVtl,
    instance_id: Guid,
    mac_address: MacAddress,
    endpoint: Resource<NetEndpointHandleKind>,
    max_queues: Option<u16>,
    pcie_port: Option<String>,
}

impl NicConfig {
    fn into_netvsp_handle(self) -> (DeviceVtl, Resource<VmbusDeviceHandleKind>) {
        (
            self.vtl,
            netvsp_resources::NetvspHandle {
                instance_id: self.instance_id,
                mac_address: self.mac_address,
                endpoint: self.endpoint,
                max_queues: self.max_queues,
            }
            .into_resource(),
        )
    }
}

enum LayerOrDisk {
    Layer(DiskLayerDescription),
    Disk(Resource<DiskHandleKind>),
}

async fn disk_open(
    disk_cli: &DiskCliKind,
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    let mut layers = Vec::new();
    disk_open_inner(disk_cli, read_only, &mut layers).await?;
    if layers.len() == 1 && matches!(layers[0], LayerOrDisk::Disk(_)) {
        let LayerOrDisk::Disk(disk) = layers.pop().unwrap() else {
            unreachable!()
        };
        Ok(disk)
    } else {
        Ok(Resource::new(disk_backend_resources::LayeredDiskHandle {
            layers: layers
                .into_iter()
                .map(|layer| match layer {
                    LayerOrDisk::Layer(layer) => layer,
                    LayerOrDisk::Disk(disk) => DiskLayerDescription {
                        layer: DiskLayerHandle(disk).into_resource(),
                        read_cache: false,
                        write_through: false,
                    },
                })
                .collect(),
        }))
    }
}

fn disk_open_inner<'a>(
    disk_cli: &'a DiskCliKind,
    read_only: bool,
    layers: &'a mut Vec<LayerOrDisk>,
) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
        fn layer<T: IntoResource<DiskLayerHandleKind>>(layer: T) -> LayerOrDisk {
            LayerOrDisk::Layer(layer.into_resource().into())
        }
        fn disk<T: IntoResource<DiskHandleKind>>(disk: T) -> LayerOrDisk {
            LayerOrDisk::Disk(disk.into_resource())
        }
        match disk_cli {
            &DiskCliKind::Memory(len) => {
                layers.push(layer(RamDiskLayerHandle {
                    len: Some(len),
                    sector_size: None,
                }));
            }
            DiskCliKind::File {
                path,
                create_with_len,
                direct,
            } => layers.push(LayerOrDisk::Disk(if let Some(size) = create_with_len {
                create_disk_type(
                    path,
                    *size,
                    OpenDiskOptions {
                        read_only: false,
                        direct: *direct,
                    },
                )
                .with_context(|| format!("failed to create {}", path.display()))?
            } else {
                open_disk_type(
                    path,
                    OpenDiskOptions {
                        read_only,
                        direct: *direct,
                    },
                )
                .await
                .with_context(|| format!("failed to open {}", path.display()))?
            })),
            DiskCliKind::Blob { kind, url } => {
                layers.push(disk(disk_backend_resources::BlobDiskHandle {
                    url: url.to_owned(),
                    format: match kind {
                        cli_args::BlobKind::Flat => disk_backend_resources::BlobDiskFormat::Flat,
                        cli_args::BlobKind::Vhd1 => {
                            disk_backend_resources::BlobDiskFormat::FixedVhd1
                        }
                    },
                }))
            }
            DiskCliKind::MemoryDiff(inner) => {
                layers.push(layer(RamDiskLayerHandle {
                    len: None,
                    sector_size: None,
                }));
                disk_open_inner(inner, true, layers).await?;
            }
            DiskCliKind::PersistentReservationsWrapper(inner) => {
                layers.push(disk(disk_backend_resources::DiskWithReservationsHandle(
                    disk_open(inner, read_only).await?,
                )))
            }
            DiskCliKind::DelayDiskWrapper {
                delay_ms,
                disk: inner,
            } => layers.push(disk(DelayDiskHandle {
                delay: CellUpdater::new(Duration::from_millis(*delay_ms)).cell(),
                disk: disk_open(inner, read_only).await?,
            })),
            DiskCliKind::Crypt {
                disk: inner,
                cipher,
                key_file,
            } => layers.push(disk(disk_crypt_resources::DiskCryptHandle {
                disk: disk_open(inner, read_only).await?,
                cipher: match cipher {
                    cli_args::DiskCipher::XtsAes256 => disk_crypt_resources::Cipher::XtsAes256,
                },
                key: fs_err::read(key_file).context("failed to read key file")?,
            })),
            DiskCliKind::Sqlite {
                path,
                create_with_len,
            } => {
                // FUTURE: this code should be responsible for opening
                // file-handle(s) itself, and passing them into sqlite via a custom
                // vfs. For now though - simply check if the file exists or not, and
                // perform early validation of filesystem-level create options.
                match (create_with_len.is_some(), path.exists()) {
                    (true, true) => anyhow::bail!(
                        "cannot create new sqlite disk at {} - file already exists",
                        path.display()
                    ),
                    (false, false) => anyhow::bail!(
                        "cannot open sqlite disk at {} - file not found",
                        path.display()
                    ),
                    _ => {}
                }

                layers.push(layer(SqliteDiskLayerHandle {
                    dbhd_path: path.display().to_string(),
                    format_dbhd: create_with_len.map(|len| {
                        disk_backend_resources::layer::SqliteDiskLayerFormatParams {
                            logically_read_only: false,
                            len: Some(len),
                        }
                    }),
                }));
            }
            DiskCliKind::SqliteDiff { path, create, disk } => {
                // FUTURE: this code should be responsible for opening
                // file-handle(s) itself, and passing them into sqlite via a custom
                // vfs. For now though - simply check if the file exists or not, and
                // perform early validation of filesystem-level create options.
                match (create, path.exists()) {
                    (true, true) => anyhow::bail!(
                        "cannot create new sqlite disk at {} - file already exists",
                        path.display()
                    ),
                    (false, false) => anyhow::bail!(
                        "cannot open sqlite disk at {} - file not found",
                        path.display()
                    ),
                    _ => {}
                }

                layers.push(layer(SqliteDiskLayerHandle {
                    dbhd_path: path.display().to_string(),
                    format_dbhd: create.then_some(
                        disk_backend_resources::layer::SqliteDiskLayerFormatParams {
                            logically_read_only: false,
                            len: None,
                        },
                    ),
                }));
                disk_open_inner(disk, true, layers).await?;
            }
            DiskCliKind::AutoCacheSqlite {
                cache_path,
                key,
                disk,
            } => {
                layers.push(LayerOrDisk::Layer(DiskLayerDescription {
                    read_cache: true,
                    write_through: false,
                    layer: SqliteAutoCacheDiskLayerHandle {
                        cache_path: cache_path.clone(),
                        cache_key: key.clone(),
                    }
                    .into_resource(),
                }));
                disk_open_inner(disk, read_only, layers).await?;
            }
        }
        Ok(())
    })
}

/// Get the system page size.
pub(crate) fn system_page_size() -> u32 {
    sparse_mmap::SparseMapping::page_size() as u32
}

/// The guest architecture string, derived from the compile-time `guest_arch` cfg.
pub(crate) const GUEST_ARCH: &str = if cfg!(guest_arch = "x86_64") {
    "x86_64"
} else {
    "aarch64"
};

/// Open a snapshot directory and validate it against the current VM config.
/// Returns the shared memory fd (from memory.bin) and the saved device state.
fn prepare_snapshot_restore(
    snapshot_dir: &Path,
    opt: &Options,
) -> anyhow::Result<(
    openvmm_defs::worker::SharedMemoryFd,
    mesh::payload::message::ProtobufMessage,
)> {
    let (manifest, state_bytes) = openvmm_helpers::snapshot::read_snapshot(snapshot_dir)?;

    // Validate manifest against current VM config.
    openvmm_helpers::snapshot::validate_manifest(
        &manifest,
        GUEST_ARCH,
        opt.memory_size(),
        opt.processors,
        system_page_size(),
    )?;

    // Open memory.bin (existing file, no create, no resize).
    let memory_file = fs_err::OpenOptions::new()
        .read(true)
        .write(true)
        .open(snapshot_dir.join("memory.bin"))?;

    // Validate file size matches expected memory size.
    let file_size = memory_file.metadata()?.len();
    if file_size != manifest.memory_size_bytes {
        anyhow::bail!(
            "memory.bin size ({file_size} bytes) doesn't match manifest ({} bytes)",
            manifest.memory_size_bytes,
        );
    }

    let shared_memory_fd =
        openvmm_helpers::shared_memory::file_to_shared_memory_fd(memory_file.into())?;

    // Reconstruct ProtobufMessage from the saved state bytes.
    // The save side wrote mesh::payload::encode(ProtobufMessage), so we decode
    // back to ProtobufMessage.
    let state_msg: mesh::payload::message::ProtobufMessage = mesh::payload::decode(&state_bytes)
        .context("failed to decode saved state from snapshot")?;

    Ok((shared_memory_fd, state_msg))
}

fn do_main(pidfile_path: &mut Option<PathBuf>) -> anyhow::Result<()> {
    #[cfg(windows)]
    pal::windows::disable_hard_error_dialog();

    tracing_init::enable_tracing()?;

    // Try to run as a worker host.
    // On success the worker runs to completion and then exits the process (does
    // not return). Any worker host setup errors are return and bubbled up.
    meshworker::run_vmm_mesh_host()?;

    let opt = Options::parse();
    if let Some(path) = &opt.write_saved_state_proto {
        mesh::payload::protofile::DescriptorWriter::new(vmcore::save_restore::saved_state_roots())
            .write_to_path(path)
            .context("failed to write protobuf descriptors")?;
        return Ok(());
    }

    if let Some(ref path) = opt.pidfile {
        std::fs::write(path, format!("{}\n", std::process::id()))
            .context("failed to write pidfile")?;
        *pidfile_path = Some(path.clone());
    }

    if let Some(path) = opt.relay_console_path {
        let console_title = opt.relay_console_title.unwrap_or_default();
        return console_relay::relay_console(&path, console_title.as_str());
    }

    #[cfg(any(feature = "grpc", feature = "ttrpc"))]
    if let Some(path) = opt.ttrpc.as_ref().or(opt.grpc.as_ref()) {
        return block_on(async {
            let _ = std::fs::remove_file(path);
            let listener =
                unix_socket::UnixListener::bind(path).context("failed to bind to socket")?;

            let transport = if opt.ttrpc.is_some() {
                ttrpc::RpcTransport::Ttrpc
            } else {
                ttrpc::RpcTransport::Grpc
            };

            // This is a local launch
            let mut handle =
                mesh_worker::launch_local_worker::<ttrpc::TtrpcWorker>(ttrpc::Parameters {
                    listener,
                    transport,
                })
                .await?;

            tracing::info!(%transport, path = %path.display(), "listening");

            // Signal the the parent process that the server is ready.
            pal::close_stdout().context("failed to close stdout")?;

            handle.join().await?;

            Ok(())
        });
    }

    DefaultPool::run_with(async |driver| run_control(&driver, opt).await)
}

fn new_hvsock_service_id(port: u32) -> Guid {
    // This GUID is an embedding of the AF_VSOCK port into an
    // AF_HYPERV service ID.
    Guid {
        data1: port,
        .."00000000-facb-11e6-bd58-64006a7986d3".parse().unwrap()
    }
}

async fn run_control(driver: &DefaultDriver, opt: Options) -> anyhow::Result<()> {
    let mut mesh = Some(VmmMesh::new(&driver, opt.single_process)?);
    let result = run_control_inner(driver, &mut mesh, opt).await;
    // If setup failed before the mesh was handed to the controller, shut it
    // down so the child host process exits cleanly without noisy logs.
    if let Some(mesh) = mesh {
        mesh.shutdown().await;
    }
    result
}

async fn run_control_inner(
    driver: &DefaultDriver,
    mesh_slot: &mut Option<VmmMesh>,
    opt: Options,
) -> anyhow::Result<()> {
    let mesh = mesh_slot.as_ref().unwrap();
    let (mut vm_config, mut resources) = vm_config_from_command_line(driver, mesh, &opt).await?;

    let mut vnc_worker = None;
    if opt.gfx || opt.vnc {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", opt.vnc_port))
            .with_context(|| format!("binding to VNC port {}", opt.vnc_port))?;

        let input_send = vm_config.input.sender();
        let framebuffer = resources
            .framebuffer_access
            .take()
            .expect("synth video enabled");

        let vnc_host = mesh
            .make_host("vnc", None)
            .await
            .context("spawning vnc process failed")?;

        vnc_worker = Some(
            vnc_host
                .launch_worker(
                    vnc_worker_defs::VNC_WORKER_TCP,
                    VncParameters {
                        listener,
                        framebuffer,
                        input_send,
                    },
                )
                .await?,
        )
    }

    // spin up the debug worker
    let gdb_worker = if let Some(port) = opt.gdb {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
            .with_context(|| format!("binding to gdb port {}", port))?;

        let (req_tx, req_rx) = mesh::channel();
        vm_config.debugger_rpc = Some(req_rx);

        let gdb_host = mesh
            .make_host("gdb", None)
            .await
            .context("spawning gdbstub process failed")?;

        Some(
            gdb_host
                .launch_worker(
                    debug_worker_defs::DEBUGGER_WORKER,
                    debug_worker_defs::DebuggerParameters {
                        listener,
                        req_chan: req_tx,
                        vp_count: vm_config.processor_topology.proc_count,
                        target_arch: if cfg!(guest_arch = "x86_64") {
                            debug_worker_defs::TargetArch::X86_64
                        } else {
                            debug_worker_defs::TargetArch::Aarch64
                        },
                    },
                )
                .await
                .context("failed to launch gdbstub worker")?,
        )
    } else {
        None
    };

    // spin up the VM
    let (vm_rpc, rpc_recv) = mesh::channel();
    let (notify_send, notify_recv) = mesh::channel();
    let vm_worker = {
        let vm_host = mesh.make_host("vm", opt.log_file.clone()).await?;

        let (shared_memory, saved_state) = if let Some(snapshot_dir) = &opt.restore_snapshot {
            let (fd, state_msg) = prepare_snapshot_restore(snapshot_dir, &opt)?;
            (Some(fd), Some(state_msg))
        } else {
            let shared_memory = opt
                .memory_backing_file()
                .map(|path| {
                    openvmm_helpers::shared_memory::open_memory_backing_file(
                        path,
                        opt.memory_size(),
                    )
                })
                .transpose()?;
            (shared_memory, None)
        };

        let params = VmWorkerParameters {
            hypervisor: match &opt.hypervisor {
                Some(name) => openvmm_helpers::hypervisor::hypervisor_resource(name)?,
                None => openvmm_helpers::hypervisor::choose_hypervisor()?,
            },
            cfg: vm_config,
            saved_state,
            shared_memory,
            rpc: rpc_recv,
            notify: notify_send,
        };
        vm_host
            .launch_worker(VM_WORKER, params)
            .await
            .context("failed to launch vm worker")?
    };

    if opt.restore_snapshot.is_some() {
        tracing::info!("restoring VM from snapshot");
    }

    if !opt.paused {
        vm_rpc.call(VmRpc::Resume, ()).await?;
    }

    let paravisor_diag = Arc::new(diag_client::DiagClient::from_dialer(
        driver.clone(),
        DiagDialer {
            driver: driver.clone(),
            vm_rpc: vm_rpc.clone(),
            openhcl_vtl: if opt.vtl2 {
                DeviceVtl::Vtl2
            } else {
                DeviceVtl::Vtl0
            },
        },
    ));

    let diag_inspector = DiagInspector::new(driver.clone(), paravisor_diag.clone());

    // Create channels between the REPL and VmController.
    let (vm_controller_send, vm_controller_recv) = mesh::channel();
    let (vm_controller_event_send, vm_controller_event_recv) = mesh::channel();

    let has_vtl2 = resources.vtl2_settings.is_some();

    // Build the VmController with exclusive resources.
    let controller = vm_controller::VmController {
        mesh: mesh_slot.take().unwrap(),
        vm_worker,
        vnc_worker,
        gdb_worker,
        diag_inspector: Some(diag_inspector),
        vtl2_settings: resources.vtl2_settings,
        ged_rpc: resources.ged_rpc.clone(),
        vm_rpc: vm_rpc.clone(),
        paravisor_diag: Some(paravisor_diag),
        igvm_path: opt.igvm.clone(),
        memory_backing_file: opt.memory_backing_file().cloned(),
        memory: opt.memory_size(),
        processors: opt.processors,
        log_file: opt.log_file.clone(),
    };

    // Spawn the VmController as a task.
    let controller_task = driver.spawn(
        "vm-controller",
        controller.run(vm_controller_recv, vm_controller_event_send, notify_recv),
    );

    // Run the REPL with shareable resources.
    let repl_result = repl::run_repl(
        driver,
        repl::ReplResources {
            vm_rpc,
            vm_controller: vm_controller_send,
            vm_controller_events: vm_controller_event_recv,
            scsi_rpc: resources.scsi_rpc,
            nvme_vtl2_rpc: resources.nvme_vtl2_rpc,
            shutdown_ic: resources.shutdown_ic,
            kvp_ic: resources.kvp_ic,
            console_in: resources.console_in,
            has_vtl2,
        },
    )
    .await;

    // Wait for the controller task to finish (it stops the VM worker and
    // shuts down the mesh).
    controller_task.await;

    repl_result
}

struct DiagDialer {
    driver: DefaultDriver,
    vm_rpc: mesh::Sender<VmRpc>,
    openhcl_vtl: DeviceVtl,
}

impl mesh_rpc::client::Dial for DiagDialer {
    type Stream = PolledSocket<unix_socket::UnixStream>;

    async fn dial(&mut self) -> io::Result<Self::Stream> {
        let service_id = new_hvsock_service_id(1);
        let socket = self
            .vm_rpc
            .call_failable(
                VmRpc::ConnectHvsock,
                (
                    CancelContext::new().with_timeout(Duration::from_secs(2)),
                    service_id,
                    self.openhcl_vtl,
                ),
            )
            .await
            .map_err(io::Error::other)?;

        PolledSocket::new(&self.driver, socket)
    }
}

/// An object that implements [`InspectMut`] by sending an inspect request over
/// TTRPC to the guest (typically the paravisor running in VTL2), then stitching
/// the response back into the inspect tree.
///
/// This also caches the TTRPC connection to the guest so that only the first
/// inspect request has to wait for the connection to be established.
pub(crate) struct DiagInspector(DiagInspectorInner);

enum DiagInspectorInner {
    NotStarted(DefaultDriver, Arc<diag_client::DiagClient>),
    Started {
        send: mesh::Sender<inspect::Deferred>,
        _task: Task<()>,
    },
    Invalid,
}

impl DiagInspector {
    pub fn new(driver: DefaultDriver, diag_client: Arc<diag_client::DiagClient>) -> Self {
        Self(DiagInspectorInner::NotStarted(driver, diag_client))
    }

    fn start(&mut self) -> &mesh::Sender<inspect::Deferred> {
        loop {
            match self.0 {
                DiagInspectorInner::NotStarted { .. } => {
                    let DiagInspectorInner::NotStarted(driver, client) =
                        std::mem::replace(&mut self.0, DiagInspectorInner::Invalid)
                    else {
                        unreachable!()
                    };
                    let (send, recv) = mesh::channel();
                    let task = driver.clone().spawn("diag-inspect", async move {
                        Self::run(&client, recv).await
                    });

                    self.0 = DiagInspectorInner::Started { send, _task: task };
                }
                DiagInspectorInner::Started { ref send, .. } => break send,
                DiagInspectorInner::Invalid => unreachable!(),
            }
        }
    }

    async fn run(
        diag_client: &diag_client::DiagClient,
        mut recv: mesh::Receiver<inspect::Deferred>,
    ) {
        while let Some(deferred) = recv.next().await {
            let info = deferred.external_request();
            let result = match info.request_type {
                inspect::ExternalRequestType::Inspect { depth } => {
                    if depth == 0 {
                        Ok(inspect::Node::Unevaluated)
                    } else {
                        // TODO: Support taking timeouts from the command line
                        diag_client
                            .inspect(info.path, Some(depth - 1), Some(Duration::from_secs(1)))
                            .await
                    }
                }
                inspect::ExternalRequestType::Update { value } => {
                    (diag_client.update(info.path, value).await).map(inspect::Node::Value)
                }
            };
            deferred.complete_external(
                result.unwrap_or_else(|err| {
                    inspect::Node::Failed(inspect::Error::Mesh(format!("{err:#}")))
                }),
                inspect::SensitivityLevel::Unspecified,
            )
        }
    }
}

impl InspectMut for DiagInspector {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.start().send(req.defer());
    }
}
