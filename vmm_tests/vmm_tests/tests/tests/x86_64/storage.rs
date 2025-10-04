// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests that focus on OpenHCL storage scenarios.
//! These tests require OpenHCL and a Linux guest.
//! They also require VTL2 support in OpenHCL, which is currently only available
//! on x86-64.

use anyhow::Context;
use disk_backend_resources::FileDiskHandle;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::DiskLayerHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use guid::Guid;
use hvlite_defs::config::DeviceVtl;
use hvlite_defs::config::VpciDeviceConfig;
use mesh::rpc::RpcSend;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri::pipette::cmd;
use petri::vtl2_settings::ControllerType;
use petri::vtl2_settings::Vtl2LunBuilder;
use petri::vtl2_settings::Vtl2StorageBackingDeviceBuilder;
use petri::vtl2_settings::Vtl2StorageControllerBuilder;
use petri::vtl2_settings::build_vtl2_storage_backing_physical_devices;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use scsidisk_resources::SimpleScsiDvdRequest;
use std::fs::File;
use std::io::Write;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;

/// Create a VPCI device config for an NVMe controller assigned to VTL2, with a single namespace.
/// The namespace will be backed by either a file or a ramdisk, depending on whether
/// `backing_file` is `Some` or `None`.
pub(crate) fn new_test_vtl2_nvme_device(
    nsid: u32,
    size: u64,
    instance_id: Guid,
    backing_file: Option<File>,
) -> VpciDeviceConfig {
    let layer = if let Some(file) = backing_file {
        LayeredDiskHandle::single_layer(DiskLayerHandle(FileDiskHandle(file).into_resource()))
    } else {
        LayeredDiskHandle::single_layer(RamDiskLayerHandle { len: Some(size) })
    };

    VpciDeviceConfig {
        vtl: DeviceVtl::Vtl2,
        instance_id,
        resource: NvmeControllerHandle {
            subsystem_id: instance_id,
            max_io_queues: 64,
            msix_count: 64,
            namespaces: vec![NamespaceDefinition {
                nsid,
                disk: layer.into_resource(),
                read_only: false,
            }],
        }
        .into_resource(),
    }
}

#[derive(Debug, Clone)]
struct ExpectedGuestDevice {
    disk_size_sectors: usize,
    #[expect(dead_code)] // Only used in logging via `Debug` trait
    friendly_name: String,
}

