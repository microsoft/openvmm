// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 guests.

mod openhcl_linux_direct;
mod openhcl_uefi;
mod storage;

use anyhow::Context;
use guid::Guid;
use mesh::CellUpdater;
use net_backend_resources::mac_address::MacAddress;
use net_backend_resources::null::NullHandle;
use nvme_resources::NvmeControllerHandle;
use nvme_resources::NvmeFaultControllerHandle;
use nvme_resources::fault::FaultConfiguration;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::VpciDeviceConfig;
use petri::ApicMode;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::OsFlavor;
use virtio_resources::VirtioPciDeviceHandle;
use virtio_resources::net::VirtioNetHandle;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_with;

/// Basic boot test with the VTL 0 alias map.
// TODO: Remove once #73 is fixed.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn boot_alias_map(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| b.with_vtl0_alias_map())
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot with a battery and check the OS-reported capacity.
#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2504_server_x64)),
    uefi_x64(vhd(windows_datacenter_core_2022_x64))
)]
async fn battery_capacity(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config.modify_backend(|b| b.with_battery()).run().await?;

    let output = match os_flavor {
        OsFlavor::Linux => {
            let sh = agent.unix_shell();
            cmd!(
                sh,
                "grep POWER_SUPPLY_CAPACITY= /sys/class/power_supply/BAT1/uevent"
            )
            .read()
            .await?
            .replace("POWER_SUPPLY_CAPACITY=", "")
        }
        OsFlavor::Windows => {
            let sh = agent.windows_shell();
            cmd!(
                sh,
                "powershell.exe -NoExit -Command (Get-WmiObject Win32_Battery).EstimatedChargeRemaining"
            )
            .read()
            .await?
            .replace("\r\nPS C:\\>", "")
            .trim()
            .to_string()
        }
        _ => unreachable!(),
    };

    let guest_capacity: i32 = output.parse().expect("Failed to parse battery capacity");
    assert_eq!(guest_capacity, 95, "Output did not match expected capacity");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

fn configure_for_sidecar<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    proc_count: u32,
    node_count: u32,
) -> PetriVmBuilder<T> {
    config.with_processor_topology({
        ProcessorTopology {
            vp_count: proc_count,
            vps_per_socket: Some(proc_count / node_count),
            enable_smt: Some(false),
            // Sidecar currently requires x2APIC.
            apic_mode: Some(ApicMode::X2apicSupported),
        }
    })
}

// Use UEFI so that the guest doesn't access the other APs, causing hot adds
// into VTL2 Linux.
//
// Sidecar isn't supported on aarch64 yet.
#[vmm_test_with(noagent(openvmm_openhcl_uefi_x64(none), hyperv_openhcl_uefi_x64(none)))]
async fn sidecar_aps_unused<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> Result<(), anyhow::Error> {
    let proc_count = 4;
    let mut vm = configure_for_sidecar(config, proc_count, 1)
        .with_uefi_frontpage(true)
        .run_without_agent()
        .await?;

    let agent = vm.wait_for_vtl2_agent().await?;
    let sh = agent.unix_shell();

    // Ensure the APs haven't been started into Linux.
    //
    // CPU 0 doesn't usually have an online file on x86_64.
    for cpu in 1..proc_count {
        let online = sh
            .read_file(format!("/sys/bus/cpu/devices/cpu{cpu}/online"))
            .await?
            .trim()
            .parse::<u8>()
            .context("failed to parse online file")?
            != 0;
        assert!(!online, "cpu {cpu} is online");
    }

    // No way to shut down cleanly, currently.
    tracing::info!("dropping VM");
    Ok(())
}

#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn sidecar_boot<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> Result<(), anyhow::Error> {
    let (vm, agent) = configure_for_sidecar(config, 8, 2).run().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[openvmm_test(openhcl_linux_direct_x64)]
