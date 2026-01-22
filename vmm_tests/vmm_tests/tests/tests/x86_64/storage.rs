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
use mesh::rpc::RpcSend;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::VpciDeviceConfig;
use petri::PetriVmBuilder;
#[cfg(windows)]
use petri::hyperv::HyperVPetriBackend;
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
use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use vm_resource::IntoResource;
#[cfg(windows)]
use vmm_test_macros::hyperv_test;
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
    lun: u32,
    disk_size_sectors: usize,
    #[expect(dead_code)] // Only used in logging via `Debug` trait
    friendly_name: String,
}

/// Get the device paths for the expected devices inside the Linux guest,
/// verifying that they exist and have the expected size.
async fn get_device_paths(
    agent: &PipetteClient,
    controller_guid: Guid,
    expected_devices: Vec<ExpectedGuestDevice>,
) -> anyhow::Result<Vec<String>> {
    let sh = agent.unix_shell();

    let all_disks = cmd!(sh, "sh -c 'ls -ld /sys/block/sd*'").read().await?;
    tracing::info!(?all_disks, "All disks");

    // Check that the correct devices are found in the VTL0 guest.
    // The test framework adds additional devices (pipette, cloud-init, etc), so
    // just check that the expected devices are indeed found.
    let mut device_paths = Vec::new();
    for d in &expected_devices {
        let list_sdx_cmd = format!(
            "ls -d /sys/bus/vmbus/devices/{}/host*/target*/*:0:0:{}/block/sd*",
            controller_guid, d.lun
        );
        let devices = cmd!(sh, "sh -c {list_sdx_cmd}").read().await?;
        let mut devices_iter = devices.lines();
        let dev = devices_iter.next().ok_or(anyhow::anyhow!(
            "Couldn't find device for controller {:#} lun {}",
            controller_guid,
            d.lun
        ))?;
        if devices_iter.next().is_some() {
            anyhow::bail!(
                "More than 1 device for controller {:#} lun {}",
                controller_guid,
                d.lun
            );
        }
        let dev = dev
            .rsplit('/')
            .next()
            .ok_or(anyhow::anyhow!("Couldn't parse device name from {dev}"))?;
        let sectors = cmd!(sh, "cat /sys/block/{dev}/size")
            .read()
            .await?
            .trim_end()
            .parse::<usize>()
            .context(format!(
                "Failed to parse size of device for controller {:#} lun {}",
                controller_guid, d.lun
            ))?;
        if sectors != d.disk_size_sectors {
            anyhow::bail!(
                "Unexpected size (in sectors) for device for controller {:#} lun {}: expected {}, got {}",
                controller_guid,
                d.lun,
                d.disk_size_sectors,
                sectors
            );
        }

        device_paths.push(format!("/dev/{dev}"));
    }

    // Check duplicates
    if device_paths.iter().collect::<HashSet<_>>().len() != device_paths.len() {
        anyhow::bail!("Found duplicate device paths: {device_paths:?}");
    }

    // Check that we found all devices and no extra devices are present
    let list_sdx_cmd = format!(
        // Don't fail if no devices are found
        "ls -d /sys/bus/vmbus/devices/{}/host*/target*/*:0:0:*/block/sd* || true",
        controller_guid
    );
    let devices = cmd!(sh, "sh -c {list_sdx_cmd}").read().await?;
    let devices_count = devices.lines().count();
    if devices_count != expected_devices.len() {
        anyhow::bail!(
            "Expected {} devices, found {} devices: {:?}",
            expected_devices.len(),
            devices_count,
            devices
        );
    }

    Ok(device_paths)
}

