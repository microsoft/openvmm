// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 Linux direct boot with OpenHCL.

use disk_backend_resources::FileDiskHandle;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::DiskLayerHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use guid::Guid;
use hvlite_defs::config::DeviceVtl;
use hvlite_defs::config::VpciDeviceConfig;
use hvlite_defs::config::Vtl2BaseAddressType;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use petri::OpenHclServicingFlags;
use petri::PetriVmBuilder;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri::pipette::cmd;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64;
use std::fs::File;
use vm_resource::IntoResource;
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
async fn mana_nic(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(|b| b.with_nic())
        .run()
        .await?;

    validate_mana_nic(&agent).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a MANA nic assigned to VTL2 (backed by
/// the MANA emulator), and vmbus relay. Use the shared pool override to test
/// the shared pool dma path.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn mana_nic_shared_pool(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(|b| b.with_nic())
        .with_openhcl_command_line("OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1")
        .run()
        .await?;

    validate_mana_nic(&agent).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a MANA nic assigned to VTL2 (backed by
/// the MANA emulator), and vmbus relay. Perform servicing and validate that the
/// nic is still functional.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn mana_nic_servicing(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<LATEST_LINUX_DIRECT_TEST_X64>,),
) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(|b| b.with_nic())
        .with_openhcl_command_line("OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1")
        .run()
        .await?;

    validate_mana_nic(&agent).await?;

    vm.restart_openhcl(igvm_file, OpenHclServicingFlags::default())
        .await?;

    validate_mana_nic(&agent).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

fn new_test_vtl2_nvme_device(
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

/// Test an OpenHCL Linux direct VM with many NVMe devices assigned to VTL2 and vmbus relay.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn many_nvme_devices_servicing(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    const NUM_NVME_DEVICES: usize = 8;
    const SIZE: u64 = 0x1000;
    // Zeros make it easy to see what's going on when inspecting logs. Each device must be
    // associated with a unique GUID. The pci subsystem uses the data2 field to differentiate
    // devices.
    const BASE_GUID: Guid = guid::guid!("00000000-0000-0000-0000-000000000000");
    // (also to make it obvious when looking at logs)
    const GUID_UPDATE_PREFIX: u16 = 0x1110;
    const NSID_OFFSET: u32 = 0x10;

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                let device_ids = (0..NUM_NVME_DEVICES)
                    .map(|i| {
                        let mut g = BASE_GUID;
                        g.data2 = g.data2.wrapping_add(i as u16) + GUID_UPDATE_PREFIX;
                        (NSID_OFFSET + i as u32, g)
                    })
                    .collect::<Vec<_>>();

                c.vpci_devices.extend(
                    device_ids
                        .iter()
                        .map(|(nsid, guid)| new_test_vtl2_nvme_device(*nsid, SIZE, *guid, None)),
                );
            })
            .with_custom_vtl2_settings(|v| {
                let device_ids = (0..NUM_NVME_DEVICES)
                    .map(|i| {
                        let mut g = BASE_GUID;
                        g.data2 = g.data2.wrapping_add(i as u16) + GUID_UPDATE_PREFIX;
                        (NSID_OFFSET + i as u32, g)
                    })
                    .collect::<Vec<_>>();

                v.dynamic.as_mut().unwrap().storage_controllers.push(
                    vtl2_settings_proto::StorageController {
                        instance_id: Guid::new_random().to_string(),
                        protocol: vtl2_settings_proto::storage_controller::StorageProtocol::Scsi
                            .into(),
                        luns: device_ids
                            .iter()
                            .map(|(nsid, guid)| vtl2_settings_proto::Lun {
                                // Add 1 so as to avoid any confusion with booting from LUN 0 (on the implicit SCSI
                                // controller created by the above `config.with_vmbus_redirect` call above).
                                location: (*nsid - NSID_OFFSET) + 1,
                                device_id: Guid::new_random().to_string(),
                                vendor_id: "OpenVMM".to_string(),
                                product_id: "Disk".to_string(),
                                product_revision_level: "1.0".to_string(),
                                serial_number: "0".to_string(),
                                model_number: "1".to_string(),
                                physical_devices: Some(vtl2_settings_proto::PhysicalDevices {
                                    r#type:
                                        vtl2_settings_proto::physical_devices::BackingType::Single
                                            .into(),
                                    device: Some(vtl2_settings_proto::PhysicalDevice {
                                        device_type:
                                            vtl2_settings_proto::physical_device::DeviceType::Nvme
                                                .into(),
                                        device_path: guid.to_string(),
                                        sub_device_path: *nsid,
                                    }),
                                    devices: Vec::new(),
                                }),
                                ..Default::default()
                            })
                            .collect(),
                        io_queue_depth: None,
                    },
                )
            })
        })
        .run()
        .await?;

    for _ in 0..3 {
        agent.ping().await?;

        // Test that inspect serialization works with the old version.
        vm.test_inspect_openhcl().await?;

        vm.restart_openhcl(
            igvm_file.clone(),
            OpenHclServicingFlags {
                enable_nvme_keepalive: false,
                ..Default::default()
            },
        )
        .await?;

        agent.ping().await?;

        // Test that inspect serialization works with the new version.
        vm.test_inspect_openhcl().await?;
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test VTL2 memory allocation mode, and validate that VTL0 saw the correct
/// amount of ram.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn openhcl_linux_vtl2_ram_self_allocate(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let vtl2_ram_size = 1024 * 1024 * 1024; // 1GB
    let vm_ram_size = 6 * 1024 * 1024 * 1024; // 6GB
    let (mut vm, agent) = config
        .modify_backend(move |b| {
            b.with_custom_config(|cfg| {
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
    // proc/meminfo MemTotal. Locally tested to be ~28000 difference, so round
    // up to 29000 to account for small differences.
    //
    // TODO: If we allowed parsing inspect output, or instead perhaps parse the
    // device tree or kmsg output, we should be able to get an exact number for
    // what the bootloader reported. Alternatively, we could look at the device
    // tree and parse it ourselves again, but this requires refactoring some
    // crates to make `bootloader_fdt_parser` available outside the underhill
    // tree.
    let vtl2_allowable_difference_kb = 29000;
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
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
