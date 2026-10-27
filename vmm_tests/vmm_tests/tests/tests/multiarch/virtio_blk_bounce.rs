// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Verified virtio-blk I/O across PCIe removal and re-add with owned buffers.

use crate::utils::virtio_blk::DiskBacking;
use crate::utils::virtio_blk::TestDisk;
use crate::utils::virtio_blk::quiesce_disk;
use crate::utils::virtio_blk::test_pattern;
use crate::utils::virtio_blk::verify_io;
use crate::utils::virtio_blk::verify_payload;
use crate::utils::virtio_blk::wait_for_disk;
use crate::utils::virtio_blk::wait_for_pci_removal;
use openvmm_defs::config::PcieDeviceConfig;
use pal_async::DefaultDriver;
use petri::MemoryConfig;
use petri::PetriVmBuilder;
use petri::ProcessorTopology;
use petri::openvmm::OpenVmmPetriBackend;
use pipette_client::cmd;
use virtio_resources::VirtioPciDeviceHandle;
use virtio_resources::blk::VirtioBlkHandle;
use vm_resource::IntoResource;
use vmm_test_macros::vmm_test_with;

#[vmm_test_with(openvmm, configs(linux_direct_x64, linux_direct_x64[snp]))]
async fn virtio_blk_bounce_ram(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(config, driver, DiskBacking::Ram).await
}

#[vmm_test_with(openvmm, configs(linux_direct_x64, linux_direct_x64[snp]))]
async fn virtio_blk_bounce_file(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(config, driver, DiskBacking::File).await
}

#[vmm_test_with(openvmm, configs(linux_direct_x64, linux_direct_x64[snp]))]
async fn virtio_blk_bounce_vmdk(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(config, driver, DiskBacking::Vmdk).await
}

#[vmm_test_with(
    openvmm,
    requires(loop_device),
    configs(linux_direct_x64, linux_direct_x64[snp])
)]
async fn virtio_blk_bounce_host_block_buffered(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(config, driver, DiskBacking::HostBlock { direct: false }).await
}

#[vmm_test_with(
    openvmm,
    requires(loop_device),
    configs(linux_direct_x64, linux_direct_x64[snp])
)]
async fn virtio_blk_bounce_host_block_direct(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    run(config, driver, DiskBacking::HostBlock { direct: true }).await
}

