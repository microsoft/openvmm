// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the interactive control process and the entry point
//! for the worker process.

#![expect(missing_docs)]
#![cfg_attr(not(test), forbid(unsafe_code))]

mod cli_args;
mod crash_dump;
mod kvp;
mod meshworker;
mod serial_io;
mod storage_builder;
mod tracing_init;
mod ttrpc;

// `pub` so that the missing_docs warning fires for options without
// documentation.
pub use cli_args::Options;
use console_relay::ConsoleLaunchOptions;

use crate::cli_args::SecureBootTemplateCli;
use anyhow::Context;
use anyhow::bail;
use chipset_resources::battery::HostBatteryUpdate;
use clap::CommandFactory;
use clap::FromArgMatches;
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
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::StreamExt;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use futures_concurrency::stream::Merge;
use gdma_resources::GdmaDeviceHandle;
use gdma_resources::VportDefinition;
use get_resources::ged::GuestServicingFlags;
use guid::Guid;
use input_core::MultiplexedInputHandle;
use inspect::InspectMut;
use inspect::InspectionBuilder;
use io::Read;
use mesh::CancelContext;
use mesh::CellUpdater;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use mesh_worker::WorkerEvent;
use mesh_worker::WorkerHandle;
use meshworker::VmmMesh;
use net_backend_resources::mac_address::MacAddress;
use openvmm_defs::config::Config;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_AARCH64;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_AARCH64_WITH_VTL2;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_X86;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_X86_WITH_VTL2;
use openvmm_defs::config::DEFAULT_PCAT_BOOT_ORDER;
use openvmm_defs::config::DEFAULT_PCIE_ECAM_BASE;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::EfiDiagnosticsLogLevelType;
use openvmm_defs::config::HypervisorConfig;
use openvmm_defs::config::LateMapVtl0MemoryPolicy;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::MemoryConfig;
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
use openvmm_defs::rpc::PulseSaveRestoreError;
use openvmm_defs::rpc::VmRpc;
use openvmm_defs::worker::VM_WORKER;
use openvmm_defs::worker::VmWorkerParameters;
use openvmm_helpers::disk::create_disk_type;
use openvmm_helpers::disk::open_disk_type;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::pipe::PolledPipe;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use serial_16550_resources::ComPort;
use serial_core::resources::DisconnectedSerialBackendHandle;
use serial_io::SerialIo;
use sparse_mmap::alloc_shared_memory;
use std::cell::RefCell;
use std::fmt::Write as _;
use std::future::pending;
use std::io;
#[cfg(unix)]
use std::io::IsTerminal;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use storvsp_resources::ScsiControllerRequest;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use tpm_resources::TpmDeviceHandle;
use tpm_resources::TpmRegisterLayout;
use tracing_helpers::AnyhowValueExt;
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

    let exit_code = match do_main() {
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
    ged_rpc: Option<mesh::Sender<get_resources::ged::GuestEmulationRequest>>,
    vtl2_settings: Option<vtl2_settings_proto::Vtl2Settings>,
    #[cfg(windows)]
    switch_ports: Vec<vmswitch::kernel::SwitchPort>,
}

impl VmResources {
    /// Modify the cached VTL2 settings and send them to OpenHCL via the GED.
    ///
    /// This follows the same pattern as petri's `modify_vtl2_settings`: the cache
    /// is modified locally, then the entire settings are sent to OpenHCL.
    async fn modify_vtl2_settings(
        &mut self,
        f: impl FnOnce(&mut vtl2_settings_proto::Vtl2Settings),
    ) -> anyhow::Result<()> {
        let mut settings_copy = self
            .vtl2_settings
            .clone()
            .context("vtl2 settings not configured")?;

        f(&mut settings_copy);

        let ged_rpc = self.ged_rpc.as_ref().context("no GED configured")?;

        ged_rpc
            .call_failable(
                get_resources::ged::GuestEmulationRequest::ModifyVtl2Settings,
                prost::Message::encode_to_vec(&settings_copy),
            )
            .await?;

        // Settings successfully applied, update our cache
        self.vtl2_settings = Some(settings_copy);
        Ok(())
    }
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

    // TODO: unify virtio serial handling and remove this.
    let setup_serial_virtio = |name, cli_cfg, device| -> anyhow::Result<_> {
        Ok(match cli_cfg {
            SerialConfigCli::Console => {
                if console_state.borrow().is_some() {
                    bail!("console already set");
                }
                let mut io = SerialIo::new().context("creating serial IO")?;
                io.spawn_copy_out(name, term::raw_stdout());
                *console_state.borrow_mut() = Some(ConsoleState {
                    device,
                    input: Box::new(PolledPipe::new(&serial_driver, io.input.unwrap())?),
                });
                Some(io.config)
            }
            SerialConfigCli::Stderr => {
                let mut io = SerialIo::new().context("creating serial IO")?;
                io.spawn_copy_out(name, term::raw_stderr());
                // Ensure there is no input so that the serial devices don't see
                // EOF and think the port is disconnected.
                io.config.input = None;
                Some(io.config)
            }
            SerialConfigCli::File(path) => {
                let mut io = SerialIo::new().context("creating serial IO")?;
                let file = fs_err::File::create(path).context("failed to create file")?;
                io.spawn_copy_out(name, file);
                // Ensure there is no input so that the serial devices don't see
                // EOF and think the port is disconnected.
                io.config.input = None;
                Some(io.config)
            }
            SerialConfigCli::None => None,
            SerialConfigCli::Pipe(path) => {
                let mut io = SerialIo::new().context("creating serial IO")?;
                io.spawn_copy_listener(serial_driver.clone(), name, &path)
                    .with_context(|| format!("listening on pipe {}", path.display()))?
                    .detach();
                Some(io.config)
            }
            SerialConfigCli::Tcp(_addr) => anyhow::bail!("TCP virtio serial not supported"),
            SerialConfigCli::NewConsole(app, window_title) => {
                let path = console_relay::random_console_path();

                let mut io = SerialIo::new().context("creating serial IO")?;
                io.spawn_copy_listener(serial_driver.clone(), name, &path)
                    .with_context(|| format!("listening on pipe {}", path.display()))?
                    .detach();

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
                Some(io.config)
            }
        })
    };

    let virtio_console = opt.virtio_console || opt.virtio_console_pci;
    let mut vmbus_devices = Vec::new();