async fn vpci_filter(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let nvme_guid = guid::guid!("78fc4861-29bf-408d-88b7-24199de560d1");
    let virtio_guid = guid::guid!("382a9da7-a7d8-44a5-9644-be3785bceda6");

    // Add an NVMe controller and a Virtio network controller. Only the NVMe
    // controller should be allowed by OpenHCL.
    let (vm, agent) = config
        .with_openhcl_command_line("OPENHCL_ENABLE_VPCI_RELAY=1")
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vpci_devices.extend([
                    VpciDeviceConfig {
                        vtl: DeviceVtl::Vtl0,
                        instance_id: nvme_guid,
                        resource: NvmeControllerHandle {
                            subsystem_id: nvme_guid,
                            msix_count: 1,
                            max_io_queues: 1,
                            namespaces: Vec::new(),
                            requests: None,
                        }
                        .into_resource(),
                    },
                    VpciDeviceConfig {
                        vtl: DeviceVtl::Vtl0,
                        instance_id: virtio_guid,
                        resource: VirtioPciDeviceHandle(
                            VirtioNetHandle {
                                max_queues: None,
                                mac_address: MacAddress::new([0x00, 0x15, 0x5D, 0x12, 0x12, 0x12]),
                                endpoint: NullHandle.into_resource(),
                            }
                            .into_resource(),
                        )
                        .into_resource(),
                    },
                ])
            })
        })
        .run()
        .await?;

    let sh = agent.unix_shell();
    let lspci_output = cmd!(sh, "lspci").read().await?;
    let devices = lspci_output
        .lines()
        .map(|line| line.trim().split_once(' ').ok_or_else(|| line.trim()))
        .collect::<Vec<_>>();

    // The virtio device should not have made it through, but the NVMe
    // controller should be there.
    assert_eq!(devices, vec![Ok(("00:00.0", "Class 0108: 1414:00a9"))]);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[openvmm_test(openhcl_linux_direct_x64)]
async fn vpci_relay_tdisp_device(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");

    // Create a VPCI device to relay to VTL0 and run basic TDISP end-to-end
    // tests on it.
    let (vm, agent) = config
        .with_openhcl_command_line("OPENHCL_ENABLE_VPCI_RELAY=1")
        // Tells VPCI relay that it should take the device through a mock TDISP
        // flow with the OpenVMM host.
        .with_openhcl_command_line("OPENHCL_TEST_CONFIG=TDISP_VPCI_FLOW_TEST")
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vpci_devices.extend([VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl0,
                    instance_id: NVME_INSTANCE,

                    // The NVMe fault controller device is a fake NVMe
                    // controller that is repurposed for use in the TDISP test
                    // flow.
                    resource: NvmeFaultControllerHandle {
                        subsystem_id: Guid::new_random(),
                        msix_count: 1,
                        max_io_queues: 1,
                        namespaces: Vec::new(),
                        fault_config: FaultConfiguration::new(CellUpdater::new(false).cell()),
                        enable_tdisp_tests: true,
                    }
                    .into_resource(),
                }])
            })
        })
        .run()
        .await?;

    let sh = agent.unix_shell();
    let lspci_output = cmd!(sh, "lspci").read().await?;
    let devices = lspci_output
        .lines()
        .map(|line| line.trim().split_once(' ').ok_or_else(|| line.trim()))
        .collect::<Vec<_>>();

    // The NVMe controller should be present after the HCL performs its TDISP test.
    assert_eq!(devices, vec![Ok(("00:00.0", "Class 0108: 1414:00a9"))]);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot with a virtio-blk disk via virtio-mmio and verify the device appears in the guest.