async fn test_storage_unix(
    agent: &PipetteClient,
    expected_devices: Vec<ExpectedGuestDevice>,
) -> anyhow::Result<()> {
    #[derive(Debug)]
    // Helper struct for things this test discovers about the device...
    struct ExpectedGuestDeviceData {
        #[expect(dead_code)] // Only used in logging via `Debug` trait
        d: ExpectedGuestDevice,
        discovered_guest_path: String,
    }

    let sh = agent.unix_shell();

    // Check that the correct devices are found in the VTL0 guest.
    // The test framework adds additional devices (pipette, cloud-init, etc), so
    // just check that the expected devices are indeed found.
    //
    // TODO: Verify VMBUS instance ID, LUN, etc.
    let devices = cmd!(sh, "sh -c 'ls -d /sys/block/sd*'").read().await?;

    let mut reported_sizes = Vec::new();
    for device in devices.lines() {
        let device_info_command =
            format!("echo /dev/$(basename {device}) $(cat /sys/block/$(basename {device})/size)");
        let line = cmd!(sh, "sh -c {device_info_command}")
            .read()
            .await?
            .split_ascii_whitespace()
            .map(|x| x.to_string())
            .collect::<Vec<_>>();

        let size = line[1].parse::<usize>().context("failed to parse size")?;

        reported_sizes.push((line[0].clone(), size));
    }

    let found_devices = expected_devices
        .iter()
        .map(|d| {
            match reported_sizes
                .iter()
                .position(|(_device, sectors)| *sectors == d.disk_size_sectors)
            {
                Some(idx) => ExpectedGuestDeviceData {
                    d: d.clone(),
                    discovered_guest_path: reported_sizes[idx].0.clone(),
                },
                None => {
                    // In the future, this could be more elegant by returning `Result` here and
                    // using `try_collect` instead of `collect` below. For now, just bail
                    // on the first non-found device (if any).
                    //
                    // See https://doc.rust-lang.org/std/iter/trait.Iterator.html#method.try_collect
                    tracing::error!(?d, "couldn't find drive with expected sector count");
                    panic!("couldn't find drive with expected sector count");
                }
            }
        })
        .collect::<Vec<_>>();

    let identical_devices: Vec<(&ExpectedGuestDeviceData, &ExpectedGuestDeviceData)> =
        found_devices
            .iter()
            .zip(found_devices.iter().skip(1))
            .filter(|(left, right)| left.discovered_guest_path == right.discovered_guest_path)
            .collect();
    if !identical_devices.is_empty() {
        tracing::error!(?identical_devices, "found identical devices");
        assert!(identical_devices.is_empty());
    }

    // Do IO to all devices. Generate a file with random contents so that we
    // can verify that the writes (and reads) work correctly.
    //
    // - `{o,i}flag=direct` is needed to ensure that the IO is not served
    //   from the guest's cache.
    // - `conv=fsync` is needed to ensure that the write is flushed to the
    //    device before `dd` exits.
    // - `iflag=fullblock` is needed to ensure that `dd` reads the full
    //   amount of data requested, otherwise it may read less and exit
    //   early.
    for device in found_devices {
        tracing::info!(?device, "Performing IO tests");
        cmd!(
            sh,
            "sh -c 'dd if=/dev/urandom of=/tmp/random_data bs=1M count=100'"
        )
        .run()
        .await?;

        let write_to_device_cmd = format!(
            "dd if=/tmp/random_data of={} bs=1M count=100 oflag=direct conv=fsync",
            device.discovered_guest_path
        );
        cmd!(sh, "sh -c {write_to_device_cmd}").run().await?;

        let read_from_device_cmd = format!(
            "dd if={} of=/tmp/verify_data bs=1M count=100 iflag=direct,fullblock",
            device.discovered_guest_path
        );
        cmd!(sh, "sh -c {read_from_device_cmd}").run().await?;

        let diff_out = cmd!(sh, "sh -c 'diff -s /tmp/random_data /tmp/verify_data'")
            .read()
            .await?;
        assert!(diff_out.contains("are identical"), "data mismatch");

        cmd!(sh, "rm -f /tmp/random_data /tmp/verify_data")
            .run()
            .await?;
    }

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a SCSI disk assigned to VTL2, an NVMe disk assigned to VTL2, and
/// vmbus relay. This should expose two disks to VTL0 via vmbus.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn storvsp(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    let vtl2_lun = 5;
    let vtl0_scsi_lun = 0;
    let vtl0_nvme_lun = 1;
    let vtl2_nsid = 37;
    let scsi_instance = Guid::new_random();
    const SCSI_DISK_SECTORS: u64 = 0x4_0000;
    const NVME_DISK_SECTORS: u64 = 0x5_0000;
    const SECTOR_SIZE: u64 = 512;
    const EXPECTED_SCSI_DISK_SIZE_BYTES: u64 = SCSI_DISK_SECTORS * SECTOR_SIZE;
    const EXPECTED_NVME_DISK_SIZE_BYTES: u64 = NVME_DISK_SECTORS * SECTOR_SIZE;

    // Assumptions made by test infra & routines:
    //
    // 1. Some test-infra added disks are 64MiB in size. Since we find disks by size,
    // ensure that our test disks are a different size.
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_unix`]),
    // with some arbitrary buffer (5MiB in this case).
    static_assertions::const_assert_ne!(EXPECTED_SCSI_DISK_SIZE_BYTES, 64 * 1024 * 1024);
    static_assertions::const_assert!(EXPECTED_SCSI_DISK_SIZE_BYTES > 105 * 1024 * 1024);
    static_assertions::const_assert_ne!(EXPECTED_NVME_DISK_SIZE_BYTES, 64 * 1024 * 1024);
    static_assertions::const_assert!(EXPECTED_NVME_DISK_SIZE_BYTES > 105 * 1024 * 1024);

    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vmbus_devices.push((
                    DeviceVtl::Vtl2,
                    ScsiControllerHandle {
                        instance_id: scsi_instance,
                        max_sub_channel_count: 1,
                        devices: vec![ScsiDeviceAndPath {
                            path: ScsiPath {
                                path: 0,
                                target: 0,
                                lun: vtl2_lun as u8,
                            },
                            device: SimpleScsiDiskHandle {
                                disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                    len: Some(SCSI_DISK_SECTORS * SECTOR_SIZE),
                                })
                                .into_resource(),
                                read_only: false,
                                parameters: Default::default(),
                            }
                            .into_resource(),
                        }],
                        io_queue_depth: None,
                        requests: None,
                        poll_mode_queue_depth: None,
                    }
                    .into_resource(),
                ));
                c.vpci_devices.push(new_test_vtl2_nvme_device(
                    vtl2_nsid,
                    NVME_DISK_SECTORS * SECTOR_SIZE,
                    NVME_INSTANCE,
                    None,
                ));
            })
            .with_custom_vtl2_settings(|v| {
                v.dynamic.as_mut().unwrap().storage_controllers.push(
                    Vtl2StorageControllerBuilder::scsi()
                        .with_instance_id(scsi_instance)
                        .with_protocol(ControllerType::Scsi)
                        .add_lun(
                            Vtl2LunBuilder::disk()
                                .with_location(vtl0_scsi_lun)
                                .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                                    ControllerType::Scsi,
                                    scsi_instance,
                                    vtl2_lun,
                                )),
                        )
                        .add_lun(
                            Vtl2LunBuilder::disk()
                                .with_location(vtl0_nvme_lun)
                                .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                                    ControllerType::Nvme,
                                    NVME_INSTANCE,
                                    vtl2_nsid,
                                )),
                        )
                        .build(),
                )
            })
        })
        .run()
        .await?;

    test_storage_unix(
        &agent,
        vec![
            ExpectedGuestDevice {
                disk_size_sectors: SCSI_DISK_SECTORS as usize,
                friendly_name: "scsi".to_string(),
            },
            ExpectedGuestDevice {
                disk_size_sectors: NVME_DISK_SECTORS as usize,
                friendly_name: "nvme".to_string(),
            },
        ],
    )
    .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux Stripe VM with two SCSI disk assigned to VTL2 via NVMe Emulator
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn openhcl_linux_stripe_storvsp(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE_1: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    const NVME_INSTANCE_2: Guid = guid::guid!("06a97a09-d5ad-4689-b638-9419d7346a68");
    let vtl0_nvme_lun = 0;
    let vtl2_nsid = 1;
    const NVME_DISK_SECTORS: u64 = 0x2_0000;
    const SECTOR_SIZE: u64 = 512;
    const NUMBER_OF_STRIPE_DEVICES: u64 = 2;
    const EXPECTED_STRIPED_DISK_SIZE_SECTORS: u64 = NVME_DISK_SECTORS * NUMBER_OF_STRIPE_DEVICES;
    const EXPECTED_STRIPED_DISK_SIZE_BYTES: u64 = EXPECTED_STRIPED_DISK_SIZE_SECTORS * SECTOR_SIZE;
    let scsi_instance = Guid::new_random();

    // Assumptions made by test infra & routines:
    //
    // 1. Some test-infra added disks are 64MiB in size. Since we find disks by size,
    // ensure that our test disks are a different size.
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_unix`]),
    // with some arbitrary buffer (5MiB in this case).
    static_assertions::const_assert_ne!(EXPECTED_STRIPED_DISK_SIZE_BYTES, 64 * 1024 * 1024);
    static_assertions::const_assert!(EXPECTED_STRIPED_DISK_SIZE_BYTES > 105 * 1024 * 1024);

    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vpci_devices.extend([
                    new_test_vtl2_nvme_device(
                        vtl2_nsid,
                        NVME_DISK_SECTORS * SECTOR_SIZE,
                        NVME_INSTANCE_1,
                        None,
                    ),
                    new_test_vtl2_nvme_device(
                        vtl2_nsid,
                        NVME_DISK_SECTORS * SECTOR_SIZE,
                        NVME_INSTANCE_2,
                        None,
                    ),
                ]);
            })
            .with_custom_vtl2_settings(|v| {
                v.dynamic.as_mut().unwrap().storage_controllers.push(
                    Vtl2StorageControllerBuilder::scsi()
                        .with_instance_id(scsi_instance)
                        .with_protocol(ControllerType::Scsi)
                        .add_lun(
                            Vtl2LunBuilder::disk()
                                .with_location(vtl0_nvme_lun)
                                .with_chunk_size_in_kb(128)
                                .with_physical_devices(vec![
                                    Vtl2StorageBackingDeviceBuilder::new(
                                        ControllerType::Nvme,
                                        NVME_INSTANCE_1,
                                        vtl2_nsid,
                                    ),
                                    Vtl2StorageBackingDeviceBuilder::new(
                                        ControllerType::Nvme,
                                        NVME_INSTANCE_2,
                                        vtl2_nsid,
                                    ),
                                ]),
                        )
                        .build(),
                )
            })
        })
        .run()
        .await?;

    test_storage_unix(
        &agent,
        vec![ExpectedGuestDevice {
            disk_size_sectors: (NVME_DISK_SECTORS * NUMBER_OF_STRIPE_DEVICES) as usize,
            friendly_name: "striped-nvme".to_string(),
        }],
    )
    .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a SCSI DVD assigned to VTL2, and vmbus