    let serial0_cfg = setup_serial(
        "com1",
        opt.com1.clone().unwrap_or({
            if !virtio_console {
                SerialConfigCli::Console
            } else {
                SerialConfigCli::None
            }
        }),
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
    let virtio_serial_cfg = setup_serial_virtio(
        "virtio_serial",
        opt.virtio_serial.clone().unwrap_or({
            if virtio_console {
                SerialConfigCli::Console
            } else {
                SerialConfigCli::None
            }
        }),
        if opt.virtio_console_pci {
            "hvc1"
        } else {
            "hvc0"
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

    let mut resources = VmResources::default();
    let mut console_str = "";
    if let Some(ConsoleState { device, input }) = console_state.into_inner() {
        resources.console_in = Some(input);
        console_str = device;
    }

    if opt.shared_memory {
        tracing::warn!("--shared-memory/-M flag has no effect and will be removed");
    }

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

        storage.add(
            vtl,
            underhill,
            storage_builder::DiskLocation::Scsi(None),
            kind,
            is_dvd,
            read_only,
        )?;
    }

    for &cli_args::IdeDiskCli {
        ref kind,
        read_only,
        channel,
        device,
        is_dvd,
    } in &opt.ide
    {
        storage.add(
            DeviceVtl::Vtl0,
            None,
            storage_builder::DiskLocation::Ide(channel, device),
            kind,
            is_dvd,
            read_only,
        )?;
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
        storage.add(
            vtl,
            underhill,
            storage_builder::DiskLocation::Nvme(None, pcie_port.clone()),
            kind,
            is_dvd,
            read_only,
        )?;
    }

    let floppy_disks: Vec<_> = opt
        .floppy
        .iter()
        .map(|disk| -> anyhow::Result<_> {
            let &cli_args::FloppyDiskCli {
                ref kind,
                read_only,
            } = disk;
            Ok(FloppyDiskConfig {
                disk_type: disk_open(kind, read_only)?,
                read_only,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut mana_nics = [(); 3].map(|()| None);
    let mut underhill_nics = Vec::new();
    let mut vpci_devices = Vec::new();

    let mut nic_index = 0;
    for cli_cfg in &opt.net {
        let vport = parse_endpoint(cli_cfg, &mut nic_index, &mut resources)?;
        if cli_cfg.underhill {
            if !opt.no_alias_map {
                anyhow::bail!("must specify --no-alias-map to offer NICs to VTL2");
            }
            let mana = mana_nics[openhcl_vtl as usize].get_or_insert_with(|| {
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
                endpoint: EndpointConfigCli::Consomme { cidr: None },
                max_queues: None,
                underhill: false,
            },
            &mut nic_index,
            &mut resources,
        )?;
        vmbus_devices.push(nic_config.into_netvsp_handle());
    }

    if opt.mcr {
        tracing::info!("Instantiating MCR controller");

        // Arbitrary but constant instance ID to be consistent across boots.
        const MCR_INSTANCE_ID: Guid = guid::guid!("07effd8f-7501-426c-a947-d8345f39113d");

        vpci_devices.push(VpciDeviceConfig {
            vtl: DeviceVtl::Vtl0,
            instance_id: MCR_INSTANCE_ID,
            resource: mcr_resources::McrControllerHandle {
                instance_id: MCR_INSTANCE_ID,
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
        mana_nics[vport.vtl as usize]
            .get_or_insert_with(|| (Guid::new_random(), GdmaDeviceHandle { vports: Vec::new() }))
            .1
            .vports
            .push(VportDefinition {
                mac_address: vport.mac_address,
                endpoint: vport.endpoint,
            });
    }

    vpci_devices.extend(mana_nics.into_iter().enumerate().filter_map(|(vtl, nic)| {
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
    }));

    let pcie_switches = build_switch_list(&opt.pcie_switch);

    let pcie_root_complexes = opt
        .pcie_root_complex
        .iter()
        .enumerate()
        .map(|(i, cli)| {
            let ports = opt
                .pcie_root_port
                .iter()
                .filter(|port_cli| port_cli.root_complex_name == cli.name)
                .map(|port_cli| PcieRootPortConfig {
                    name: port_cli.name.clone(),
                    hotplug: port_cli.hotplug,
                })
                .collect();

            PcieRootComplexConfig {
                index: i as u32,
                name: cli.name.clone(),
                segment: cli.segment,
                start_bus: cli.start_bus,
                end_bus: cli.end_bus,
                low_mmio_size: cli.low_mmio,
                high_mmio_size: cli.high_mmio,
                ports,
            }
        })
        .collect();

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
        let vram = alloc_shared_memory(FRAMEBUFFER_SIZE)?;
        let (fb, fba) =
            framebuffer::framebuffer(vram, FRAMEBUFFER_SIZE, 0).context("creating framebuffer")?;
        resources.framebuffer_access = Some(fba);
        Some(fb)
    } else {
        None
    };

    let is_arm = cfg!(guest_arch = "aarch64");
    let is_x86 = cfg!(guest_arch = "x86_64");

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
        if is_x86 {
            MachineArch::X86_64
        } else {
            MachineArch::Aarch64
        },
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
    } = chipset
        .build()
        .context("failed to build chipset configuration")?;

    if let Some(path) = &opt.igvm {
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
        if !is_x86 {
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
        if with_hv {
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
        };
    }

    let mut vmgs = Some(if let Some(VmgsCli { kind, provision }) = &opt.vmgs {
        let disk = VmgsDisk {
            disk: disk_open(kind, false).context("failed to open vmgs disk")?,
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
                storage_controllers: storage.build_underhill(),
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
            Some(template) => {
                if is_x86 {
                    match template {
                        SecureBootTemplateCli::Windows => {
                            hyperv_secure_boot_templates::x64::microsoft_windows()
                        }
                        SecureBootTemplateCli::UefiCa => {
                            hyperv_secure_boot_templates::x64::microsoft_uefi_ca()
                        }
                    }
                } else if is_arm {
                    match template {
                        SecureBootTemplateCli::Windows => {
                            hyperv_secure_boot_templates::aarch64::microsoft_windows()
                        }
                        SecureBootTemplateCli::UefiCa => {
                            hyperv_secure_boot_templates::aarch64::microsoft_uefi_ca()
                        }
                    }
                } else {
                    anyhow::bail!("no secure boot template for current guest_arch")
                }
            }
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

    let vtl0_vsock_listener = vsock_listener(opt.vsock_path.as_deref())?;
    let vtl2_vsock_listener = vsock_listener(opt.vtl2_vsock_path.as_deref())?;

    // If VTL2 is enabled, and we are not in VTL2 self allocate mode, provide an
    // mmio gap for VTL2.
    let mmio_gaps = if opt.vtl2
        && !matches!(
            opt.igvm_vtl2_relocation_type,
            Vtl2BaseAddressType::Vtl2Allocate { .. },
        ) {
        if is_x86 {
            DEFAULT_MMIO_GAPS_X86_WITH_VTL2.into()
        } else {
            DEFAULT_MMIO_GAPS_AARCH64_WITH_VTL2.into()
        }
    } else if is_x86 {
        DEFAULT_MMIO_GAPS_X86.into()
    } else {
        DEFAULT_MMIO_GAPS_AARCH64.into()
    };

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
        add_virtio_device(
            VirtioBusCli::Auto,
            virtio_resources::net::VirtioNetHandle {
                max_queues: vport.max_queues,
                mac_address: vport.mac_address,
                endpoint: vport.endpoint,
            }
            .into_resource(),
        );
    }

    for args in &opt.virtio_fs {
        add_virtio_device(
            opt.virtio_fs_bus,
            virtio_resources::fs::VirtioFsHandle {
                tag: args.tag.clone(),
                fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                    root_path: args.path.clone(),
                    mount_options: args.options.clone(),
                },
            }
            .into_resource(),
        );
    }

    for args in &opt.virtio_fs_shmem {
        add_virtio_device(
            opt.virtio_fs_bus,
            virtio_resources::fs::VirtioFsHandle {
                tag: args.tag.clone(),
                fs: virtio_resources::fs::VirtioFsBackend::SectionFs {
                    root_path: args.path.clone(),
                },
            }
            .into_resource(),
        );
    }

    for args in &opt.virtio_9p {
        add_virtio_device(
            VirtioBusCli::Auto,
            virtio_resources::p9::VirtioPlan9Handle {
                tag: args.tag.clone(),
                root_path: args.path.clone(),
                debug: opt.virtio_9p_debug,
            }
            .into_resource(),
        );
    }

    if let Some(path) = &opt.virtio_pmem {
        add_virtio_device(
            VirtioBusCli::Auto,
            virtio_resources::pmem::VirtioPmemHandle { path: path.clone() }.into_resource(),
        );
    }

    let mut cfg = Config {
        chipset,
        load_mode,
        floppy_disks,
        pcie_root_complexes,
        pcie_devices: Vec::new(),
        pcie_switches,
        vpci_devices,
        ide_disks: Vec::new(),
        memory: MemoryConfig {
            mem_size: opt.memory,
            mmio_gaps,
            prefetch_memory: opt.prefetch,
            pcie_ecam_base: DEFAULT_PCIE_ECAM_BASE,
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
            user_mode_hv_enlightenments: opt.no_enlightenments,
            user_mode_apic: opt.user_mode_apic,
        },
        #[cfg(windows)]
        kernel_vmnics,
        input: mesh::Receiver::new(),
        framebuffer,
        vga_firmware,
        vtl2_gfx: opt.vtl2_gfx,
        virtio_console_pci: opt.virtio_console_pci,
        virtio_serial: virtio_serial_cfg,
        virtio_devices,
        vmbus: with_hv.then_some(VmbusConfig {
            vsock_listener: vtl0_vsock_listener,
            vsock_path: opt.vsock_path.clone(),
            vtl2_redirect: opt.vmbus_redirect,
            vmbus_max_version: opt.vmbus_max_version,
            #[cfg(windows)]
            vmbusproxy_handle,
        }),
        vtl2_vmbus: (with_hv && opt.vtl2).then_some(VmbusConfig {
            vsock_listener: vtl2_vsock_listener,
            vsock_path: opt.vtl2_vsock_path.clone(),
            ..Default::default()
        }),
        vmbus_devices,
        chipset_devices,
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
fn openvmm_terminal_app() -> Option<PathBuf> {
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
        EndpointConfigCli::Consomme { cidr } => {
            net_backend_resources::consomme::ConsommeHandle { cidr: cidr.clone() }.into_resource()
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
            net_backend_resources::tap::TapHandle { name: name.clone() }.into_resource()
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
    })
}

#[derive(Debug)]
struct NicConfig {
    vtl: DeviceVtl,
    instance_id: Guid,
    mac_address: MacAddress,
    endpoint: Resource<NetEndpointHandleKind>,
    max_queues: Option<u16>,
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

fn disk_open(disk_cli: &DiskCliKind, read_only: bool) -> anyhow::Result<Resource<DiskHandleKind>> {
    let mut layers = Vec::new();
    disk_open_inner(disk_cli, read_only, &mut layers)?;
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

fn disk_open_inner(
    disk_cli: &DiskCliKind,
    read_only: bool,
    layers: &mut Vec<LayerOrDisk>,
) -> anyhow::Result<()> {
    fn layer<T: IntoResource<DiskLayerHandleKind>>(layer: T) -> LayerOrDisk {
        LayerOrDisk::Layer(layer.into_resource().into())
    }
    fn disk<T: IntoResource<DiskHandleKind>>(disk: T) -> LayerOrDisk {
        LayerOrDisk::Disk(disk.into_resource())
    }
    match disk_cli {
        &DiskCliKind::Memory(len) => {
            layers.push(layer(RamDiskLayerHandle { len: Some(len) }));
        }
        DiskCliKind::File {
            path,
            create_with_len,
        } => layers.push(LayerOrDisk::Disk(if let Some(size) = create_with_len {
            create_disk_type(path, *size)
                .with_context(|| format!("failed to create {}", path.display()))?
        } else {
            open_disk_type(path, read_only)
                .with_context(|| format!("failed to open {}", path.display()))?
        })),
        DiskCliKind::Blob { kind, url } => {
            layers.push(disk(disk_backend_resources::BlobDiskHandle {
                url: url.to_owned(),
                format: match kind {
                    cli_args::BlobKind::Flat => disk_backend_resources::BlobDiskFormat::Flat,
                    cli_args::BlobKind::Vhd1 => disk_backend_resources::BlobDiskFormat::FixedVhd1,
                },
            }))
        }
        DiskCliKind::MemoryDiff(inner) => {
            layers.push(layer(RamDiskLayerHandle { len: None }));
            disk_open_inner(inner, true, layers)?;
        }
        DiskCliKind::PersistentReservationsWrapper(inner) => layers.push(disk(
            disk_backend_resources::DiskWithReservationsHandle(disk_open(inner, read_only)?),
        )),
        DiskCliKind::DelayDiskWrapper {
            delay_ms,
            disk: inner,
        } => layers.push(disk(DelayDiskHandle {
            delay: CellUpdater::new(Duration::from_millis(*delay_ms)).cell(),
            disk: disk_open(inner, read_only)?,
        })),
        DiskCliKind::Crypt {
            disk: inner,
            cipher,
            key_file,
        } => layers.push(disk(disk_crypt_resources::DiskCryptHandle {
            disk: disk_open(inner, read_only)?,
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
            disk_open_inner(disk, true, layers)?;
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
            disk_open_inner(disk, read_only, layers)?;
        }
    }
    Ok(())
}

fn do_main() -> anyhow::Result<()> {
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

    DefaultPool::run_with(async |driver| {
        let mesh = VmmMesh::new(&driver, opt.single_process)?;
        let result = run_control(&driver, &mesh, opt).await;
        mesh.shutdown().await;
        result
    })
}

fn maybe_with_radix_u64(s: &str) -> Result<u64, String> {
    let (radix, prefix_len) = if s.starts_with("0x") || s.starts_with("0X") {
        (16, 2)
    } else if s.starts_with("0o") || s.starts_with("0O") {
        (8, 2)
    } else if s.starts_with("0b") || s.starts_with("0B") {
        (2, 2)
    } else {
        (10, 0)
    };

    u64::from_str_radix(&s[prefix_len..], radix).map_err(|e| format!("{e}"))
}

#[derive(Parser)]
#[clap(
    name = "openvmm",
    disable_help_flag = true,
    disable_version_flag = true,
    no_binary_name = true,
    help_template("{subcommands}")
)]
enum InteractiveCommand {
    /// Restart the VM worker (experimental).
    ///
    /// This restarts the VM worker while preserving state.
    #[clap(visible_alias = "R")]
    Restart,

    /// Inject an NMI.
    #[clap(visible_alias = "n")]
    Nmi,

    /// Pause the VM.
    #[clap(visible_alias = "p")]
    Pause,

    /// Resume the VM.
    #[clap(visible_alias = "r")]
    Resume,

    /// Do a pulsed save restore (pause, save, reset, restore, resume) to the VM.
    #[clap(visible_alias = "psr")]
    PulseSaveRestore,

    /// Schedule a pulsed save restore (pause, save, reset, restore, resume) to the VM.
    #[clap(visible_alias = "spsr")]
    SchedulePulseSaveRestore {
        /// The interval between pulse save restore operations in seconds.
        /// None or 0 means any previous scheduled pulse save restores will be cleared.
        interval: Option<u64>,
    },

    /// Hot add a disk.
    #[clap(visible_alias = "d")]
    AddDisk {
        #[clap(long = "ro")]
        read_only: bool,
        #[clap(long = "dvd")]
        is_dvd: bool,
        #[clap(long, default_value_t)]
        target: u8,
        #[clap(long, default_value_t)]
        path: u8,
        #[clap(long, default_value_t)]
        lun: u8,
        #[clap(long)]
        ram: Option<u64>,
        file_path: Option<PathBuf>,
    },

    /// Hot remove a disk.
    #[clap(visible_alias = "D")]
    RmDisk {
        #[clap(long)]
        target: u8,
        #[clap(long)]
        path: u8,
        #[clap(long)]
        lun: u8,
    },

    /// Manage VTL2 settings (storage controllers, NICs exposed to VTL0).
    #[clap(subcommand)]
    Vtl2Settings(Vtl2SettingsCommand),

    /// Inspect program state.
    #[clap(visible_alias = "x")]
    Inspect {
        /// Enumerate state recursively.
        #[clap(short, long)]
        recursive: bool,
        /// The recursive depth limit.
        #[clap(short, long, requires("recursive"))]
        limit: Option<usize>,
        /// Target the paravisor.
        #[clap(short = 'v', long)]
        paravisor: bool,
        /// The element path to inspect.
        element: Option<String>,
        /// Update the path with a new value.
        #[clap(short, long, conflicts_with("recursive"))]
        update: Option<String>,
    },

    /// Restart the VNC worker.
    #[clap(visible_alias = "V")]
    RestartVnc,

    /// Start an hvsocket terminal window.
    #[clap(visible_alias = "v")]
    Hvsock {
        /// the terminal emulator to run (defaults to conhost.exe or xterm)
        #[clap(short, long)]
        term: Option<PathBuf>,
        /// the vsock port to connect to
        port: u32,
    },

    /// Quit the program.
    #[clap(visible_alias = "q")]
    Quit,

    /// Write input to the VM console.
    ///
    /// This will write each input parameter to the console's associated serial
    /// port, separated by spaces.
    #[clap(visible_alias = "i")]
    Input { data: Vec<String> },

    /// Switch to input mode.
    ///
    /// Once in input mode, Ctrl-Q returns to command mode.
    #[clap(visible_alias = "I")]
    InputMode,

    /// Reset the VM.
    Reset,

    /// Send a request to the VM to shut it down.
    Shutdown {
        /// Reboot the VM instead of powering it off.
        #[clap(long, short = 'r')]
        reboot: bool,
        /// Hibernate the VM instead of powering it off.
        #[clap(long, short = 'h', conflicts_with = "reboot")]
        hibernate: bool,
        /// Tell the guest to force the power state transition.
        #[clap(long, short = 'f')]
        force: bool,
    },

    /// Clears the current halt condition, resuming the VPs if the VM is
    /// running.
    #[clap(visible_alias = "ch")]
    ClearHalt,

    /// Update the image in VTL2.
    ServiceVtl2 {
        /// Just restart the user-mode paravisor process, not the full
        /// firmware.
        #[clap(long, short = 'u')]
        user_mode_only: bool,
        /// The path to the new IGVM file. If missing, use the originally
        /// configured path.
        #[clap(long, conflicts_with("user_mode_only"))]
        igvm: Option<PathBuf>,
        /// Enable keepalive when servicing VTL2 devices.
        /// Default is `true`.
        #[clap(long, short = 'n', default_missing_value = "true")]
        nvme_keepalive: bool,
        /// Enable keepalive when servicing VTL2 devices.
        /// Default is `false`.
        #[clap(long)]
        mana_keepalive: bool,
    },

    /// Read guest memory
    ReadMemory {
        /// Guest physical address to start at.
        #[clap(value_parser=maybe_with_radix_u64)]
        gpa: u64,
        /// How many bytes to dump.
        #[clap(value_parser=maybe_with_radix_u64)]
        size: u64,
        /// File to save the data to. If omitted,
        /// the data will be presented as a hex dump.
        #[clap(long, short = 'f')]
        file: Option<PathBuf>,
    },

    /// Write guest memory
    WriteMemory {
        /// Guest physical address to start at
        #[clap(value_parser=maybe_with_radix_u64)]
        gpa: u64,
        /// Hex string encoding data, with no `0x` radix.
        /// If omitted, the source file must be specified.
        hex: Option<String>,
        /// File to write the data from.
        #[clap(long, short = 'f')]
        file: Option<PathBuf>,
    },

    /// Inject an artificial panic into OpenVMM
    Panic,

    /// Use KVP to interact with the guest.
    Kvp(kvp::KvpCommand),
}

/// Subcommands for managing VTL2 settings.
#[derive(clap::Subcommand)]
enum Vtl2SettingsCommand {
    /// Show the current VTL2 settings.
    Show,

    /// Add a SCSI disk to VTL0 backed by a VTL2 storage device.
    ///
    /// The backing device can be either a VTL2 NVMe namespace or a VTL2 SCSI disk.
    AddScsiDisk {
        /// The VTL0 SCSI controller instance ID (GUID). Defaults to the standard
        /// OpenVMM VTL0 SCSI instance.
        #[clap(long)]
        controller: Option<String>,
        /// The SCSI LUN to expose to VTL0.
        #[clap(long)]
        lun: u32,
        /// The backing VTL2 NVMe namespace ID.
        #[clap(
            long,
            conflicts_with = "backing_scsi_lun",
            required_unless_present = "backing_scsi_lun"
        )]
        backing_nvme_nsid: Option<u32>,
        /// The backing VTL2 SCSI LUN.
        #[clap(
            long,
            conflicts_with = "backing_nvme_nsid",
            required_unless_present = "backing_nvme_nsid"
        )]
        backing_scsi_lun: Option<u32>,
    },

    /// Remove a SCSI disk from VTL0.
    RmScsiDisk {
        /// The SCSI controller instance ID (GUID). Defaults to the standard
        /// OpenVMM VTL0 SCSI instance.
        #[clap(long)]
        controller: Option<String>,
        /// The SCSI LUN to remove.
        #[clap(long)]
        lun: u32,
    },
}

struct CommandParser {
    app: clap::Command,
}

impl CommandParser {
    fn new() -> Self {
        // Update the help template for each subcommand.
        let mut app = InteractiveCommand::command();
        for sc in app.get_subcommands_mut() {
            *sc = sc
                .clone()
                .help_template("{about-with-newline}\n{usage-heading}\n    {usage}\n\n{all-args}");
        }
        Self { app }
    }

    fn parse(&mut self, line: &str) -> clap::error::Result<InteractiveCommand> {
        let args = shell_words::split(line)
            .map_err(|err| self.app.error(clap::error::ErrorKind::ValueValidation, err))?;
        let matches = self.app.try_get_matches_from_mut(args)?;
        InteractiveCommand::from_arg_matches(&matches).map_err(|err| err.format(&mut self.app))
    }
}

fn new_hvsock_service_id(port: u32) -> Guid {
    // This GUID is an embedding of the AF_VSOCK port into an
    // AF_HYPERV service ID.
    Guid {
        data1: port,
        .."00000000-facb-11e6-bd58-64006a7986d3".parse().unwrap()
    }
}

async fn run_control(driver: &DefaultDriver, mesh: &VmmMesh, opt: Options) -> anyhow::Result<()> {
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
    let mut vm_worker = {
        let vm_host = mesh.make_host("vm", opt.log_file.clone()).await?;

        let params = VmWorkerParameters {
            hypervisor: opt.hypervisor,
            cfg: vm_config,
            saved_state: None,
            rpc: rpc_recv,
            notify: notify_send,
        };
        vm_host
            .launch_worker(VM_WORKER, params)
            .await
            .context("failed to launch vm worker")?
    };

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

    let mut diag_inspector = DiagInspector::new(driver.clone(), paravisor_diag.clone());

    let (console_command_send, console_command_recv) = mesh::channel();
    let (inspect_completion_engine_send, inspect_completion_engine_recv) = mesh::channel();

    let mut console_in = resources.console_in.take();
    thread::Builder::new()
        .name("stdio-thread".to_string())
        .spawn(move || {
            // install panic hook to restore cooked terminal (linux)
            #[cfg(unix)]
            if io::stderr().is_terminal() {
                term::revert_terminal_on_panic()
            }

            let mut rl = rustyline::Editor::<
                interactive_console::OpenvmmRustylineEditor,
                rustyline::history::FileHistory,
            >::with_config(
                rustyline::Config::builder()
                    .completion_type(rustyline::CompletionType::List)
                    .build(),
            )
            .unwrap();

            rl.set_helper(Some(interactive_console::OpenvmmRustylineEditor {
                openvmm_inspect_req: Arc::new(inspect_completion_engine_send),
            }));

            let history_file = {
                const HISTORY_FILE: &str = ".openvmm_history";

                // using a `None` to kick off the `.or()` chain in order to make
                // it a bit easier to visually inspect the fallback chain.
                let history_folder = None
                    .or_else(dirs::state_dir)
                    .or_else(dirs::data_local_dir)
                    .map(|path| path.join("openvmm"));

                if let Some(history_folder) = history_folder {
                    if let Err(err) = std::fs::create_dir_all(&history_folder) {
                        tracing::warn!(
                            error = &err as &dyn std::error::Error,
                            "could not create directory: {}",
                            history_folder.display()
                        )
                    }

                    Some(history_folder.join(HISTORY_FILE))
                } else {
                    None
                }
            };

            if let Some(history_file) = &history_file {
                tracing::info!("restoring history from {}", history_file.display());
                if rl.load_history(history_file).is_err() {
                    tracing::info!("could not find existing {}", history_file.display());
                }
            }

            // Enable Ctrl-Backspace to delete the current word.
            rl.bind_sequence(
                rustyline::KeyEvent::new('\x08', rustyline::Modifiers::CTRL),
                rustyline::Cmd::Kill(rustyline::Movement::BackwardWord(1, rustyline::Word::Emacs)),
            );

            let mut parser = CommandParser::new();

            let mut stdin = io::stdin();
            loop {
                // Raw console text until Ctrl-Q.
                term::set_raw_console(true).expect("failed to set raw console mode");

                if let Some(input) = console_in.as_mut() {
                    let mut buf = [0; 32];
                    loop {
                        let n = stdin.read(&mut buf).unwrap();
                        let mut b = &buf[..n];
                        let stop = if let Some(ctrlq) = b.iter().position(|x| *x == 0x11) {
                            b = &b[..ctrlq];
                            true
                        } else {
                            false
                        };
                        block_on(input.as_mut().write_all(b)).expect("BUGBUG");
                        if stop {
                            break;
                        }
                    }
                }

                term::set_raw_console(false).expect("failed to set raw console mode");

                loop {
                    let line = rl.readline("openvmm> ");
                    if line.is_err() {
                        break;
                    }
                    let line = line.unwrap();
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Err(err) = rl.add_history_entry(&line) {
                        tracing::warn!(
                            err = &err as &dyn std::error::Error,
                            "error adding to .openvmm_history"
                        )
                    }

                    match parser.parse(trimmed) {
                        Ok(cmd) => match cmd {
                            InteractiveCommand::Input { data } => {
                                let mut data = data.join(" ");
                                data.push('\n');
                                if let Some(input) = console_in.as_mut() {
                                    block_on(input.write_all(data.as_bytes())).expect("BUGBUG");
                                }
                            }
                            InteractiveCommand::InputMode => break,
                            cmd => {
                                // Send the command to the main thread for processing.
                                let (processing_done_send, processing_done_recv) =
                                    mesh::oneshot::<()>();
                                console_command_send.send((cmd, processing_done_send));
                                let _ = block_on(processing_done_recv);
                            }
                        },
                        Err(err) => {
                            err.print().unwrap();
                        }
                    }

                    if let Some(history_file) = &history_file {
                        rl.append_history(history_file).unwrap();
                    }
                }
            }
        })
        .unwrap();

    let mut state_change_task = None::<Task<Result<StateChange, RpcError>>>;
    let mut pulse_save_restore_interval: Option<Duration> = None;
    let mut pending_shutdown = None;

    enum StateChange {
        Pause(bool),
        Resume(bool),
        Reset(Result<(), RemoteError>),
        PulseSaveRestore(Result<(), PulseSaveRestoreError>),
        ServiceVtl2(anyhow::Result<Duration>),
    }

    enum Event {
        Command((InteractiveCommand, mesh::OneshotSender<()>)),
        InspectRequestFromCompletionEngine(
            (InspectTarget, String, mesh::OneshotSender<inspect::Node>),
        ),
        Quit,
        Halt(vmm_core_defs::HaltReason),
        PulseSaveRestore,
        Worker(WorkerEvent),
        VncWorker(WorkerEvent),
        StateChange(Result<StateChange, RpcError>),
        ShutdownResult(Result<hyperv_ic_resources::shutdown::ShutdownResult, RpcError>),
    }

    let mut console_command_recv = console_command_recv
        .map(Event::Command)
        .chain(futures::stream::repeat_with(|| Event::Quit));

    let mut notify_recv = notify_recv.map(Event::Halt);

    let mut inspect_completion_engine_recv =
        inspect_completion_engine_recv.map(Event::InspectRequestFromCompletionEngine);

    let mut quit = false;
    loop {
        let event = {
            let pulse_save_restore = pin!(async {
                match pulse_save_restore_interval {
                    Some(wait) => {
                        PolledTimer::new(driver).sleep(wait).await;
                        Event::PulseSaveRestore
                    }
                    None => pending().await,
                }
            });

            let vm = (&mut vm_worker).map(Event::Worker);
            let vnc = futures::stream::iter(vnc_worker.as_mut())
                .flatten()
                .map(Event::VncWorker);
            let change = futures::stream::iter(state_change_task.as_mut().map(|x| x.into_stream()))
                .flatten()
                .map(Event::StateChange);
            let shutdown = pin!(async {
                if let Some(s) = &mut pending_shutdown {
                    Event::ShutdownResult(s.await)
                } else {
                    pending().await
                }
            });

            (
                &mut console_command_recv,
                &mut inspect_completion_engine_recv,
                &mut notify_recv,
                pulse_save_restore.into_stream(),
                vm,
                vnc,
                change,
                shutdown.into_stream(),
            )
                .merge()
                .next()
                .await
                .unwrap()
        };

        let (cmd, _processing_done_send) = match event {
            Event::Command(message) => message,
            Event::InspectRequestFromCompletionEngine((vtl, path, res)) => {
                let mut inspection =
                    InspectionBuilder::new(&path)
                        .depth(Some(1))
                        .inspect(inspect_obj(
                            vtl,
                            mesh,
                            &vm_worker,
                            vnc_worker.as_ref(),
                            gdb_worker.as_ref(),
                            &mut diag_inspector,
                        ));
                let _ = CancelContext::new()
                    .with_timeout(Duration::from_secs(1))
                    .until_cancelled(inspection.resolve())
                    .await;

                let node = inspection.results();
                res.send(node);
                continue;
            }
            Event::Quit => break,
            Event::Halt(reason) => {
                tracing::info!(?reason, "guest halted");
                continue;
            }
            Event::PulseSaveRestore => {
                vm_rpc.call(VmRpc::PulseSaveRestore, ()).await??;
                continue;
            }
            Event::Worker(event) => {
                match event {
                    WorkerEvent::Stopped => {
                        if quit {
                            tracing::info!("vm stopped");
                        } else {
                            tracing::error!("vm worker unexpectedly stopped");
                        }
                        break;
                    }
                    WorkerEvent::Failed(err) => {
                        tracing::error!(error = &err as &dyn std::error::Error, "vm worker failed");
                        break;
                    }
                    WorkerEvent::RestartFailed(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "vm worker restart failed"
                        );
                    }
                    WorkerEvent::Started => {
                        tracing::info!("vm worker restarted");
                    }
                }
                continue;
            }
            Event::VncWorker(event) => {
                match event {
                    WorkerEvent::Stopped => tracing::error!("vnc unexpectedly stopped"),
                    WorkerEvent::Failed(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "vnc worker failed"
                        );
                    }
                    WorkerEvent::RestartFailed(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "vnc worker restart failed"
                        );
                    }
                    WorkerEvent::Started => {
                        tracing::info!("vnc worker restarted");
                    }
                }
                continue;
            }
            Event::StateChange(r) => {
                match r {
                    Ok(sc) => match sc {
                        StateChange::Pause(success) => {
                            if success {
                                tracing::info!("pause complete");
                            } else {
                                tracing::warn!("already paused");
                            }
                        }
                        StateChange::Resume(success) => {
                            if success {
                                tracing::info!("resumed complete");
                            } else {
                                tracing::warn!("already running");
                            }
                        }
                        StateChange::Reset(r) => match r {
                            Ok(()) => tracing::info!("reset complete"),
                            Err(err) => tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "reset failed"
                            ),
                        },
                        StateChange::PulseSaveRestore(r) => match r {
                            Ok(()) => tracing::info!("pulse save/restore complete"),
                            Err(err) => tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "pulse save/restore failed"
                            ),
                        },
                        StateChange::ServiceVtl2(r) => match r {
                            Ok(dur) => {
                                tracing::info!(
                                    duration = dur.as_millis() as i64,
                                    "vtl2 servicing complete"
                                )
                            }
                            Err(err) => tracing::error!(
                                error = err.as_ref() as &dyn std::error::Error,
                                "vtl2 servicing failed"
                            ),
                        },
                    },
                    Err(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "communication failure during state change"
                        );
                    }
                }
                state_change_task = None;
                continue;
            }
            Event::ShutdownResult(r) => {
                match r {
                    Ok(r) => match r {
                        hyperv_ic_resources::shutdown::ShutdownResult::Ok => {
                            tracing::info!("shutdown initiated");
                        }
                        hyperv_ic_resources::shutdown::ShutdownResult::NotReady => {
                            tracing::error!("shutdown ic not ready");
                        }
                        hyperv_ic_resources::shutdown::ShutdownResult::AlreadyInProgress => {
                            tracing::error!("shutdown already in progress");
                        }
                        hyperv_ic_resources::shutdown::ShutdownResult::Failed(hr) => {
                            tracing::error!("shutdown failed with error code {hr:#x}");
                        }
                    },
                    Err(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "communication failure during shutdown"
                        );
                    }
                }
                pending_shutdown = None;
                continue;
            }
        };

        fn inspect_obj<'a>(
            target: InspectTarget,
            mesh: &'a VmmMesh,
            vm_worker: &'a WorkerHandle,
            vnc_worker: Option<&'a WorkerHandle>,
            gdb_worker: Option<&'a WorkerHandle>,
            diag_inspector: &'a mut DiagInspector,
        ) -> impl 'a + InspectMut {
            inspect::adhoc_mut(move |req| match target {
                InspectTarget::Host => {
                    let mut resp = req.respond();
                    resp.field("mesh", mesh)
                        .field("vm", vm_worker)
                        .field("vnc", vnc_worker)
                        .field("gdb", gdb_worker);
                }
                InspectTarget::Paravisor => {
                    diag_inspector.inspect_mut(req);
                }
            })
        }

        fn state_change<U: 'static + Send>(
            driver: impl Spawn,
            vm_rpc: &mesh::Sender<VmRpc>,
            state_change_task: &mut Option<Task<Result<StateChange, RpcError>>>,
            f: impl FnOnce(Rpc<(), U>) -> VmRpc,
            g: impl FnOnce(U) -> StateChange + 'static + Send,
        ) {
            if state_change_task.is_some() {
                tracing::error!("state change already in progress");
            } else {
                let rpc = vm_rpc.call(f, ());
                *state_change_task =
                    Some(driver.spawn("state-change", async move { Ok(g(rpc.await?)) }));
            }
        }

