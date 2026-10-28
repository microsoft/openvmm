// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::utils::virtio_blk::DiskBacking;
use crate::utils::virtio_blk::TestDisk;
use crate::utils::virtio_blk::quiesce_disk;
use crate::utils::virtio_blk::test_pattern;
use crate::utils::virtio_blk::verify_io;
use crate::utils::virtio_blk::wait_for_disk;
use crate::utils::virtio_blk::wait_for_pci_removal;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use vmm_test_macros::vmm_test_with;

type RpcArtifacts<A> = (
    ResolvedArtifact<A>,
    ResolvedArtifact<artifacts::snp_igvm::SNP_LINUX_DIRECT_IGVM_X64>,
);

fn rpc_error(status: mesh_rpc::service::Status) -> anyhow::Error {
    anyhow::anyhow!("RPC failed: {status:?}")
}

// TODO: Enable once the SNP guest can use ECAM above 4 GiB. The non-ttrpc
// SNP tests cover the same block I/O paths with ECAM below 4 GiB.
#[vmm_test_with(
    openvmm,
    ignore(reason = "SNP guest needs support for PCI ECAM above 4 GiB"),
    configs(linux_direct_x64[snp][
        artifacts::OPENVMM_NATIVE,
        artifacts::snp_igvm::SNP_LINUX_DIRECT_IGVM_X64
    ])
)]
async fn virtio_blk_file<A>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: RpcArtifacts<A>,
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(config, artifacts, driver, DiskBacking::File, false).await
}

#[vmm_test_with(
    openvmm,
    ignore(reason = "SNP guest needs support for PCI ECAM above 4 GiB"),
    configs(linux_direct_x64[snp][
        artifacts::OPENVMM_NATIVE,
        artifacts::snp_igvm::SNP_LINUX_DIRECT_IGVM_X64
    ])
)]
async fn virtio_blk_file_direct<A>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: RpcArtifacts<A>,
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(config, artifacts, driver, DiskBacking::File, true).await
}

#[vmm_test_with(
    openvmm,
    ignore(reason = "SNP guest needs support for PCI ECAM above 4 GiB"),
    configs(linux_direct_x64[snp][
        artifacts::OPENVMM_NATIVE,
        artifacts::snp_igvm::SNP_LINUX_DIRECT_IGVM_X64
    ])
)]
async fn virtio_blk_vmdk<A>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: RpcArtifacts<A>,
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(config, artifacts, driver, DiskBacking::Vmdk, false).await
}

#[vmm_test_with(
    openvmm,
    ignore(reason = "SNP guest needs support for PCI ECAM above 4 GiB"),
    requires(loop_device),
    configs(linux_direct_x64[snp][
        artifacts::OPENVMM_NATIVE,
        artifacts::snp_igvm::SNP_LINUX_DIRECT_IGVM_X64
    ])
)]
async fn virtio_blk_host_block_buffered<A>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: RpcArtifacts<A>,
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(
        config,
        artifacts,
        driver,
        DiskBacking::HostBlock { direct: false },
        false,
    )
    .await
}

#[vmm_test_with(
    openvmm,
    ignore(reason = "SNP guest needs support for PCI ECAM above 4 GiB"),
    requires(loop_device),
    configs(linux_direct_x64[snp][
        artifacts::OPENVMM_NATIVE,
        artifacts::snp_igvm::SNP_LINUX_DIRECT_IGVM_X64
    ])
)]
async fn virtio_blk_host_block_direct<A>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: RpcArtifacts<A>,
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(
        config,
        artifacts,
        driver,
        DiskBacking::HostBlock { direct: true },
        true,
    )
    .await
}

async fn run<A>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (openvmm, igvm): RpcArtifacts<A>,
    driver: DefaultDriver,
    backing: DiskBacking,
    direct: bool,
) -> anyhow::Result<()> {
    run_with_boot(
        config,
        openvmm.erase(),
        vmservice::vm_config::BootConfig::Igvm(vmservice::IgvmBoot {
            igvm_path: igvm.get().to_string_lossy().into(),
            personality: vmservice::igvm_boot::Personality::LinuxDirect as i32,
        }),
        driver,
        backing,
        direct,
    )
    .await
}