/// relay. This should expose a DVD to VTL0 via vmbus. Start with an empty
/// drive, then add and remove media.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn openhcl_linux_storvsp_dvd(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let vtl2_lun = 5;
    let vtl0_scsi_lun = 0;
    let scsi_instance = Guid::new_random();

    let (hot_plug_send, hot_plug_recv) = mesh::channel();

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vmbus_devices.push((
                    DeviceVtl::Vtl2,
                    ScsiControllerHandle {
                        instance_id: scsi_instance,
                        max_sub_channel_count: 1,
                        devices: vec![ScsiDeviceAndPath {
                            path: ScsiPath {
                                path: 0,
                                target: 0,
                                lun: vtl2_lun as u8,
                            },
                            device: SimpleScsiDvdHandle {
                                media: None,
                                requests: Some(hot_plug_recv),
                            }
                            .into_resource(),
                        }],
                        io_queue_depth: None,
                        requests: None,
                        poll_mode_queue_depth: None,
                    }
                    .into_resource(),
                ));
            })
            .with_custom_vtl2_settings(|v| {
                v.dynamic.as_mut().unwrap().storage_controllers.push(
                    Vtl2StorageControllerBuilder::scsi()
                        .with_instance_id(scsi_instance)
                        .add_lun(Vtl2LunBuilder::dvd().with_location(vtl0_scsi_lun))
                        // No physical devices initially, so the drive is empty
                        .build(),
                )
            })
        })
        .run()
        .await?;

    let read_drive = || agent.read_file("/dev/sr0");

    let ensure_no_medium = |r: anyhow::Result<_>| {
        match r {
            Ok(_) => anyhow::bail!("expected error reading from dvd drive"),
            Err(e) => {
                let e = format!("{:#}", e);
                if !e.contains("No medium found") {
                    anyhow::bail!("unexpected error reading from dvd drive: {e}");
                }
            }
        }
        Ok(())
    };

    // Initially no media.
    ensure_no_medium(read_drive().await)?;

    let len = 0x42000;

    hot_plug_send
        .call_failable(
            SimpleScsiDvdRequest::ChangeMedia,
            Some(
                LayeredDiskHandle::single_layer(RamDiskLayerHandle { len: Some(len) })
                    .into_resource(),
            ),
        )
        .await
        .context("failed to change media")?;

    vm.backend()
        .modify_vtl2_settings(|v| {
            v.dynamic.as_mut().unwrap().storage_controllers[0].luns[0].physical_devices =
                build_vtl2_storage_backing_physical_devices(vec![
                    Vtl2StorageBackingDeviceBuilder::new(
                        ControllerType::Scsi,
                        scsi_instance,
                        vtl2_lun,
                    ),
                ])
        })
        .await
        .context("failed to modify vtl2 settings")?;

    let b = read_drive().await.context("failed to read dvd drive")?;
    assert_eq!(
        b.len() as u64,
        len,
        "expected {} bytes, got {}",
        len,
        b.len()
    );

    // Remove media.
    vm.backend()
        .modify_vtl2_settings(|v| {
            v.dynamic.as_mut().unwrap().storage_controllers[0].luns[0].physical_devices =
                build_vtl2_storage_backing_physical_devices(vec![])
        })
        .await
        .context("failed to modify vtl2 settings")?;

    ensure_no_medium(read_drive().await)?;

    hot_plug_send
        .call_failable(SimpleScsiDvdRequest::ChangeMedia, None)
        .await
        .context("failed to change media")?;

    agent.power_off().await?;
    drop(hot_plug_send);
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a SCSI DVD assigned to VTL2, using NVMe
/// backing, and vmbus relay. This should expose a DVD to VTL0 via vmbus.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn openhcl_linux_storvsp_dvd_nvme(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    let vtl2_nsid = 1;
    let nvme_disk_sectors: u64 = 0x4000;
    let sector_size = 4096;

    let vtl2_lun = 5;
    let scsi_instance = Guid::new_random();

    let disk_len = nvme_disk_sectors * sector_size;
    let mut backing_file = tempfile::tempfile()?;
    let data_chunk: Vec<u8> = (0..64).collect();
    let data_chunk = data_chunk.as_slice();
    let mut bytes = vec![0_u8; disk_len as usize];
    bytes.chunks_exact_mut(64).for_each(|v| {
        v.copy_from_slice(data_chunk);
    });
    backing_file.write_all(&bytes)?;

    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vpci_devices.extend([new_test_vtl2_nvme_device(
                    vtl2_nsid,
                    disk_len,
                    NVME_INSTANCE,
                    Some(backing_file),
                )]);
            })
            .with_custom_vtl2_settings(|v| {
                v.dynamic.as_mut().unwrap().storage_controllers.push(
                    Vtl2StorageControllerBuilder::scsi()
                        .with_instance_id(scsi_instance)
                        .with_protocol(ControllerType::Scsi)
                        .add_lun(
                            Vtl2LunBuilder::dvd()
                                .with_location(vtl2_lun)
                                .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                                    ControllerType::Nvme,
                                    NVME_INSTANCE,
                                    vtl2_nsid,
                                )),
                        )
                        .build(),
                );
            })
        })
        .run()
        .await?;

    let b = agent
        .read_file("/dev/sr0")
        .await
        .context("failed to read dvd drive")?;
    assert_eq!(
        b.len() as u64,
        disk_len,
        "expected {} bytes, got {}",
        disk_len,
        b.len()
    );
    assert_eq!(b[..], bytes[..], "content mismatch");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