        match cmd {
            InteractiveCommand::Panic => {
                panic!("injected panic")
            }
            InteractiveCommand::Restart => {
                // create a new host process
                let vm_host = mesh.make_host("vm", opt.log_file.clone()).await?;

                vm_worker.restart(&vm_host);
            }
            InteractiveCommand::Pause => {
                state_change(
                    driver,
                    &vm_rpc,
                    &mut state_change_task,
                    VmRpc::Pause,
                    StateChange::Pause,
                );
            }
            InteractiveCommand::Resume => {
                state_change(
                    driver,
                    &vm_rpc,
                    &mut state_change_task,
                    VmRpc::Resume,
                    StateChange::Resume,
                );
            }
            InteractiveCommand::Reset => {
                state_change(
                    driver,
                    &vm_rpc,
                    &mut state_change_task,
                    VmRpc::Reset,
                    StateChange::Reset,
                );
            }
            InteractiveCommand::PulseSaveRestore => {
                state_change(
                    driver,
                    &vm_rpc,
                    &mut state_change_task,
                    VmRpc::PulseSaveRestore,
                    StateChange::PulseSaveRestore,
                );
            }
            InteractiveCommand::SchedulePulseSaveRestore { interval } => {
                pulse_save_restore_interval = match interval {
                    Some(seconds) if seconds != 0 => Some(Duration::from_secs(seconds)),
                    _ => {
                        // Treat None and 0 seconds as do not perform scheduled pulse save restores anymore.
                        None
                    }
                }
            }
            InteractiveCommand::Shutdown {
                reboot,
                hibernate,
                force,
            } => {
                if pending_shutdown.is_some() {
                    println!("shutdown already in progress");
                } else if let Some(ic) = &resources.shutdown_ic {
                    let params = hyperv_ic_resources::shutdown::ShutdownParams {
                        shutdown_type: if hibernate {
                            hyperv_ic_resources::shutdown::ShutdownType::Hibernate
                        } else if reboot {
                            hyperv_ic_resources::shutdown::ShutdownType::Reboot
                        } else {
                            hyperv_ic_resources::shutdown::ShutdownType::PowerOff
                        },
                        force,
                    };
                    pending_shutdown =
                        Some(ic.call(hyperv_ic_resources::shutdown::ShutdownRpc::Shutdown, params));
                } else {
                    println!("no shutdown ic configured");
                }
            }
            InteractiveCommand::Nmi => {
                let _ = vm_rpc.call(VmRpc::Nmi, 0).await;
            }
            InteractiveCommand::ClearHalt => {
                vm_rpc.call(VmRpc::ClearHalt, ()).await.ok();
            }
            InteractiveCommand::AddDisk {
                read_only,
                target,
                path,
                lun,
                ram,
                file_path,
                is_dvd,
            } => {
                let action = async {
                    let scsi = resources.scsi_rpc.as_ref().context("no scsi controller")?;
                    let disk_type = match ram {
                        None => {
                            let path = file_path.context("no filename passed")?;
                            open_disk_type(path.as_ref(), read_only)
                                .with_context(|| format!("failed to open {}", path.display()))?
                        }
                        Some(size) => {
                            Resource::new(disk_backend_resources::LayeredDiskHandle::single_layer(
                                RamDiskLayerHandle { len: Some(size) },
                            ))
                        }
                    };

                    let device = if is_dvd {
                        SimpleScsiDvdHandle {
                            media: Some(disk_type),
                            requests: None,
                        }
                        .into_resource()
                    } else {
                        SimpleScsiDiskHandle {
                            disk: disk_type,
                            read_only,
                            parameters: Default::default(),
                        }
                        .into_resource()
                    };

                    let cfg = ScsiDeviceAndPath {
                        path: ScsiPath { path, target, lun },
                        device,
                    };

                    scsi.call_failable(ScsiControllerRequest::AddDevice, cfg)
                        .await?;

                    anyhow::Result::<_>::Ok(())
                };

                if let Err(error) = action.await {
                    tracing::error!(error = error.as_error(), "error adding disk")
                }
            }
            InteractiveCommand::RmDisk { target, path, lun } => {
                let action = async {
                    let scsi = resources.scsi_rpc.as_ref().context("no scsi controller")?;
                    scsi.call_failable(
                        ScsiControllerRequest::RemoveDevice,
                        ScsiPath { target, path, lun },
                    )
                    .await?;
                    anyhow::Ok(())
                };

                if let Err(error) = action.await {
                    tracing::error!(error = error.as_error(), "error removing disk")
                }
            }
            InteractiveCommand::Vtl2Settings(cmd) => {
                if resources.vtl2_settings.is_none() {
                    eprintln!("error: no VTL2 settings (not running with VTL2?)");
                    continue;
                }
                let action = async {
                    match cmd {
                        Vtl2SettingsCommand::Show => {
                            let settings = resources.vtl2_settings.as_ref().unwrap();
                            println!("{:#?}", settings);
                        }
                        Vtl2SettingsCommand::AddScsiDisk {
                            controller,
                            lun,
                            backing_nvme_nsid,
                            backing_scsi_lun,
                        } => {
                            // Determine the backing device type and path
                            let (device_type, device_path, sub_device_path) = if let Some(nsid) =
                                backing_nvme_nsid
                            {
                                (
                                    vtl2_settings_proto::physical_device::DeviceType::Nvme,
                                    storage_builder::NVME_VTL2_INSTANCE_ID,
                                    nsid,
                                )
                            } else if let Some(scsi_lun) = backing_scsi_lun {
                                (
                                    vtl2_settings_proto::physical_device::DeviceType::Vscsi,
                                    storage_builder::SCSI_VTL2_INSTANCE_ID,
                                    scsi_lun,
                                )
                            } else {
                                anyhow::bail!(
                                    "must specify either --backing-nvme-nsid or --backing-scsi-lun"
                                );
                            };

                            // Default to the standard OpenVMM VTL0 SCSI instance
                            let controller_guid = controller
                                .map(|s| s.parse())
                                .transpose()
                                .context("invalid controller GUID")?
                                .unwrap_or(storage_builder::UNDERHILL_VTL0_SCSI_INSTANCE);

                            let mut not_found = false;
                            resources
                                .modify_vtl2_settings(|settings| {
                                    let dynamic = settings.dynamic.get_or_insert_with(Default::default);

                                    // Find the SCSI controller, bail out if not found (we can't create new controllers at runtime)
                                    let scsi_controller = dynamic
                                        .storage_controllers
                                        .iter_mut()
                                        .find(|c| {
                                            c.instance_id == controller_guid.to_string()
                                                && c.protocol
                                                    == vtl2_settings_proto::storage_controller::StorageProtocol::Scsi
                                                        as i32
                                        });

                                    let Some(scsi_controller) = scsi_controller else {
                                        not_found = true;
                                        return;
                                    };

                                    // Add the LUN backed by the VTL2 storage device. If the LUN exists already, UH will reject the settings
                                    scsi_controller.luns.push(vtl2_settings_proto::Lun {
                                        location: lun,
                                        device_id: Guid::new_random().to_string(),
                                        vendor_id: "OpenVMM".to_string(),
                                        product_id: "Disk".to_string(),
                                        product_revision_level: "1.0".to_string(),
                                        serial_number: "0".to_string(),
                                        model_number: "1".to_string(),
                                        physical_devices: Some(vtl2_settings_proto::PhysicalDevices {
                                            r#type: vtl2_settings_proto::physical_devices::BackingType::Single.into(),
                                            device: Some(vtl2_settings_proto::PhysicalDevice {
                                                device_type: device_type.into(),
                                                device_path: device_path.to_string(),
                                                sub_device_path,
                                            }),
                                            devices: Vec::new(),
                                        }),
                                        is_dvd: false,
                                        ..Default::default()
                                    });
                                })
                                .await?;

                            if not_found {
                                anyhow::bail!("SCSI controller {} not found", controller_guid);
                            }
                            let backing_desc = if backing_nvme_nsid.is_some() {
                                format!("nvme_nsid={}", sub_device_path)
                            } else {
                                format!("scsi_lun={}", sub_device_path)
                            };
                            println!(
                                "Added VTL0 SCSI disk: controller={}, lun={}, backing={}",
                                controller_guid, lun, backing_desc
                            );
                        }
                        Vtl2SettingsCommand::RmScsiDisk { controller, lun } => {
                            // Default to the standard OpenVMM VTL0 SCSI instance
                            let controller_guid = controller
                                .map(|s| s.parse())
                                .transpose()
                                .context("invalid controller GUID")?
                                .unwrap_or(storage_builder::UNDERHILL_VTL0_SCSI_INSTANCE);

                            resources
                                .modify_vtl2_settings(|settings| {
                                    let dynamic = settings.dynamic.as_mut();
                                    if let Some(dynamic) = dynamic {
                                        // Find the SCSI controller
                                        if let Some(scsi_controller) =
                                            dynamic.storage_controllers.iter_mut().find(|c| {
                                                c.instance_id == controller_guid.to_string()
                                                    && c.protocol
                                                        == vtl2_settings_proto::storage_controller::StorageProtocol::Scsi
                                                            as i32
                                            })
                                        {
                                            // Remove the LUN
                                            scsi_controller.luns.retain(|l| l.location != lun);
                                        }
                                    }
                                })
                                .await?;

                            println!(
                                "Removed VTL0 SCSI disk: controller={}, lun={}",
                                controller_guid, lun
                            );
                        }
                    }
                    anyhow::Ok(())
                };

                if let Err(error) = action.await {
                    eprintln!("error: {}", error);
                }
            }
            InteractiveCommand::Inspect {
                recursive,
                limit,
                paravisor,
                element,
                update,
            } => {
                let obj = inspect_obj(
                    if paravisor {
                        InspectTarget::Paravisor
                    } else {
                        InspectTarget::Host
                    },
                    mesh,
                    &vm_worker,
                    vnc_worker.as_ref(),
                    gdb_worker.as_ref(),
                    &mut diag_inspector,
                );

                if let Some(value) = update {
                    let Some(element) = element else {
                        anyhow::bail!("must provide element for update")
                    };

                    let value = async {
                        let update = inspect::update(&element, &value, obj);
                        let value = CancelContext::new()
                            .with_timeout(Duration::from_secs(1))
                            .until_cancelled(update)
                            .await??;
                        anyhow::Ok(value)
                    }
                    .await;
                    match value {
                        Ok(node) => println!("{:#}", node),
                        Err(err) => println!("error: {:#}", err),
                    }
                } else {
                    let element = element.unwrap_or_default();
                    let depth = if recursive { limit } else { Some(0) };
                    let node = async {
                        let mut inspection =
                            InspectionBuilder::new(&element).depth(depth).inspect(obj);
                        let _ = CancelContext::new()
                            .with_timeout(Duration::from_secs(1))
                            .until_cancelled(inspection.resolve())
                            .await;
                        inspection.results()
                    }
                    .await;

                    println!("{:#}", node);
                }
            }
            InteractiveCommand::RestartVnc => {
                if let Some(vnc) = &mut vnc_worker {
                    let action = async {
                        let vnc_host = mesh
                            .make_host("vnc", None)
                            .await
                            .context("spawning vnc process failed")?;

                        vnc.restart(&vnc_host);
                        anyhow::Result::<_>::Ok(())
                    };

                    if let Err(error) = action.await {
                        eprintln!("error: {}", error);
                    }
                } else {
                    eprintln!("ERROR: no VNC server running");
                }
            }
            InteractiveCommand::Hvsock { term, port } => {
                let vm_rpc = &vm_rpc;
                let action = async || {
                    let service_id = new_hvsock_service_id(port);
                    let socket = vm_rpc
                        .call_failable(
                            VmRpc::ConnectHvsock,
                            (
                                CancelContext::new().with_timeout(Duration::from_secs(2)),
                                service_id,
                                DeviceVtl::Vtl0,
                            ),
                        )
                        .await?;
                    let socket = PolledSocket::new(driver, socket)?;
                    let mut console = console_relay::Console::new(
                        driver.clone(),
                        term.or_else(openvmm_terminal_app).as_deref(),
                        Some(ConsoleLaunchOptions {
                            window_title: Some(format!("HVSock{} [OpenVMM]", port)),
                        }),
                    )?;
                    driver
                        .spawn("console-relay", async move { console.relay(socket).await })
                        .detach();
                    anyhow::Result::<_>::Ok(())
                };

                if let Err(error) = (action)().await {
                    eprintln!("error: {}", error);
                }
            }
            InteractiveCommand::ServiceVtl2 {
                user_mode_only,
                igvm,
                mana_keepalive,
                nvme_keepalive,
            } => {
                let paravisor_diag = paravisor_diag.clone();
                let vm_rpc = vm_rpc.clone();
                let igvm = igvm.or_else(|| opt.igvm.clone());
                let ged_rpc = resources.ged_rpc.clone();
                let r = async move {
                    let start;
                    if user_mode_only {
                        start = Instant::now();
                        paravisor_diag.restart().await?;
                    } else {
                        let path = igvm.context("no igvm file loaded")?;
                        let file = fs_err::File::open(path)?;
                        start = Instant::now();
                        openvmm_helpers::underhill::save_underhill(
                            &vm_rpc,
                            ged_rpc.as_ref().context("no GED")?,
                            GuestServicingFlags {
                                nvme_keepalive,
                                mana_keepalive,
                            },
                            file.into(),
                        )
                        .await?;
                        openvmm_helpers::underhill::restore_underhill(
                            &vm_rpc,
                            ged_rpc.as_ref().context("no GED")?,
                        )
                        .await?;
                    }
                    let end = Instant::now();
                    Ok(end - start)
                }
                .map(|r| Ok(StateChange::ServiceVtl2(r)));
                if state_change_task.is_some() {
                    tracing::error!("state change already in progress");
                } else {
                    state_change_task = Some(driver.spawn("state-change", r));
                }
            }
            InteractiveCommand::Quit => {
                tracing::info!("quitting");
                // Work around the detached SCSI task holding up worker stop.
                // TODO: Fix the underlying bug
                resources.scsi_rpc = None;

                vm_worker.stop();
                quit = true;
            }
            InteractiveCommand::ReadMemory { gpa, size, file } => {
                let size = size as usize;
                let data = vm_rpc.call(VmRpc::ReadMemory, (gpa, size)).await?;

                match data {
                    Ok(bytes) => {
                        if let Some(file) = file {
                            if let Err(err) = fs_err::write(file, bytes) {
                                eprintln!("error: {err:?}");
                            }
                        } else {
                            let width = 16;
                            let show_ascii = true;

                            let mut dump = String::new();
                            for (i, chunk) in bytes.chunks(width).enumerate() {
                                let hex_part: Vec<String> =
                                    chunk.iter().map(|byte| format!("{:02x}", byte)).collect();
                                let hex_line = hex_part.join(" ");

                                if show_ascii {
                                    let ascii_part: String = chunk
                                        .iter()
                                        .map(|&byte| {
                                            if byte.is_ascii_graphic() || byte == b' ' {
                                                byte as char
                                            } else {
                                                '.'
                                            }
                                        })
                                        .collect();
                                    dump.push_str(&format!(
                                        "{:04x}: {:<width$}  {}\n",
                                        i * width,
                                        hex_line,
                                        ascii_part,
                                        width = width * 3 - 1
                                    ));
                                } else {
                                    dump.push_str(&format!("{:04x}: {}\n", i * width, hex_line));
                                }
                            }

                            println!("{dump}");
                        }
                    }
                    Err(err) => {
                        eprintln!("error: {err:?}");
                    }
                }
            }
            InteractiveCommand::WriteMemory { gpa, hex, file } => {
                if hex.is_some() == file.is_some() {
                    eprintln!("error: either path to the file or the hex string must be specified");
                    continue;
                }

                let data = if let Some(file) = file {
                    let data = fs_err::read(file);
                    match data {
                        Ok(data) => data,
                        Err(err) => {
                            eprintln!("error: {err:?}");
                            continue;
                        }
                    }
                } else if let Some(hex) = hex {
                    if hex.len() & 1 != 0 {
                        eprintln!(
                            "error: expected even number of hex digits (2 hex digits per byte)"
                        );
                        continue;
                    }
                    let data: Result<Vec<u8>, String> = (0..hex.len())
                        .step_by(2)
                        .map(|i| {
                            u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| {
                                format!("invalid hex character at position {}: {}", i, e)
                            })
                        })
                        .collect();

                    match data {
                        Ok(data) => data,
                        Err(err) => {
                            eprintln!("error: {err}");
                            continue;
                        }
                    }
                } else {
                    unreachable!();
                };

                if data.is_empty() {
                    eprintln!("error: no data to write");
                    continue;
                }

                if let Err(err) = vm_rpc.call(VmRpc::WriteMemory, (gpa, data)).await? {
                    eprintln!("error: {err:?}");
                }
            }
            InteractiveCommand::Kvp(command) => {
                let Some(kvp) = &resources.kvp_ic else {
                    eprintln!("error: no kvp ic configured");
                    continue;
                };
                if let Err(err) = kvp::handle_kvp(kvp, command).await {
                    eprintln!("error: {err:#}");
                }
            }
            InteractiveCommand::Input { .. } | InteractiveCommand::InputMode => unreachable!(),
        }
    }

    vm_worker.stop();
    vm_worker.join().await?;
    Ok(())
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
pub struct DiagInspector(DiagInspectorInner);

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

