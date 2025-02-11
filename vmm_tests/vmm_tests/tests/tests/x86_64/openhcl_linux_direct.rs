// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 Linux direct boot with OpenHCL.

use anyhow::Context;
use disk_backend_resources::layer::RamDiskLayerHandle;
use disk_backend_resources::LayeredDiskHandle;
use guid::Guid;
use hvlite_defs::config::DeviceVtl;
use hvlite_defs::config::VpciDeviceConfig;
use hvlite_defs::config::Vtl2BaseAddressType;
use mesh::rpc::RpcSend;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use petri::openvmm::PetriVmConfigOpenVmm;
use petri::pipette::cmd;
use petri::pipette::PipetteClient;
use petri::OpenHclServicingFlags;
use petri::ResolvedArtifact;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use scsidisk_resources::SimpleScsiDvdRequest;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use vm_resource::IntoResource;
use vmm_core_defs::HaltReason;
use vmm_test_macros::openvmm_test;

/// Today this only tests that the nic can get an IP address via consomme's DHCP
/// implementation.
///
/// FUTURE: Test traffic on the nic.
async fn validate_mana_nic(agent: &PipetteClient) -> Result<(), anyhow::Error> {
    let sh = agent.unix_shell();
    cmd!(sh, "ifconfig eth0 up").run().await?;
    cmd!(sh, "udhcpc eth0").run().await?;
    let output = cmd!(sh, "ifconfig eth0").read().await?;
    // Validate that we see a mana nic with the expected MAC address and IPs.
    assert!(output.contains("HWaddr 00:15:5D:12:12:12"));
    assert!(output.contains("inet addr:10.0.0.2"));
    assert!(output.contains("inet6 addr: fe80::215:5dff:fe12:1212/64"));

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a MANA nic assigned to VTL2 (backed by
/// the MANA emulator), and vmbus relay.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn mana_nic(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    let (vm, agent) = config.with_vmbus_redirect().with_nic().run().await?;

    validate_mana_nic(&agent).await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a MANA nic assigned to VTL2 (backed by
/// the MANA emulator), and vmbus relay. Use the shared pool override to test
/// the shared pool dma path.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn mana_nic_shared_pool(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_vmbus_redirect()
        .with_nic()
        .with_openhcl_command_line("OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1")
        .run()
        .await?;

    validate_mana_nic(&agent).await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a MANA nic assigned to VTL2 (backed by
/// the MANA emulator), and vmbus relay. Perform servicing and validate that the
/// nic is still functional.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn mana_nic_servicing(
    config: PetriVmConfigOpenVmm,
    (igvm_file,): (ResolvedArtifact<LATEST_LINUX_DIRECT_TEST_X64>,),
) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config
        .with_vmbus_redirect()
        .with_nic()
        .with_openhcl_command_line("OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1")
        .run()
        .await?;

    validate_mana_nic(&agent).await?;

    vm.restart_openhcl(igvm_file, OpenHclServicingFlags::default())
        .await?;

    validate_mana_nic(&agent).await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a SCSI disk assigned to VTL2, and
/// vmbus relay. This should expose a disk to VTL0 via vmbus.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn storvsp(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE: Guid = Guid::from_static_str("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    let vtl2_lun = 5;
    let vtl0_scsi_lun = 0;
    let vtl0_nvme_lun = 1;
    let vtl2_nsid = 37;
    let scsi_instance = Guid::new_random();

    let scsi_disk_sectors = 0x2000;
    let nvme_disk_sectors: u64 = 0x3000;
    let sector_size = 512;

    let (vm, agent) = config
        .with_vmbus_redirect()
        .with_custom_config(|c| {
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
                                len: Some(scsi_disk_sectors * sector_size),
                            })
                            .into_resource(),
                            read_only: false,
                            parameters: Default::default(),
                        }
                        .into_resource(),
                    }],
                    io_queue_depth: None,
                    requests: None,
                }
                .into_resource(),
            ));
            c.vpci_devices.push(VpciDeviceConfig {
                vtl: DeviceVtl::Vtl2,
                instance_id: NVME_INSTANCE,
                resource: NvmeControllerHandle {
                    subsystem_id: NVME_INSTANCE,
                    max_io_queues: 64,
                    msix_count: 64,
                    namespaces: vec![NamespaceDefinition {
                        nsid: vtl2_nsid,
                        disk: (LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                            len: Some(nvme_disk_sectors * sector_size),
                        })
                        .into_resource()),
                        read_only: false,
                    }],
                }
                .into_resource(),
            });
        })
        .with_custom_vtl2_settings(|v| {
            v.dynamic.as_mut().unwrap().storage_controllers.push(
                vtl2_settings_proto::StorageController {
                    instance_id: scsi_instance.to_string(),
                    protocol: vtl2_settings_proto::storage_controller::StorageProtocol::Scsi.into(),
                    luns: vec![
                        vtl2_settings_proto::Lun {
                            location: vtl0_scsi_lun,
                            device_id: Guid::new_random().to_string(),
                            vendor_id: "OpenVMM".to_string(),
                            product_id: "Disk".to_string(),
                            product_revision_level: "1.0".to_string(),
                            serial_number: "0".to_string(),
                            model_number: "1".to_string(),
                            physical_devices: Some(vtl2_settings_proto::PhysicalDevices {
                                r#type: vtl2_settings_proto::physical_devices::BackingType::Single
                                    .into(),
                                device: Some(vtl2_settings_proto::PhysicalDevice {
                                    device_type:
                                        vtl2_settings_proto::physical_device::DeviceType::Vscsi
                                            .into(),
                                    device_path: scsi_instance.to_string(),
                                    sub_device_path: vtl2_lun,
                                }),
                                devices: Vec::new(),
                            }),
                            ..Default::default()
                        },
                        vtl2_settings_proto::Lun {
                            location: vtl0_nvme_lun,
                            device_id: Guid::new_random().to_string(),
                            vendor_id: "OpenVMM".to_string(),
                            product_id: "Disk".to_string(),
                            product_revision_level: "1.0".to_string(),
                            serial_number: "0".to_string(),
                            model_number: "1".to_string(),
                            physical_devices: Some(vtl2_settings_proto::PhysicalDevices {
                                r#type: vtl2_settings_proto::physical_devices::BackingType::Single
                                    .into(),
                                device: Some(vtl2_settings_proto::PhysicalDevice {
                                    device_type:
                                        vtl2_settings_proto::physical_device::DeviceType::Nvme
                                            .into(),
                                    device_path: NVME_INSTANCE.to_string(),
                                    sub_device_path: vtl2_nsid,
                                }),
                                devices: Vec::new(),
                            }),
                            ..Default::default()
                        },
                    ],
                    io_queue_depth: None,
                },
            )
        })
        .run()
        .await?;

    let sh = agent.unix_shell();
    // The drive ordering is not guaranteed, so we need to check all drives.
    let output = cmd!(sh, "sh -c 'cat /sys/block/sd*/size'").read().await?;
    // Make sure the disk sizes match.
    let reported_sizes = output
        .split_ascii_whitespace()
        .map(|x| x.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .context("failed to parse sizes")?;

    let scsi_drive_index = reported_sizes
        .iter()
        .position(|x| *x == scsi_disk_sectors)
        .expect("couldn't find scsi drive");
    let nvme_drive_index = reported_sizes
        .iter()
        .position(|x| *x == nvme_disk_sectors)
        .expect("couldn't find nvme drive");
    assert_ne!(scsi_drive_index, nvme_drive_index);
    // Account for the pipette drive too
    assert_eq!(reported_sizes.len(), 3);

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a SCSI DVD assigned to VTL2, and vmbus
/// relay. This should expose a DVD to VTL0 via vmbus. Start with an empty
/// drive, then add and remove media.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn openhcl_linux_storvsp_dvd(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    let vtl2_lun = 5;
    let vtl0_scsi_lun = 0;
    let scsi_instance = Guid::new_random();

    let (hot_plug_send, hot_plug_recv) = mesh::channel();

    let mut vtl2_settings = None;

    let (mut vm, agent) = config
        .with_vmbus_redirect()
        .with_custom_config(|c| {
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
                }
                .into_resource(),
            ));
        })
        .with_custom_vtl2_settings(|v| {
            v.dynamic.as_mut().unwrap().storage_controllers.push(
                vtl2_settings_proto::StorageController {
                    instance_id: scsi_instance.to_string(),
                    protocol: vtl2_settings_proto::storage_controller::StorageProtocol::Scsi.into(),
                    luns: vec![vtl2_settings_proto::Lun {
                        location: vtl0_scsi_lun,
                        device_id: Guid::new_random().to_string(),
                        vendor_id: "OpenVMM".to_string(),
                        product_id: "Disk".to_string(),
                        product_revision_level: "1.0".to_string(),
                        serial_number: "0".to_string(),
                        model_number: "1".to_string(),
                        is_dvd: true,
                        ..Default::default()
                    }],
                    io_queue_depth: None,
                },
            );
            vtl2_settings = Some(v.clone());
        })
        .run()
        .await?;

    let mut vtl2_settings = vtl2_settings.unwrap();

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
                LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                    len: Some(len as u64),
                })
                .into_resource(),
            ),
        )
        .await
        .context("failed to change media")?;

    // Insert media.
    vtl2_settings.dynamic.as_mut().unwrap().storage_controllers[0].luns[0].physical_devices =
        Some(vtl2_settings_proto::PhysicalDevices {
            r#type: vtl2_settings_proto::physical_devices::BackingType::Single.into(),
            device: Some(vtl2_settings_proto::PhysicalDevice {
                device_type: vtl2_settings_proto::physical_device::DeviceType::Vscsi.into(),
                device_path: scsi_instance.to_string(),
                sub_device_path: vtl2_lun,
            }),
            devices: Vec::new(),
        });

    vm.modify_vtl2_settings(&vtl2_settings)
        .await
        .context("failed to modify vtl2 settings")?;

    let b = read_drive().await.context("failed to read dvd drive")?;
    if b.len() != len {
        anyhow::bail!("expected {} bytes, got {}", len, b.len());
    }

    // Remove media.
    vtl2_settings.dynamic.as_mut().unwrap().storage_controllers[0].luns[0].physical_devices = None;
    vm.modify_vtl2_settings(&vtl2_settings)
        .await
        .context("failed to modify vtl2 settings")?;

    ensure_no_medium(read_drive().await)?;

    hot_plug_send
        .call_failable(SimpleScsiDvdRequest::ChangeMedia, None)
        .await
        .context("failed to change media")?;

    agent.power_off().await?;
    drop(hot_plug_send);
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Test an OpenHCL Linux Stripe VM with two SCSI disk assigned to VTL2 via NVMe Emulator
#[openvmm_test(openhcl_linux_direct_x64)]
async fn openhcl_linux_stripe_storvsp(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE_1: Guid = Guid::from_static_str("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    const NVME_INSTANCE_2: Guid = Guid::from_static_str("06a97a09-d5ad-4689-b638-9419d7346a68");
    let vtl0_nvme_lun = 0;
    let vtl2_nsid = 1;
    let nvme_disk_sectors: u64 = 0x10000;
    let sector_size = 512;
    let number_of_stripe_devices = 2;
    let scsi_instance = Guid::new_random();

    let (vm, agent) = config
        .with_vmbus_redirect()
        .with_custom_config(|c| {
            c.vpci_devices.extend([
                VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: NVME_INSTANCE_1,
                    resource: NvmeControllerHandle {
                        subsystem_id: NVME_INSTANCE_1,
                        max_io_queues: 64,
                        msix_count: 64,
                        namespaces: vec![NamespaceDefinition {
                            nsid: vtl2_nsid,
                            disk: (LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                len: Some(nvme_disk_sectors * sector_size),
                            })
                            .into_resource()),
                            read_only: false,
                        }],
                    }
                    .into_resource(),
                },
                VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: NVME_INSTANCE_2,
                    resource: NvmeControllerHandle {
                        subsystem_id: NVME_INSTANCE_2,
                        max_io_queues: 64,
                        msix_count: 64,
                        namespaces: vec![NamespaceDefinition {
                            nsid: vtl2_nsid,
                            disk: (LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                len: Some(nvme_disk_sectors * sector_size),
                            })
                            .into_resource()),
                            read_only: false,
                        }],
                    }
                    .into_resource(),
                },
            ]);
        })
        .with_custom_vtl2_settings(|v| {
            v.dynamic.as_mut().unwrap().storage_controllers.push(
                vtl2_settings_proto::StorageController {
                    instance_id: scsi_instance.to_string(),
                    protocol: vtl2_settings_proto::storage_controller::StorageProtocol::Scsi.into(),
                    luns: vec![vtl2_settings_proto::Lun {
                        location: vtl0_nvme_lun,
                        device_id: Guid::new_random().to_string(),
                        vendor_id: "OpenVMM".to_string(),
                        product_id: "Disk".to_string(),
                        product_revision_level: "1.0".to_string(),
                        serial_number: "0".to_string(),
                        model_number: "1".to_string(),
                        chunk_size_in_kb: 128,
                        is_dvd: false,
                        physical_devices: Some(vtl2_settings_proto::PhysicalDevices {
                            r#type: vtl2_settings_proto::physical_devices::BackingType::Striped
                                .into(),
                            device: None,
                            devices: vec![
                                vtl2_settings_proto::PhysicalDevice {
                                    device_type:
                                        vtl2_settings_proto::physical_device::DeviceType::Nvme
                                            .into(),
                                    device_path: NVME_INSTANCE_1.to_string(),
                                    sub_device_path: vtl2_nsid,
                                },
                                vtl2_settings_proto::PhysicalDevice {
                                    device_type:
                                        vtl2_settings_proto::physical_device::DeviceType::Nvme
                                            .into(),
                                    device_path: NVME_INSTANCE_2.to_string(),
                                    sub_device_path: vtl2_nsid,
                                },
                            ],
                        }),
                        ..Default::default()
                    }],
                    io_queue_depth: None,
                },
            )
        })
        .run()
        .await?;

    let sh = agent.unix_shell();
    let output = sh.read_file("/sys/block/sda/size").await?;

    let reported_nvme_sectors = output
        .trim()
        .parse::<u64>()
        .context("failed to parse size")?;

    assert_eq!(
        reported_nvme_sectors,
        nvme_disk_sectors * number_of_stripe_devices
    );

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Test VTL2 memory allocation mode, and validate that VTL0 saw the correct
/// amount of ram.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn openhcl_linux_vtl2_ram_self_allocate(
    config: PetriVmConfigOpenVmm,
) -> Result<(), anyhow::Error> {
    let vtl2_ram_size = 1024 * 1024 * 1024; // 1GB
    let vm_ram_size = 6 * 1024 * 1024 * 1024; // 6GB
    let (mut vm, agent) = config
        .with_custom_config(|cfg| {
            if let hvlite_defs::config::LoadMode::Igvm {
                ref mut vtl2_base_address,
                ..
            } = cfg.load_mode
            {
                *vtl2_base_address = Vtl2BaseAddressType::Vtl2Allocate {
                    size: Some(vtl2_ram_size),
                }
            } else {
                panic!("unexpected load mode, must be igvm");
            }

            // Disable late map vtl0 memory when vtl2 allocation mode is used.
            cfg.hypervisor
                .with_vtl2
                .as_mut()
                .unwrap()
                .late_map_vtl0_memory = None;

            // Set overall VM ram.
            cfg.memory.mem_size = vm_ram_size;
        })
        .run()
        .await?;

    let parse_meminfo_kb = |output: &str| -> Result<u64, anyhow::Error> {
        let meminfo = output
            .lines()
            .find(|line| line.starts_with("MemTotal:"))
            .unwrap();

        let mem_kb = meminfo.split_whitespace().nth(1).unwrap();
        Ok(mem_kb.parse()?)
    };

    let vtl2_agent = vm.wait_for_vtl2_agent().await?;

    // Make sure VTL2 ram is 1GB, as requested.
    let vtl2_mem_kb = parse_meminfo_kb(&vtl2_agent.unix_shell().read_file("/proc/meminfo").await?)?;

    // The allowable difference between VTL2's expected ram size and
    // proc/meminfo MemTotal. Locally tested to be 27200 difference, so round up
    // to 28000 to account for small differences.
    //
    // TODO: If we allowed parsing inspect output, or instead perhaps parse the
    // device tree or kmsg output, we should be able to get an exact number for
    // what the bootloader reported. Alternatively, we could look at the device
    // tree and parse it ourselves again, but this requires refactoring some
    // crates to make `bootloader_fdt_parser` available outside the underhill
    // tree.
    let vtl2_allowable_difference_kb = 28000;
    let vtl2_expected_mem_kb = vtl2_ram_size / 1024;
    let vtl2_diff = (vtl2_mem_kb as i64 - vtl2_expected_mem_kb as i64).unsigned_abs();
    tracing::info!(
        vtl2_mem_kb,
        vtl2_expected_mem_kb,
        vtl2_diff,
        "parsed vtl2 ram"
    );
    assert!(
        vtl2_diff <= vtl2_allowable_difference_kb,
        "expected VTL2 MemTotal to be around {} kb, actual was {} kb, diff {} kb, allowable_diff {} kb",
        vtl2_expected_mem_kb,
        vtl2_mem_kb,
        vtl2_diff,
        vtl2_allowable_difference_kb
    );

    // Parse MemTotal from /proc/meminfo, and validate that it is around 5GB.
    let mem_kb = parse_meminfo_kb(&agent.unix_shell().read_file("/proc/meminfo").await?)?;

    // The allowable difference between the expected ram size and proc/meminfo
    // MemTotal. Locally tested to be 188100 KB difference, so add a bit more
    // to account for small variations.
    let allowable_difference_kb = 200000;
    let expected_mem_kb = (vm_ram_size / 1024) - (vtl2_ram_size / 1024);
    let diff = (mem_kb as i64 - expected_mem_kb as i64).unsigned_abs();
    tracing::info!(mem_kb, expected_mem_kb, diff, "parsed vtl0 ram");
    assert!(
        diff <= allowable_difference_kb,
        "expected vtl0 MemTotal to be around {} kb, actual was {} kb, diff {} kb, allowable_diff {} kb",
        expected_mem_kb,
        mem_kb,
        diff,
        allowable_difference_kb
    );

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}
