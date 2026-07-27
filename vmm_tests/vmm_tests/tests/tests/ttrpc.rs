// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for OpenVMM's TTRPC interface.

// Tests for the fd-passing protocol, which shares the TTRPC socket. It is
// UNIX-only and tap `fd_name` resolution is Linux-only, so it only builds on
// Linux.
#[cfg(target_os = "linux")]
mod fd_passing;

use anyhow::Context;
use futures::AsyncReadExt;
use guid::Guid;
use mesh::CancelContext;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use petri::ResolvedArtifact;
use petri_artifacts_vmm_test::artifacts;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use unix_socket::UnixListener;
use unix_socket::UnixStream;

petri::test!(test_ttrpc_interface, |resolver| {
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    let initrd = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE);
    Some([openvmm.erase(), kernel.erase(), initrd.erase()])
});

fn test_ttrpc_interface(
    params: petri::PetriTestParams<'_>,
    [openvmm, kernel_path, initrd_path]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    // All temporary files for this test live under a single temp directory
    // that is cleaned up automatically when it is dropped at the end of the
    // test.
    let tempdir = tempfile::tempdir()?;
    let socket_path = tempdir.path().join("ttrpc.sock");
    let pidfile_path = tempdir.path().join("openvmm.pid");

    // The serial console device differs by architecture: x86 exposes a 16550
    // UART as `ttyS0`, while aarch64 exposes a PL011 UART as `ttyAMA0`.
    let console = match petri_artifacts_common::tags::MachineArch::host() {
        petri_artifacts_common::tags::MachineArch::X86_64 => "ttyS0",
        petri_artifacts_common::tags::MachineArch::Aarch64 => "ttyAMA0",
    };

    DefaultPool::run_with(async |driver| {
        let (mut child, client, _stderr_task) =
            launch_openvmm(&driver, &params, &openvmm, &socket_path, &pidfile_path).await?;

        let query_props = || {
            client.call().start(
                vmservice::Vm::PropertiesVm,
                vmservice::PropertiesVmRequest { types: Vec::new() },
            )
        };

        let caps = client
            .call()
            .start(vmservice::Vm::CapabilitiesVm, ())
            .await
            .unwrap();
        assert!(
            caps.supported_resources.iter().any(|r| r.resource
                == vmservice::capabilities_vm_response::Resource::Scsi as i32
                && r.add),
            "SCSI add should be advertised as a supported resource"
        );
        assert!(
            caps.supported_resources.iter().any(|r| r.resource
                == vmservice::capabilities_vm_response::Resource::Vpci as i32
                && r.add
                && r.remove
                && !r.update),
            "vPCI add/remove should be advertised as supported"
        );
        assert_eq!(
            caps.supported_guest_os,
            vec![vmservice::capabilities_vm_response::SupportedGuestOs::Linux as i32],
            "only Linux direct boot is supported"
        );

        let props = query_props().await.unwrap();
        assert_eq!(
            props.state,
            vmservice::VmState::Uninitialized as i32,
            "no VM created yet, expected UNINITIALIZED"
        );

        client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                vmservice::CreateVmRequest {
                    config: Some(vmservice::VmConfig::default()),
                    log_id: String::new(),
                },
            )
            .await
            .unwrap_err();
        let props = query_props().await.unwrap();
        assert_eq!(
            props.state,
            vmservice::VmState::Uninitialized as i32,
            "a failed CreateVm must leave state UNINITIALIZED"
        );

        // Backing files for the PCIe storage devices created on iteration 0
        // (virtio-blk and an NVMe namespace). They are plain raw disks.
        let nvme_disk_path = tempdir.path().join("nvme.img");
        let blk_disk_path = tempdir.path().join("blk.img");
        for path in [&nvme_disk_path, &blk_disk_path] {
            std::fs::File::create(path)?.set_len(1024 * 1024)?;
        }

        for i in 0..3 {
            let com1_path = tempdir.path().join(format!("com1-{i}.sock"));
            let console_path = tempdir.path().join(format!("console-{i}.sock"));
            let virtiofs_root = tempdir.path().join(format!("virtiofs-{i}"));
            std::fs::create_dir_all(&virtiofs_root)?;

            let consomme_nic_id = Guid::new_random().to_string();

            // On iteration 0, test `connect: true` for both serial and
            // virtio console by pre-creating listeners that the VM will
            // connect to. On other iterations, test the default
            // `connect: false` (VM creates the socket).
            let use_connect = i == 0;
            let com1_listener = if use_connect {
                Some(UnixListener::bind(&com1_path).unwrap())
            } else {
                None
            };
            let console_listener = if use_connect {
                Some(UnixListener::bind(&console_path).unwrap())
            } else {
                None
            };

            // On iteration 0, exercise the richer CreateVM surface: a NUMA
            // topology (replacing flat memory), an explicit processor topology,
            // and a PCIe topology with virtio + NVMe devices behind root ports
            // and a switch, plus an empty hotplug port used below for
            // AddPcieDevice/RemovePcieDevice. Other iterations use the simpler
            // flat-memory configuration so the flat path stays covered too.
            let (memory_config, numa_config, processor_config, pcie) = if i == 0 {
                let switch = vmservice::PcieSwitch {
                    name: "sw0".to_string(),
                    downstream_ports: vec![
                        vmservice::PciePort {
                            name: "sw0-dp0".to_string(),
                            hotplug: false,
                            attached: Some(attachment_device(virtio_device(
                                vmservice::virtio_device::Kind::Blk(vmservice::VirtioBlk {
                                    backend: Some(file_disk(&blk_disk_path)),
                                    read_only: false,
                                }),
                            ))),
                            devfn: None,
                        },
                        vmservice::PciePort {
                            name: "sw0-dp1".to_string(),
                            hotplug: false,
                            attached: None,
                            devfn: None,
                        },
                    ],
                };
                let root_complex = vmservice::PcieRootComplex {
                    name: "rc0".to_string(),
                    segment: 0,
                    start_bus: 0,
                    end_bus: 255,
                    low_mmio: 64 * 1024 * 1024,
                    high_mmio: 1024 * 1024 * 1024,
                    root_ports: vec![
                        // virtio-rng behind a root port.
                        pcie_root_port(
                            "rp0",
                            false,
                            Some(attachment_device(virtio_device(
                                vmservice::virtio_device::Kind::Rng(vmservice::VirtioRng {}),
                            ))),
                        ),
                        // NVMe controller with a file-backed namespace.
                        pcie_root_port(
                            "rp1",
                            false,
                            Some(attachment_device(vmservice::PcieDeviceKind {
                                kind: Some(vmservice::pcie_device_kind::Kind::Nvme(
                                    vmservice::NvmeConfig {
                                        controller_id: "nvme0".to_string(),
                                        namespaces: vec![vmservice::NvmeNamespace {
                                            nsid: 1,
                                            backend: Some(file_disk(&nvme_disk_path)),
                                            read_only: false,
                                        }],
                                    },
                                )),
                            })),
                        ),
                        // virtio-net (consomme) behind a root port.
                        pcie_root_port(
                            "rp2",
                            false,
                            Some(attachment_device(virtio_device(
                                vmservice::virtio_device::Kind::Net(vmservice::VirtioNet {
                                    max_queues: None,
                                    mac_address: "00-15-5D-12-12-13".to_string(),
                                    backend: Some(vmservice::NicBackend {
                                        kind: Some(vmservice::nic_backend::Kind::Consomme(
                                            vmservice::ConsommeBackend {
                                                cidr: String::new(),
                                                ports: vec![],
                                            },
                                        )),
                                    }),
                                }),
                            ))),
                        ),
                        // A switch hosting a virtio-blk device on its first
                        // downstream port.
                        pcie_root_port("rp3", false, Some(attachment_switch(switch))),
                        // Empty hotplug-capable port for AddPcieDevice.
                        pcie_root_port("rphp", true, None),
                    ],
                    ..Default::default()
                };
                (
                    None,
                    Some(vmservice::NumaConfig {
                        nodes: vec![
                            vmservice::NumaNode {
                                memory: Some(vmservice::NodeMemoryConfig {
                                    memory_mb: 128,
                                    ..Default::default()
                                }),
                                vps: None,
                            },
                            vmservice::NumaNode {
                                memory: Some(vmservice::NodeMemoryConfig {
                                    memory_mb: 128,
                                    ..Default::default()
                                }),
                                vps: None,
                            },
                        ],
                        distances: vec![vmservice::NumaDistance {
                            src: 0,
                            dst: 1,
                            distance: 20,
                        }],
                    }),
                    Some(vmservice::ProcessorConfig {
                        processor_count: 2,
                        ..Default::default()
                    }),
                    Some(vmservice::PcieTopologyConfig {
                        root_complexes: vec![root_complex],
                    }),
                )
            } else {
                (
                    Some(vmservice::MemoryConfig {
                        memory_mb: 256,
                        ..Default::default()
                    }),
                    None,
                    Some(vmservice::ProcessorConfig {
                        processor_count: 2,
                        ..Default::default()
                    }),
                    None,
                )
            };

            let guest_command = if i == 1 { "sleep 30" } else { "poweroff -f" };

            client
                .call()
                .start(
                    vmservice::Vm::CreateVm,
                    vmservice::CreateVmRequest {
                        config: Some(vmservice::VmConfig {
                            memory_config,
                            numa_config,
                            processor_config,
                            pcie,
                            boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                                vmservice::DirectBoot {
                                    kernel_path: kernel_path.get().to_string_lossy().to_string(),
                                    initrd_path: initrd_path.get().to_string_lossy().to_string(),
                                    kernel_cmdline: format!(
                                        "console={console} rdinit=/bin/busybox panic=-1 -- {guest_command}"
                                    ),
                                },
                            )),
                            serial_config: Some(vmservice::SerialConfig {
                                ports: vec![vmservice::serial_config::Config {
                                    port: 0,
                                    socket_path: com1_path.to_string_lossy().into(),
                                    connect: use_connect,
                                }],
                            }),
                            devices_config: Some(vmservice::DevicesConfig {
                                nic_config: vec![vmservice::NicConfig {
                                    nic_id: consomme_nic_id.clone(),
                                    mac_address: "00-15-5D-12-12-12".to_string(),
                                    backend: Some(vmservice::nic_config::Backend::Consomme(
                                        vmservice::ConsommeBackend {
                                            cidr: String::new(),
                                            ports: vec![],
                                        },
                                    )),
                                    ..Default::default()
                                }],
                                virtio_console: Some(vmservice::VirtioConsoleConfig {
                                    socket_path: console_path.to_string_lossy().into(),
                                    connect: use_connect,
                                }),
                                virtiofs_config: vec![vmservice::VirtioFsConfig {
                                    tag: "testfs".to_string(),
                                    root_path: virtiofs_root.to_string_lossy().into(),
                                }],
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        log_id: String::new(),
                    },
                )
                .await
                .unwrap();

            let props = query_props().await.unwrap();
            assert_eq!(
                props.state,
                vmservice::VmState::Paused as i32,
                "VM should be PAUSED immediately after CreateVm"
            );
            assert!(
                props.memory_stats.is_none() && props.processor_stats.is_none(),
                "memory/processor stats should be unset, not zeroed"
            );

            // Invalid protocols exercise Consomme update/remove without binding a port.
            for modify_type in [vmservice::ModifyType::Update, vmservice::ModifyType::Remove] {
                let err = client
                    .call()
                    .start(
                        vmservice::Vm::ModifyResource,
                        vmservice::ModifyResourceRequest {
                            r#type: modify_type as i32,
                            resource: Some(
                                vmservice::modify_resource_request::Resource::NicConfig(
                                    vmservice::NicConfig {
                                        nic_id: consomme_nic_id.clone(),
                                        mac_address: "00-15-5D-12-12-12".to_string(),
                                        backend: Some(vmservice::nic_config::Backend::Consomme(
                                            vmservice::ConsommeBackend {
                                                cidr: String::new(),
                                                ports: vec![vmservice::PortConfig {
                                                    host_port: 8080,
                                                    guest_port: 80,
                                                    protocol: 99,
                                                }],
                                            },
                                        )),
                                        ..Default::default()
                                    },
                                ),
                            ),
                        },
                    )
                    .await
                    .unwrap_err();
                assert!(
                    err.message.contains("invalid protocol"),
                    "expected invalid protocol error, got: {}",
                    err.message
                );
            }

            // On iteration 0, hot-add a virtio-rng device to the empty
            // hotplug-capable port and then hot-remove it, exercising the
            // AddPcieDevice/RemovePcieDevice RPCs.
            if i == 0 {
                client
                    .call()
                    .start(
                        vmservice::Vm::AddPcieDevice,
                        vmservice::AddPcieDeviceRequest {
                            port_name: "rphp".to_string(),
                            device: Some(virtio_device(vmservice::virtio_device::Kind::Rng(
                                vmservice::VirtioRng {},
                            ))),
                        },
                    )
                    .await
                    .unwrap();

                client
                    .call()
                    .start(
                        vmservice::Vm::RemovePcieDevice,
                        vmservice::RemovePcieDeviceRequest {
                            port_name: "rphp".to_string(),
                        },
                    )
                    .await
                    .unwrap();
            }

            // Get the serial connection - either by accepting on our listener
            // (connect: true) or connecting to the VM's socket (connect: false).
            let com1 = if let Some(listener) = com1_listener {
                let (stream, _) = listener.accept().unwrap();
                stream
            } else {
                UnixStream::connect(&com1_path).unwrap()
            };

            // Get the console connection the same way.
            let console = if let Some(listener) = console_listener {
                let (stream, _) = listener.accept().unwrap();
                stream
            } else {
                UnixStream::connect(&console_path).unwrap()
            };

            let _com1_task = driver.spawn(
                "com1",
                petri::log_task(
                    params.logger.log_file("linux").unwrap(),
                    PolledSocket::new(&driver, com1).unwrap(),
                    "linux com1",
                ),
            );

            let _console_task = driver.spawn(
                "console",
                petri::log_task(
                    params.logger.log_file("virtio-console").unwrap(),
                    PolledSocket::new(&driver, console).unwrap(),
                    "virtio console",
                ),
            );

            assert_eq!(
                client
                    .call()
                    .timeout(Some(Duration::from_millis(100)))
                    .start(vmservice::Vm::WaitVm, (),)
                    .await
                    .unwrap_err()
                    .code,
                mesh_rpc::service::Code::DeadlineExceeded as i32
            );

            let waiter = client.call().start(vmservice::Vm::WaitVm, ());

            match i {
                0 | 2 => {
                    client
                        .call()
                        .start(vmservice::Vm::ResumeVm, ())
                        .await
                        .unwrap();

                    let props = query_props().await.unwrap();
                    assert_eq!(
                        props.state,
                        vmservice::VmState::Running as i32,
                        "after ResumeVm, expected RUNNING"
                    );

                    waiter.await.unwrap();

                    let props = query_props().await.unwrap();
                    assert_eq!(
                        props.state,
                        vmservice::VmState::Halted as i32,
                        "guest powered off, expected HALTED"
                    );
                    assert!(
                        props.halt_reason.as_deref().is_some_and(|r| !r.is_empty()),
                        "HALTED state should carry a halt_reason"
                    );

                    if i == 0 {
                        client
                            .call()
                            .start(vmservice::Vm::TeardownVm, ())
                            .await
                            .unwrap();

                        let props = query_props().await.unwrap();
                        assert_eq!(
                            props.state,
                            vmservice::VmState::Uninitialized as i32,
                            "after TeardownVm, expected UNINITIALIZED"
                        );
                        assert!(
                            props.halt_reason.is_none(),
                            "after TeardownVm, halt_reason should be cleared"
                        );

                        client
                            .call()
                            .start(vmservice::Vm::WaitVm, ())
                            .await
                            .unwrap_err();
                    } else {
                        let _ = client.call().start(vmservice::Vm::Quit, ()).await;
                    }
                }
                1 => {
                    client
                        .call()
                        .start(vmservice::Vm::ResumeVm, ())
                        .await
                        .unwrap();

                    let props = query_props().await.unwrap();
                    assert_eq!(
                        props.state,
                        vmservice::VmState::Running as i32,
                        "after ResumeVm, expected RUNNING"
                    );

                    client
                        .call()
                        .start(vmservice::Vm::PauseVm, ())
                        .await
                        .unwrap();

                    let props = query_props().await.unwrap();
                    assert_eq!(
                        props.state,
                        vmservice::VmState::Paused as i32,
                        "after PauseVm, expected PAUSED"
                    );

                    client
                        .call()
                        .start(vmservice::Vm::TeardownVm, ())
                        .await
                        .unwrap();

                    waiter.await.unwrap_err();
                }
                _ => unreachable!(),
            }
        }

        let exit_status = child.wait().await?;

        // Surface the OpenVMM exit status so that abnormal exits (e.g. an abort
        // from a panic — the workspace uses `panic = 'abort'`) are visible in
        // test logs alongside any pidfile/cleanup assertion below.
        tracing::info!(?exit_status, "openvmm exited");
        assert!(
            exit_status.success(),
            "openvmm exited abnormally: {:?}",
            exit_status
        );

        // Verify the pidfile was cleaned up on exit.
        assert!(
            !pidfile_path.exists(),
            "pidfile should be removed after exit"
        );

        Ok(())
    })
}

/// Wraps a `PcieDeviceKind` as a device attachment behind a PCIe port.
fn attachment_device(device: vmservice::PcieDeviceKind) -> vmservice::PcieAttachment {
    vmservice::PcieAttachment {
        kind: Some(vmservice::pcie_attachment::Kind::Device(device)),
    }
}

/// Wraps a `PcieSwitch` as a switch attachment behind a PCIe port.
fn attachment_switch(switch: vmservice::PcieSwitch) -> vmservice::PcieAttachment {
    vmservice::PcieAttachment {
        kind: Some(vmservice::pcie_attachment::Kind::Switch(switch)),
    }
}

/// Builds a PCIe root port with the given name, hotplug flag, and optional
/// attached device/switch.
fn pcie_root_port(
    name: &str,
    hotplug: bool,
    attached: Option<vmservice::PcieAttachment>,
) -> vmservice::PciePort {
    vmservice::PciePort {
        name: name.to_string(),
        hotplug,
        attached,
        devfn: None,
    }
}

/// Wraps a virtio device function kind as a `PcieDeviceKind`.
fn virtio_device(kind: vmservice::virtio_device::Kind) -> vmservice::PcieDeviceKind {
    vmservice::PcieDeviceKind {
        kind: Some(vmservice::pcie_device_kind::Kind::Virtio(
            vmservice::VirtioDevice { kind: Some(kind) },
        )),
    }
}

/// Spawns `openvmm --rpc path=<socket_path>,transport=ttrpc --pidfile
/// <pidfile_path>`, waits for it to signal readiness (by closing stdout),
/// validates the pidfile, and connects a ttrpc client.
///
/// Returns the child process, a connected ttrpc client, and the stderr-pump
/// task (which must be kept alive for the child's lifetime).
async fn launch_openvmm(
    driver: &DefaultDriver,
    params: &petri::PetriTestParams<'_>,
    openvmm: &ResolvedArtifact,
    socket_path: &Path,
    pidfile_path: &Path,
) -> anyhow::Result<(
    PolledChild<std::process::Child>,
    mesh_rpc::Client,
    Task<anyhow::Result<()>>,
)> {
    tracing::info!(socket_path = %socket_path.display(), "launching OpenVMM with ttrpc");

    let (stderr_read, stderr_write) = pal::pipe_pair()?;
    let (stdout_read, stdout_write) = pal::pipe_pair()?;
    let child = std::process::Command::new(openvmm)
        .arg("--rpc")
        .arg(format!("path={},transport=ttrpc", socket_path.display()))
        .arg("--pidfile")
        .arg(pidfile_path)
        .stdin(Stdio::null())
        .stdout(stdout_write)
        .stderr(stderr_write)
        .spawn()?;

    let mut child = PolledChild::<std::process::Child>::new(driver, child)?;

    // Start pumping stderr immediately so the pipe buffer doesn't fill up and
    // block the child.
    let stderr_task = driver.spawn(
        "stderr",
        petri::log_task(
            params.logger.log_file("stderr")?,
            PolledPipe::new(driver, stderr_read)?,
            "openvmm stderr",
        ),
    );

    // Wait for stdout to close (readiness signal). If the child crashes at
    // startup, stdout closes too and we detect the exit when the pidfile is
    // missing.
    let mut stdout = PolledPipe::new(driver, stdout_read)?;
    let mut buf = [0u8; 1];
    let n = stdout
        .read(&mut buf)
        .await
        .context("reading from openvmm stdout")?;
    anyhow::ensure!(n == 0, "openvmm wrote unexpected data to stdout");
    drop(stdout);

    // Verify the pidfile was created with the correct PID. If it's missing,
    // wait briefly for the child to exit (the PidfileGuard deletes it on drop)
    // and report the exit status.
    let pid_content = match std::fs::read_to_string(pidfile_path) {
        Ok(s) => s,
        Err(e) => {
            let wait_result = CancelContext::new()
                .with_timeout(Duration::from_secs(10))
                .until_cancelled(child.wait())
                .await;
            match wait_result {
                Ok(Ok(status)) => {
                    let _ = stderr_task.await;
                    anyhow::bail!("openvmm exited with {status} before pidfile was created");
                }
                _ => {
                    return Err(e).context("failed to read pidfile");
                }
            }
        }
    };
    assert_eq!(
        pid_content,
        format!("{}\n", child.get().id()),
        "pidfile should contain the child PID"
    );

    let client = mesh_rpc::Client::new(
        driver,
        mesh_rpc::client::UnixDialier::new(driver.clone(), socket_path.to_path_buf()),
    );

    Ok((child, client, stderr_task))
}

/// Builds a file-backed disk backend for the given path.
fn file_disk(path: &Path) -> vmservice::DiskBackend {
    vmservice::DiskBackend {
        kind: Some(vmservice::disk_backend::Kind::File(vmservice::FileDisk {
            path: path.to_string_lossy().into(),
            direct: false,
        })),
    }
}
