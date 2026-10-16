// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Forced owned-buffer I/O on ordinary guest memory.

use anyhow::Context;
use net_backend_resources::consomme::ConsommeHandle;
use net_backend_resources::mac_address::MacAddress;
use openvmm_defs::config::PcieDeviceConfig;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use virtio_resources::VirtioPciDeviceHandle;
use virtio_resources::blk::VirtioBlkBounceDiskHandle;
use virtio_resources::blk::VirtioBlkHandle;
use virtio_resources::net::VirtioNetHandle;
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

/// Verify forced owned TX and RX on a NIC that does not carry test control traffic.
#[openvmm_test(linux_direct_x64)]
async fn virtio_net_forced_bounce(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_virtio_vsock()
        .modify_backend(|backend| {
            backend
                .with_pcie_root_topology(1, 1, 2)
                .with_custom_config(|config| {
                    config.pcie_devices.push(PcieDeviceConfig {
                        port_name: "s0rc0rp1".into(),
                        resource: VirtioPciDeviceHandle(
                            VirtioNetHandle {
                                max_queues: None,
                                mac_address: MacAddress::new([0x00, 0x15, 0x5d, 0x88, 0x00, 0x01]),
                                endpoint: ConsommeHandle {
                                    cidr: None,
                                    ports: Vec::new(),
                                    recv: None,
                                }
                                .into_resource(),
                                bounce_io: true,
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
    let interfaces = cmd!(shell, "ls /sys/class/net").read().await?;
    let mut selected = None;
    for interface in interfaces.split_whitespace() {
        let mac = shell
            .read_file(format!("/sys/class/net/{interface}/address"))
            .await?;
        if mac.trim() == "00:15:5d:88:00:01" {
            anyhow::ensure!(selected.is_none(), "duplicate forced-bounce NIC");
            selected = Some(interface.to_string());
        }
    }
    let interface = selected.context("forced-bounce virtio-net NIC not found")?;
    cmd!(shell, "ip link set {interface} up").run().await?;
    cmd!(shell, "ip address replace 10.0.0.2/24 dev {interface}")
        .run()
        .await?;
    let stats = format!("/sys/class/net/{interface}/statistics");
    let tx_before: u64 = shell
        .read_file(format!("{stats}/tx_bytes"))
        .await?
        .trim()
        .parse()?;
    let rx_before: u64 = shell
        .read_file(format!("{stats}/rx_bytes"))
        .await?
        .trim()
        .parse()?;
    cmd!(shell, "ping -I {interface} -c 8 -W 2 -s 1024 10.0.0.1")
        .run()
        .await
        .context("round-trip payload through forced virtio-net staging")?;
    let tx_after: u64 = shell
        .read_file(format!("{stats}/tx_bytes"))
        .await?
        .trim()
        .parse()?;
    let rx_after: u64 = shell
        .read_file(format!("{stats}/rx_bytes"))
        .await?
        .trim()
        .parse()?;
    assert!(tx_after.saturating_sub(tx_before) >= 8 * 1024);
    assert!(rx_after.saturating_sub(rx_before) >= 8 * 1024);
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Use the forced-bounce vsock transport for verified traffic in both directions.
#[openvmm_test(linux_direct_x64)]
async fn virtio_vsock_forced_bounce(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_virtio_vsock_bounce()
        .modify_backend(|backend| backend.with_pcie_root_topology(1, 1, 1))
        .run()
        .await?;
    for round in 0..2 {
        let payload: Vec<u8> = (0usize..1024 * 1024)
            .map(|index| (index.wrapping_mul(53) ^ (index / 4096) ^ round) as u8)
            .collect();
        agent
            .write_file("/tmp/forced-vsock-payload", payload.as_slice())
            .await?;
        assert_eq!(
            agent.read_file("/tmp/forced-vsock-payload").await?,
            payload,
            "forced vsock transfer changed the payload"
        );
    }
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