async fn run(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    driver: DefaultDriver,
    backing: DiskBacking,
) -> anyhow::Result<()> {
    const BOOT_SERIAL: &str = "BOUNCE-BOOT";
    const HOT_SERIAL: &str = "BOUNCE-HOT";
    const BOOT_PORT: &str = "s0rc0rp1";
    const HOT_PORT: &str = "s0rc0rp2";
    let isolated = config.isolation().is_some();
    let kvm = petri::requirements::HostContext::new()
        .await
        .openvmm_hypervisor
        == Some(petri::requirements::OpenVmmHypervisor::Kvm);
    // Keep Hyper-V clocks on WHP/MSHV; CopyOnly cannot use KVM's page overlays.
    let config = if !isolated && kvm {
        config.with_no_hv()
    } else {
        config.with_no_vmbus()
    };
    let boot_disk = TestDisk::new(backing).await?;
    let hot_disk = TestDisk::new(backing).await?;
    let initial_read_only = matches!(backing, DiskBacking::Vmdk);
    let boot_payload = test_pattern(0x39);
    let payload = test_pattern(0x7b);
    if initial_read_only {
        boot_disk.seed_backing(&boot_payload)?;
        hot_disk.seed_backing(&payload)?;
    }
    let initial_resource = VirtioPciDeviceHandle(
        VirtioBlkHandle {
            disk: boot_disk.resource(initial_read_only).await?,
            read_only: initial_read_only,
            serial: Some(BOOT_SERIAL.into()),
        }
        .into_resource(),
    )
    .into_resource();
    let (mut vm, agent) = config
        .with_virtio_vsock(true)
        .with_processor_topology(ProcessorTopology {
            vp_count: 2,
            vps_per_socket: Some(2),
            ..Default::default()
        })
        .with_memory(MemoryConfig {
            startup_bytes: 256 * petri::SIZE_1_MB,
            transparent_hugepages: !isolated,
            ..Default::default()
        })
        .modify_backend(move |backend| {
            backend
                .with_pcie_root_topology(1, 1, 3)
                .with_custom_config(move |config| {
                    config.pcie_ecam_below_4gb = true;
                    if !isolated {
                        config.hypervisor.force_guest_memory_bounce = true;
                    }
                    config.pcie_devices.push(PcieDeviceConfig {
                        port_name: BOOT_PORT.into(),
                        resource: initial_resource,
                    });
                })
        })
        .run()
        .await?;

    let result = async {
        if isolated {
            let shell = agent.unix_shell();
            let dmesg = cmd!(shell, "dmesg").read().await?;
            anyhow::ensure!(
                dmesg.contains("SEV: Status: SEV SEV-ES SEV-SNP"),
                "guest did not report active SEV-SNP"
            );
        }
        wait_for_disk(&agent, BOOT_SERIAL, true, &driver).await?;
        verify_io(&agent, BOOT_SERIAL, &boot_payload, initial_read_only).await?;
        boot_disk.verify_backing(&boot_payload)?;

        wait_for_disk(&agent, HOT_SERIAL, false, &driver).await?;
        let resource = VirtioPciDeviceHandle(
            VirtioBlkHandle {
                disk: hot_disk.resource(initial_read_only).await?,
                read_only: initial_read_only,
                serial: Some(HOT_SERIAL.into()),
            }
            .into_resource(),
        )
        .into_resource();
        vm.add_pcie_device(HOT_PORT.into(), resource).await?;
        wait_for_disk(&agent, HOT_SERIAL, true, &driver).await?;
        verify_io(&agent, HOT_SERIAL, &payload, initial_read_only).await?;
        hot_disk.verify_backing(&payload)?;

        let pci_path = quiesce_disk(&agent, HOT_SERIAL).await?;
        vm.remove_pcie_device(HOT_PORT.into()).await?;
        wait_for_disk(&agent, HOT_SERIAL, false, &driver).await?;
        wait_for_pci_removal(&agent, &pci_path, &driver).await?;
        hot_disk.verify_backing(&payload)?;
        verify_payload(&agent, BOOT_SERIAL, &boot_payload).await?;
        boot_disk.verify_backing(&boot_payload)?;

        // A newly created RAM backend has no persistence contract.
        let read_only = !matches!(backing, DiskBacking::Ram);
        let resource = VirtioPciDeviceHandle(
            VirtioBlkHandle {
                disk: hot_disk.resource(read_only).await?,
                read_only,
                serial: Some(HOT_SERIAL.into()),
            }
            .into_resource(),
        )
        .into_resource();
        vm.add_pcie_device(HOT_PORT.into(), resource).await?;
        wait_for_disk(&agent, HOT_SERIAL, true, &driver).await?;
        let readded_payload = if read_only {
            payload
        } else {
            test_pattern(0xc7)
        };
        verify_io(&agent, HOT_SERIAL, &readded_payload, read_only).await?;
        hot_disk.verify_backing(&readded_payload)?;
        verify_payload(&agent, BOOT_SERIAL, &boot_payload).await?;
        boot_disk.verify_backing(&boot_payload)?;
        agent.power_off().await?;
        vm.wait_for_clean_shutdown().await?;
        anyhow::Ok(())
    }
    .await;
    // Release every VM/disk handle before detaching our loop device, also on error.
    let teardown = vm.teardown().await;
    let boot_close = boot_disk.close();
    let hot_close = hot_disk.close();
    if let Err(error) = &teardown {
        tracing::error!(?error, "failed to tear down virtio-blk test VM");
    }
    if let Err(error) = &boot_close {
        tracing::error!(?error, "failed to close virtio-blk boot disk");
    }
    if let Err(error) = &hot_close {
        tracing::error!(?error, "failed to close virtio-blk hotplug disk");
    }
    result?;
    teardown?;
    boot_close?;
    hot_close?;
    Ok(())
}