pub(super) async fn run_with_boot(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    openvmm: ResolvedArtifact,
    boot_config: vmservice::vm_config::BootConfig,
    driver: DefaultDriver,
    backing: DiskBacking,
    direct: bool,
) -> anyhow::Result<()> {
    const BOOT_SERIAL: &str = "SNP-RPC-BOOT";
    const HOT_SERIAL: &str = "SNP-RPC-HOT";
    let isolated = config.isolation().is_some();
    let boot_disk = TestDisk::new(backing).await?;
    let disk = TestDisk::new(backing).await?;
    let initial_read_only = matches!(backing, DiskBacking::Vmdk);
    let boot_pattern = test_pattern(37);
    let pattern = test_pattern(0xa7);
    if initial_read_only {
        boot_disk.seed_backing(&boot_pattern)?;
        disk.seed_backing(&pattern)?;
    }
    let directory = tempfile::tempdir()?;
    let disk_path = |disk: &TestDisk, name: &str| -> anyhow::Result<std::path::PathBuf> {
        let path = disk.path()?;
        if matches!(backing, DiskBacking::HostBlock { .. }) {
            let alias = directory.path().join(format!("{name}.vhdx"));
            std::os::unix::fs::symlink(path, &alias)?;
            Ok(alias)
        } else {
            Ok(path.to_owned())
        }
    };
    let boot_path = disk_path(&boot_disk, "boot")?;
    let hot_path = disk_path(&disk, "hot")?;
    let vsock_path = directory.path().join("vsock");
    let com1_path = directory.path().join("com1");
    let mut listener = PolledSocket::new(
        &driver,
        UnixListener::bind(format!(
            "{}_{}",
            vsock_path.display(),
            pipette_client::PIPETTE_PORT
        ))?,
    )?;
    let (mut child, client, stderr_task) = launch_openvmm(
        &driver,
        config.log_source(),
        &openvmm,
        &directory.path().join("rpc"),
        &directory.path().join("pid"),
    )
    .await?;
    let block_device = |path: &Path, serial: &str, read_only| -> vmservice::PcieDeviceKind {
        virtio_device(vmservice::virtio_device::Kind::Blk(vmservice::VirtioBlk {
            backend: Some(vmservice::DiskBackend {
                kind: Some(vmservice::disk_backend::Kind::File(vmservice::FileDisk {
                    path: path.to_string_lossy().into(),
                    direct,
                })),
            }),
            read_only,
            serial: Some(serial.into()),
        }))
    };
    client
        .call()
        .start(
            vmservice::Vm::CreateVm,
            vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    disable_vmbus: true,
                    memory_config: Some(vmservice::MemoryConfig {
                        memory_mb: 256,
                        ..Default::default()
                    }),
                    processor_config: Some(vmservice::ProcessorConfig {
                        processor_count: 2,
                        ..Default::default()
                    }),
                    isolation_config: isolated.then_some(vmservice::IsolationConfig {
                        isolation_type: Some(vmservice::isolation_config::IsolationType::Snp(
                            Default::default(),
                        )),
                    }),
                    boot_config: Some(boot_config),
                    serial_config: Some(vmservice::SerialConfig {
                        ports: vec![vmservice::serial_config::Config {
                            port: 0,
                            socket_path: com1_path.to_string_lossy().into(),
                            connect: false,
                        }],
                    }),
                    pcie: Some(vmservice::PcieTopologyConfig {
                        root_complexes: vec![vmservice::PcieRootComplex {
                            name: "rc0".into(),
                            end_bus: 255,
                            low_mmio: 64 * 1024 * 1024,
                            high_mmio: 1024 * 1024 * 1024,
                            root_ports: vec![
                                pcie_root_port(
                                    "vsock",
                                    false,
                                    Some(attachment_device(virtio_device(
                                        vmservice::virtio_device::Kind::Vsock(
                                            vmservice::VirtioVsock {
                                                socket_path: vsock_path.to_string_lossy().into(),
                                                guest_cid: Some(42),
                                            },
                                        ),
                                    ))),
                                ),
                                pcie_root_port(
                                    "blk",
                                    false,
                                    Some(attachment_device(block_device(
                                        &boot_path,
                                        BOOT_SERIAL,
                                        initial_read_only,
                                    ))),
                                ),
                                pcie_root_port("hotblk", true, None),
                            ],
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                log_id: String::new(),
            },
        )
        .await
        .map_err(rpc_error)
        .context("create virtio-blk VM over ttrpc")?;
    let serial_task = driver.spawn(
        "snp-com1",
        petri::log_task(
            config.log_source().log_file("snp-com1")?,
            PolledSocket::new(&driver, UnixStream::connect(&com1_path)?)?,
            "SNP COM1",
        ),
    );
    let waiter = client.call().start(vmservice::Vm::WaitVm, ());
    client
        .call()
        .start(vmservice::Vm::ResumeVm, ())
        .await
        .map_err(rpc_error)
        .context("resume virtio-blk VM over ttrpc")?;
    let agent = CancelContext::new()
        .with_timeout(Duration::from_secs(120))
        .until_cancelled(async {
            let (connection, _) = listener.accept().await?;
            pipette_client::PipetteClient::new(
                &driver,
                PolledSocket::new(&driver, connection)?,
                config.log_source().output_dir(),
            )
            .await
        })
        .await
        .context("guest pipette connection timed out")??;
    anyhow::ensure!(agent.local_vsock_cid() == Some(42), "wrong guest vsock CID");
    if isolated {
        let shell = agent.unix_shell();
        let dmesg = cmd!(shell, "dmesg").read().await?;
        anyhow::ensure!(
            dmesg.contains("SEV: Status: SEV SEV-ES SEV-SNP"),
            "guest did not confirm active SNP"
        );
    }
    wait_for_disk(&agent, BOOT_SERIAL, true, &driver).await?;
    verify_io(&agent, BOOT_SERIAL, &boot_pattern, initial_read_only).await?;
    boot_disk.verify_backing(&boot_pattern)?;

    for read_only in [initial_read_only, true] {
        client
            .call()
            .start(
                vmservice::Vm::AddPcieDevice,
                vmservice::AddPcieDeviceRequest {
                    port_name: "hotblk".into(),
                    device: Some(block_device(&hot_path, HOT_SERIAL, read_only)),
                },
            )
            .await
            .map_err(rpc_error)
            .context("add virtio-blk over ttrpc")?;
        wait_for_disk(&agent, HOT_SERIAL, true, &driver).await?;
        verify_io(&agent, HOT_SERIAL, &pattern, read_only).await?;
        disk.verify_backing(&pattern)?;
        let pci_path = quiesce_disk(&agent, HOT_SERIAL).await?;
        client
            .call()
            .start(
                vmservice::Vm::RemovePcieDevice,
                vmservice::RemovePcieDeviceRequest {
                    port_name: "hotblk".into(),
                },
            )
            .await
            .map_err(rpc_error)
            .context("remove virtio-blk over ttrpc")?;
        wait_for_disk(&agent, HOT_SERIAL, false, &driver).await?;
        wait_for_pci_removal(&agent, &pci_path, &driver).await?;
        boot_disk.verify_backing(&boot_pattern)?;
    }

    agent.power_off().await?;
    waiter
        .await
        .map_err(rpc_error)
        .context("wait for guest shutdown")?;
    client
        .call()
        .start(vmservice::Vm::TeardownVm, ())
        .await
        .map_err(rpc_error)
        .context("teardown virtio-blk VM over ttrpc")?;
    let quit = client.call().start(vmservice::Vm::Quit, ());
    let status = CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(child.wait())
        .await??;
    anyhow::ensure!(status.success(), "RPC process exited with {status}");
    if let Err(error) = quit.await {
        anyhow::ensure!(
            error.code == mesh_rpc::service::Code::Unavailable as i32
                && error.message == "channel closed",
            "Quit failed: {error:?}"
        );
        tracing::debug!("RPC channel closed after the process exited successfully");
    }
    stderr_task.await?;
    serial_task.await?;
    boot_disk.close()?;
    disk.close()?;
    Ok(())
}