/// Runs a series of validation steps inside the Linux guest to verify that the
/// storage devices (especially as presented by OpenHCL's vSCSI implementation
/// storvsp) are present and working correctly.
///
/// May `panic!`, `assert!`, or return an `Err` if any checks fail. Which
/// mechanism is used depends on the nature of the failure and the most
/// convenient way to check for it in this routine.
async fn test_storage_linux(
    agent: &PipetteClient,
    controller_guid: Guid,
    expected_devices: Vec<ExpectedGuestDevice>,
) -> anyhow::Result<()> {
    const DEVICE_DISCOVER_RETRIES: u32 = 10;
    const DEVICE_DISCOVER_SLEEP_SECS: u64 = 3;

    let sh = agent.unix_shell();

    // Discover device paths, with retries
    let device_paths = {
        let mut attempt = 0;
        loop {
            match get_device_paths(agent, controller_guid, expected_devices.clone()).await {
                Ok(paths) => {
                    tracing::info!(?paths, "Discovered device paths");
                    break paths;
                }
                Err(e) if attempt + 1 < DEVICE_DISCOVER_RETRIES => {
                    tracing::warn!(
                        "Attempt {}/{}: Failed to get device paths: {:#}. Retrying in {} seconds...",
                        attempt + 1,
                        DEVICE_DISCOVER_RETRIES,
                        e,
                        DEVICE_DISCOVER_SLEEP_SECS
                    );
                    let seconds = format!("{DEVICE_DISCOVER_SLEEP_SECS}");
                    cmd!(sh, "sleep {seconds}").run().await?;
                    attempt += 1;
                }
                Err(e) => {
                    anyhow::bail!(
                        "Failed to get device paths after {} attempts: {:#}",
                        DEVICE_DISCOVER_RETRIES,
                        e
                    );
                }
            }
        }
    };

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
    for device in &device_paths {
        tracing::info!(?device, "Performing IO tests");
        cmd!(sh, "dd if=/dev/urandom of=/tmp/random_data bs=1M count=100")
            .run()
            .await?;

        cmd!(
            sh,
            "dd if=/tmp/random_data of={device} bs=1M count=100 oflag=direct conv=fsync"
        )
        .run()
        .await?;

        cmd!(
            sh,
            "dd if={device} of=/tmp/verify_data bs=1M count=100 iflag=direct,fullblock"
        )
        .run()
        .await?;

        cmd!(sh, "cmp -s /tmp/random_data /tmp/verify_data")
            .read()
            .await
            .with_context(|| format!("Read and written data differs for device {device}"))?;

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
    //openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)) TODO: re-enable once pipette issues in #2039 are resolved
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
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_linux`]),
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
        })
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
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
        .run()
        .await?;

    test_storage_linux(
        &agent,
        scsi_instance,
        vec![
            ExpectedGuestDevice {
                lun: vtl0_scsi_lun,
                disk_size_sectors: SCSI_DISK_SECTORS as usize,
                friendly_name: "scsi".to_string(),
            },
            ExpectedGuestDevice {
                lun: vtl0_nvme_lun,
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

/// Test a Linux VM with a SCSI disk assigned to VTL2 and
/// vmbus relay. This should expose one disk to VTL0 via vmbus.
#[cfg(windows)]
#[hyperv_test(openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)))]
async fn storvsp_hyperv(config: PetriVmBuilder<HyperVPetriBackend>) -> Result<(), anyhow::Error> {
    let vtl2_lun = 5;
    let vtl0_scsi_lun = 0;
    let scsi_instance = Guid::new_random();
    let vtl2_vsid = Guid::new_random();
    const SCSI_DISK_SECTORS: u64 = 0x4_0000;
    const SECTOR_SIZE: u64 = 512;
    const EXPECTED_SCSI_DISK_SIZE_BYTES: u64 = SCSI_DISK_SECTORS * SECTOR_SIZE;

    // Assumptions made by test infra & routines:
    //
    // 1. Some test-infra added disks are 64MiB in size. Since we find disks by size,
    // ensure that our test disks are a different size.
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_linux`]),
    // with some arbitrary buffer (5MiB in this case).
    static_assertions::const_assert_ne!(EXPECTED_SCSI_DISK_SIZE_BYTES, 64 * 1024 * 1024);
    static_assertions::const_assert!(EXPECTED_SCSI_DISK_SIZE_BYTES > 105 * 1024 * 1024);

    let mut vhd =
        tempfile::NamedTempFile::with_suffix("vtl2.vhd").context("create temp vtl2 vhd")?;
    vhd.as_file()
        .set_len(EXPECTED_SCSI_DISK_SIZE_BYTES)
        .context("set file length")?;

    disk_vhd1::Vhd1Disk::make_fixed(vhd.as_file_mut()).context("make fixed")?;

    // Close a handle to the file without deleting it, so that Hyper-V can open it.
    let vhd_path = vhd.into_temp_path();

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
                .build(),
        )
        .add_vmbus_storage_controller(&vtl2_vsid, petri::Vtl::Vtl2, petri::VmbusStorageType::Scsi)
        .add_vmbus_drive(
            petri::Drive::new(Some(petri::Disk::Persistent(vhd_path.to_path_buf())), false),
            &vtl2_vsid,
            Some(vtl2_lun),
        )
        .run()
        .await?;

    vm.modify_vtl2_settings(|s| {
        let storage_controllers = &mut s.dynamic.as_mut().unwrap().storage_controllers;
        assert_eq!(storage_controllers.len(), 1);
        assert_eq!(
            storage_controllers[0].instance_id,
            scsi_instance.to_string()
        );

        let controller = storage_controllers.get_mut(0).unwrap();
        controller.luns.push(
            Vtl2LunBuilder::disk()
                .with_location(vtl0_scsi_lun)
                .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                    ControllerType::Scsi,
                    vtl2_vsid,
                    vtl2_lun,
                ))
                .build(),
        );
    })
    .await?;

    test_storage_linux(
        &agent,
        scsi_instance,
        vec![ExpectedGuestDevice {
            lun: vtl0_scsi_lun,
            disk_size_sectors: SCSI_DISK_SECTORS as usize,
            friendly_name: "scsi".to_string(),
        }],
    )
    .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux Stripe VM with two SCSI disk assigned to VTL2 via NVMe Emulator
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
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
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_linux`]),
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
        })
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
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
        .run()
        .await?;

    test_storage_linux(
        &agent,
        scsi_instance,
        vec![ExpectedGuestDevice {
            lun: vtl0_nvme_lun,
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
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
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
        })
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
                .add_lun(Vtl2LunBuilder::dvd().with_location(vtl0_scsi_lun))
                // No physical devices initially, so the drive is empty
                .build(),
        )
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

    vm.modify_vtl2_settings(|v| {
        v.dynamic.as_mut().unwrap().storage_controllers[0].luns[0].physical_devices =
            build_vtl2_storage_backing_physical_devices(vec![Vtl2StorageBackingDeviceBuilder::new(
                ControllerType::Scsi,
                scsi_instance,
                vtl2_lun,
            )])
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
    vm.modify_vtl2_settings(|v| {
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
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
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
        })
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
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
        )
        .run()
        .await?;

    tracing::info!("VM is running, issuing read to dvd drive");

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

    tracing::info!("read complete and verified, powering off VM");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with several NVMe namespaces assigned to VTL2, and
/// vmbus relay. This should expose the disks to VTL0 as SCSI via vmbus.
/// The disks are added and removed in a loop, dynamically after VM boot rather than being there at boot time.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn storvsp_dynamic_add_disk(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    const NS_COUNT: u32 = 8;
    const FIRST_NS: u32 = 30;
    const FIRST_LUN: u32 = 0;
    const SECTOR_SIZE: u64 = 512;
    const NUM_ITERATIONS: u32 = 5;

    // 128MB for the first NS and 1MB extra for each subsequent NS
    const fn disk_sectors(index: u32) -> u64 {
        (128 + (index as u64)) * 1024 * 1024 / SECTOR_SIZE
    }

    let scsi_instance = Guid::new_random();

    // Assumptions made by test infra & routines:
    //
    // 1. Some test-infra added disks are 64MiB in size. Since we find disks by size,
    // ensure that our test disks are a different size.
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_linux`]),
    // with some arbitrary buffer (5MiB in this case).
    static_assertions::const_assert!(disk_sectors(0) * SECTOR_SIZE > 105 * 1024 * 1024);

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                // Create NVMe controller with all namespaces
                c.vpci_devices.push(VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: NVME_INSTANCE,
                    resource: NvmeControllerHandle {
                        subsystem_id: NVME_INSTANCE,
                        max_io_queues: 64,
                        msix_count: 64,
                        namespaces: (0..NS_COUNT)
                            .map(|i| NamespaceDefinition {
                                nsid: FIRST_NS + i,
                                disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                    len: Some(disk_sectors(i) * SECTOR_SIZE),
                                })
                                .into_resource(),
                                read_only: false,
                            })
                            .collect(),
                    }
                    .into_resource(),
                });
            })
        })
        .with_custom_vtl2_settings(move |v| {
            v.dynamic.as_mut().unwrap().storage_controllers.push(
                Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                    .with_instance_id(scsi_instance)
                    // No disks are attached initially
                    .build(),
            )
        })
        .run()
        .await?;

    tracing::info!("Testing that no disks are present in the guest");
    test_storage_linux(&agent, scsi_instance, vec![]).await?;

    for iteration in 1..=NUM_ITERATIONS {
        // Now dynamically add disks
        tracing::info!("Dynamically adding disks to VTL2 settings {iteration}/{NUM_ITERATIONS}");
        vm.modify_vtl2_settings(|s| {
            s.dynamic.as_mut().unwrap().storage_controllers[0]
                .luns
                .extend((0..NS_COUNT).map(|i| {
                    Vtl2LunBuilder::disk()
                        .with_location(FIRST_LUN + i)
                        .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                            ControllerType::Nvme,
                            NVME_INSTANCE,
                            FIRST_NS + i,
                        ))
                        .build()
                }))
        })
        .await?;

        tracing::info!(
            "Testing presence and IO on all disks in guest {iteration}/{NUM_ITERATIONS}"
        );
        test_storage_linux(
            &agent,
            scsi_instance,
            (0..NS_COUNT)
                .map(|i| ExpectedGuestDevice {
                    lun: FIRST_LUN + i,
                    disk_size_sectors: disk_sectors(i) as usize,
                    friendly_name: format!("nvme{}", i),
                })
                .collect(),
        )
        .await?;

        tracing::info!(
            "Dynamically removing all disks from VTL2 settings {iteration}/{NUM_ITERATIONS}"
        );
        vm.modify_vtl2_settings(|s| {
            s.dynamic.as_mut().unwrap().storage_controllers[0]
                .luns
                .clear();
        })
        .await?;

        tracing::info!("Testing absence of disks in guest {iteration}/{NUM_ITERATIONS}");
        test_storage_linux(&agent, scsi_instance, vec![]).await?;
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