enum InspectTarget {
    Host,
    Paravisor,
}

mod interactive_console {
    use super::InteractiveCommand;
    use rustyline::Helper;
    use rustyline::Highlighter;
    use rustyline::Hinter;
    use rustyline::Validator;

    #[derive(Helper, Highlighter, Hinter, Validator)]
    pub(crate) struct OpenvmmRustylineEditor {
        pub openvmm_inspect_req: std::sync::Arc<
            mesh::Sender<(
                super::InspectTarget,
                String,
                mesh::OneshotSender<inspect::Node>,
            )>,
        >,
    }

    impl rustyline::completion::Completer for OpenvmmRustylineEditor {
        type Candidate = String;

        fn complete(
            &self,
            line: &str,
            pos: usize,
            _ctx: &rustyline::Context<'_>,
        ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
            let Ok(cmd) = shell_words::split(line) else {
                return Ok((0, Vec::with_capacity(0)));
            };

            let completions = futures::executor::block_on(
                clap_dyn_complete::Complete {
                    cmd,
                    raw: Some(line.into()),
                    position: Some(pos),
                }
                .generate_completions::<InteractiveCommand>(None, self),
            );

            let pos_from_end = {
                let line = line.chars().take(pos).collect::<String>();

                let trailing_ws = line.len() - line.trim_end().len();

                if trailing_ws > 0 {
                    line.len() - trailing_ws + 1 // +1 for the space
                } else {
                    let last_word = shell_words::split(&line)
                        .unwrap_or_default()
                        .last()
                        .cloned()
                        .unwrap_or_default();

                    line.len() - last_word.len()
                }
            };

            Ok((pos_from_end, completions))
        }
    }