#[openvmm_test(linux_direct_x64)]
async fn virtio_blk_device(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    use disk_backend_resources::LayeredDiskHandle;
    use disk_backend_resources::layer::RamDiskLayerHandle;
    use openvmm_defs::config::VirtioBus;
    use virtio_resources::blk::VirtioBlkHandle;

    let disk_size: u64 = 8 * 1024 * 1024; // 8 MiB
    let disk_resource = LayeredDiskHandle::single_layer(RamDiskLayerHandle {
        len: Some(disk_size),
        sector_size: None,
    })
    .into_resource();

    let (mut vm, agent) = config
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.virtio_devices.push((
                    VirtioBus::Mmio,
                    VirtioBlkHandle {
                        disk: disk_resource,
                        read_only: false,
                    }
                    .into_resource(),
                ));
            })
        })
        .run()
        .await?;

    let sh = agent.unix_shell();

    // Verify virtio-blk device appears as /dev/vda via sysfs
    let vda_size = cmd!(sh, "cat /sys/block/vda/size")
        .read()
        .await
        .context("virtio-blk device /dev/vda not found")?;
    let vda_sectors: u64 = vda_size.trim().parse().context("parse vda size")?;
    let expected_sectors = disk_size / 512;
    assert_eq!(
        vda_sectors, expected_sectors,
        "unexpected disk size in sectors"
    );

    // Verify we can write and read back data
    cmd!(
        sh,
        "sh -c 'echo hello_virtio_blk | dd of=/dev/vda bs=512 count=1 conv=notrunc 2>/dev/null'"
    )
    .read()
    .await
    .context("write to virtio-blk device")?;
    let readback = cmd!(
        sh,
        "sh -c 'dd if=/dev/vda bs=512 count=1 2>/dev/null | head -c 16'"
    )
    .read()
    .await
    .context("read from virtio-blk device")?;
    assert!(
        readback.starts_with("hello_virtio_blk"),
        "read back data mismatch: {readback}"
    );

    // Pulse save/restore with the device active and data on disk.
    // Drop the old agent — its vsock connection won't survive the pulse.
    drop(agent);
    vm.backend().verify_save_restore().await?;

    // Pipette automatically reconnects after the pulse. Accept the new connection.
    let agent = vm.backend().wait_for_agent(false).await?;
    let sh = agent.unix_shell();

    // Verify the device still works after save/restore.
    // Use iflag=direct to bypass the page cache and force a real device read.
    let readback = cmd!(
        sh,
        "sh -c 'dd if=/dev/vda iflag=direct bs=512 count=1 2>/dev/null | head -c 16'"
    )
    .read()
    .await
    .context("read from virtio-blk after save/restore")?;
    assert!(
        readback.starts_with("hello_virtio_blk"),
        "data mismatch after save/restore: {readback}"
    );

    // Write new data after restore to confirm writes work too.
    cmd!(
        sh,
        "sh -c 'echo post_restore_ok | dd of=/dev/vda oflag=direct bs=512 count=1 conv=sync,notrunc 2>/dev/null'"
    )
    .read()
    .await
    .context("write to virtio-blk after save/restore")?;
    let readback = cmd!(
        sh,
        "sh -c 'dd if=/dev/vda iflag=direct bs=512 count=1 2>/dev/null | head -c 15'"
    )
    .read()
    .await
    .context("read new data after save/restore")?;
    assert!(
        readback.starts_with("post_restore_ok"),
        "post-restore write/read mismatch: {readback}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot with a virtio-rng device via virtio-mmio and verify the guest can read entropy.
#[openvmm_test(linux_direct_x64)]
async fn virtio_rng_device(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    use openvmm_defs::config::VirtioBus;
    use virtio_resources::rng::VirtioRngHandle;

    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                c.virtio_devices
                    .push((VirtioBus::Mmio, VirtioRngHandle.into_resource()));
            })
        })
        .run()
        .await?;

    let sh = agent.unix_shell();

    // Fail fast if the virtio-rng driver isn't available in the guest kernel
    cmd!(sh, "test -e /dev/hwrng")
        .run()
        .await
        .context("/dev/hwrng not found — guest kernel may lack CONFIG_HW_RANDOM_VIRTIO")?;

    // Verify virtio-rng driver bound to the device
    let rng_current = cmd!(sh, "cat /sys/class/misc/hw_random/rng_current")
        .read()
        .await
        .context("failed to read rng_current")?;
    let rng_current = rng_current.trim();
    assert!(
        rng_current.starts_with("virtio_rng"),
        "expected virtio_rng as current hwrng, got {rng_current:?}"
    );

    // Read 64 bytes of entropy with a timeout to avoid hanging if the device is broken
    let read_entropy = async {
        cmd!(
            sh,
            "sh -c 'dd if=/dev/hwrng bs=64 count=1 2>/dev/null | od -A n -t x1 | tr -d \" \\n\"'"
        )
        .read()
        .await
    };
    let hex_output = mesh::CancelContext::new()
        .with_timeout(std::time::Duration::from_secs(10))
        .until_cancelled(read_entropy)
        .await
        .context("timed out reading from /dev/hwrng — device may be broken")?
        .context("failed to read from /dev/hwrng")?;
    let hex = hex_output.trim();
    assert_eq!(
        hex.len(),
        128,
        "expected 128 hex chars (64 bytes), got {}",
        hex.len()
    );
    assert_ne!(
        hex,
        "0".repeat(128),
        "hwrng returned all zeros — device not producing entropy"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot Linux with guest memory backed by a file instead of anonymous RAM.
///
/// This validates that the file-backed memory plumbing through petri works
/// end-to-end: the VM should boot normally, and the backing file should
/// exist and be non-empty after boot.
#[openvmm_test(linux_direct_x64)]
async fn file_backed_memory_boot(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let mem_dir = tempfile::tempdir().expect("failed to create temp dir");
    let mem_path: std::path::PathBuf = mem_dir.path().join("memory.bin");

    let (vm, agent) = config
        .modify_backend({
            let mem_path = mem_path.clone();
            move |b| b.with_memory_backing_file(mem_path)
        })
        .run()
        .await?;

    // Verify the backing file was created and is non-empty.
    let metadata = std::fs::metadata(&mem_path).expect("memory backing file should exist");
    assert!(
        metadata.len() > 0,
        "memory backing file should be non-empty"
    );

    agent.ping().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Boot with file-backed memory, pause + save VM state, write the snapshot
/// artifacts to disk, read them back to verify the roundtrip, then resume
/// the VM and confirm it is still functional.
///
/// This exercises the full save-to-disk path with real VM state and validates
/// that the serialized state bytes survive a disk roundtrip unchanged.
#[openvmm_test(linux_direct_x64)]
async fn snapshot_save_to_disk(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let work_dir = tempfile::tempdir().expect("failed to create temp dir");
    let mem_path: std::path::PathBuf = work_dir.path().join("memory.bin");
    let snap_dir = work_dir.path().join("snapshot");

    let (mut vm, agent) = config
        .modify_backend({
            let mem_path = mem_path.clone();
            move |b| b.with_memory_backing_file(mem_path)
        })
        .run()
        .await?;

    // Verify the guest is functional before saving.
    agent.ping().await?;

    // Pause the VM.
    vm.backend().pause().await?;

    // Save device + processor state.
    let saved_state_bytes = vm.backend().save_state().await?;
    assert!(
        !saved_state_bytes.is_empty(),
        "saved state should be non-empty"
    );

    // Get the size of the memory backing file. The VM is paused so dirty
    // pages have already been flushed by the hypervisor.
    let mem_size = std::fs::metadata(&mem_path)?.len();
    assert!(mem_size > 0, "memory file should be non-empty");

    // Build manifest and write snapshot to disk.
    //
    // vp_count and page_size are hardcoded to match the petri test defaults.
    // If those defaults change, update these values accordingly.
    let manifest = openvmm_helpers::snapshot::SnapshotManifest {
        version: openvmm_helpers::snapshot::MANIFEST_VERSION,
        created_at: std::time::SystemTime::now().into(),
        openvmm_version: env!("CARGO_PKG_VERSION").to_string(),
        memory_size_bytes: mem_size,
        vp_count: 2,
        page_size: 4096,
        architecture: "x86_64".to_string(),
    };
    openvmm_helpers::snapshot::write_snapshot(&snap_dir, &manifest, &saved_state_bytes, &mem_path)?;

    // Verify all snapshot files exist and the saved state roundtrips.
    assert!(snap_dir.join("manifest.bin").exists());
    assert!(snap_dir.join("state.bin").exists());
    assert!(snap_dir.join("memory.bin").exists());
    let (read_manifest, read_state) = openvmm_helpers::snapshot::read_snapshot(&snap_dir)?;
    assert_eq!(
        read_state, saved_state_bytes,
        "state roundtrip through disk should match"
    );
    assert_eq!(read_manifest.memory_size_bytes, mem_size);

    // Resume the VM and verify it is still functional.
    vm.backend().resume().await?;
    agent.ping().await?;

    // Clean shutdown.
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
