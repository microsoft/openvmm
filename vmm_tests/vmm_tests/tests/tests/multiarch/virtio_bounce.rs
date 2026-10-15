// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Forced owned-buffer I/O on ordinary guest memory.

use anyhow::Context;
use openvmm_defs::config::PcieDeviceConfig;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use virtio_resources::VirtioPciDeviceHandle;
use virtio_resources::blk::VirtioBlkBounceDiskHandle;
use virtio_resources::blk::VirtioBlkHandle;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;

/// Exercise forced block staging without relying on an isolated-memory policy.
#[openvmm_test(linux_direct_x64)]
async fn virtio_blk_forced_bounce(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    const BLOCK_SIZE: usize = 4096;
    const BLOCK_COUNT: usize = 256;
    let disk = VirtioBlkBounceDiskHandle::Ram {
        len: 16 * 1024 * 1024,
    }
    .into_resource();
    let (vm, agent) = config
        .with_virtio_vsock()
        .modify_backend(move |backend| {
            backend
                .with_pcie_root_topology(1, 1, 2)
                .with_custom_config(move |config| {
                    config.pcie_devices.push(PcieDeviceConfig {
                        port_name: "s0rc0rp1".into(),
                        resource: VirtioPciDeviceHandle(
                            VirtioBlkHandle {
                                disk,
                                read_only: false,
                                serial: Some("FORCED-BOUNCE".into()),
                                bounce_io: true,
                                bounce_disk: None,
                            }
                            .into_resource(),
                        )
                        .into_resource(),
                    });
                })
        })
        .run()
        .await?;

    let shell = agent.unix_shell();
    let serial = cmd!(shell, "cat /sys/block/vda/serial").read().await?;
    assert_eq!(serial.trim(), "FORCED-BOUNCE");
    let pattern: Vec<u8> = (0..BLOCK_SIZE * BLOCK_COUNT)
        .map(|index| (index.wrapping_mul(37) ^ (index / BLOCK_SIZE)) as u8)
        .collect();
    agent
        .write_file("/tmp/forced-bounce-input", pattern.as_slice())
        .await?;
    cmd!(
        shell,
        "dd if=/tmp/forced-bounce-input of=/dev/vda bs=4096 seek=8 oflag=direct conv=notrunc"
    )
    .run()
    .await
    .context("write through forced virtio-blk staging")?;
    cmd!(
        shell,
        "dd if=/dev/vda of=/tmp/forced-bounce-output bs=4096 skip=8 count=256 iflag=direct"
    )
    .run()
    .await
    .context("read through forced virtio-blk staging")?;
    assert_eq!(
        agent.read_file("/tmp/forced-bounce-output").await?,
        pattern,
        "forced virtio-blk I/O changed the payload"
    );
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