    impl clap_dyn_complete::CustomCompleterFactory for &OpenvmmRustylineEditor {
        type CustomCompleter = OpenvmmComplete;
        async fn build(&self, _ctx: &clap_dyn_complete::RootCtx<'_>) -> Self::CustomCompleter {
            OpenvmmComplete {
                openvmm_inspect_req: self.openvmm_inspect_req.clone(),
            }
        }
    }

    pub struct OpenvmmComplete {
        openvmm_inspect_req: std::sync::Arc<
            mesh::Sender<(
                super::InspectTarget,
                String,
                mesh::OneshotSender<inspect::Node>,
            )>,
        >,
    }

    impl clap_dyn_complete::CustomCompleter for OpenvmmComplete {
        async fn complete(
            &self,
            ctx: &clap_dyn_complete::RootCtx<'_>,
            subcommand_path: &[&str],
            arg_id: &str,
        ) -> Vec<String> {
            match (subcommand_path, arg_id) {
                (["openvmm", "inspect"], "element") => {
                    let on_error = vec!["failed/to/connect".into()];

                    let (parent_path, to_complete) = (ctx.to_complete)
                        .rsplit_once('/')
                        .unwrap_or(("", ctx.to_complete));

                    let node = {
                        let paravisor = {
                            let raw_arg = ctx
                                .matches
                                .subcommand()
                                .unwrap()
                                .1
                                .get_one::<String>("paravisor")
                                .map(|x| x.as_str())
                                .unwrap_or_default();
                            raw_arg == "true"
                        };

                        let (tx, rx) = mesh::oneshot();
                        self.openvmm_inspect_req.send((
                            if paravisor {
                                super::InspectTarget::Paravisor
                            } else {
                                super::InspectTarget::Host
                            },
                            parent_path.to_owned(),
                            tx,
                        ));
                        let Ok(node) = rx.await else {
                            return on_error;
                        };

                        node
                    };

                    let mut completions = Vec::new();

                    if let inspect::Node::Dir(dir) = node {
                        for entry in dir {
                            if entry.name.starts_with(to_complete) {
                                if parent_path.is_empty() {
                                    completions.push(format!("{}/", entry.name))
                                } else {
                                    completions.push(format!(
                                        "{}/{}{}",
                                        parent_path,
                                        entry.name,
                                        if matches!(entry.node, inspect::Node::Dir(..)) {
                                            "/"
                                        } else {
                                            ""
                                        }
                                    ))
                                }
                            }
                        }
                    } else {
                        return on_error;
                    }

                    completions
                }
                _ => Vec::new(),
            }
        }
    }
}
