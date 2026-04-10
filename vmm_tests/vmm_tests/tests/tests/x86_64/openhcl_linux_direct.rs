// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 Linux direct boot with OpenHCL.

use crate::x86_64::storage::new_test_vtl2_nvme_device;
use guid::Guid;
use memory_range::MemoryRange;
use openvmm_defs::config::Vtl2BaseAddressType;
use petri::MemoryConfig;
use petri::OpenvmmLogConfig;
use petri::PetriVmBuilder;
use petri::ProcessorTopology;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri::pipette::cmd;
use petri::vtl2_settings::ControllerType;
use petri::vtl2_settings::Vtl2LunBuilder;
use petri::vtl2_settings::Vtl2StorageBackingDeviceBuilder;
use petri::vtl2_settings::Vtl2StorageControllerBuilder;
use vmm_test_macros::openvmm_test;
use zerocopy::FromBytes;

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
        .run()
        .await?;

    validate_mana_nic(&agent).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with many NVMe devices assigned to VTL2 and vmbus relay.
///#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn _many_nvme_devices_servicing_very_heavy(
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

    let flags = config.default_servicing_flags();

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .with_vtl2_base_address_type(Vtl2BaseAddressType::MemoryLayout {
            size: Some((960 + 64) * 1024 * 1024), // 960MB as specified in manifest, plus 64MB extra for private pool.
        })
        .with_host_log_levels(OpenvmmLogConfig::Custom([
            ("OPENVMM_LOG".to_owned(), "debug,vpci=trace".to_owned()),
            ("OPENVMM_SHOW_SPANS".to_owned(), "true".to_owned()),
        ].into()))
        .with_openhcl_command_line(
            "OPENHCL_ENABLE_VTL2_GPA_POOL=16384 dyndbg=\"module vfio_pci +p; module pci_hyperv +p\" udev.log_priority=debug OPENHCL_CONFIG_TIMEOUT_IN_SECONDS=30",
        ) // 64MB of private pool for VTL2 NVMe devices, debug logging for vfio-pci driver.
        .with_memory(MemoryConfig {
            startup_bytes: 8 * 1024 * 1024 * 1024, // 8GB
            ..Default::default()
        })
        .with_processor_topology(ProcessorTopology {
            vp_count: 4,
            ..Default::default()
        })
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
        })
        .add_vtl2_storage_controller({
            let device_ids = (0..NUM_NVME_DEVICES)
                .map(|i| {
                    let mut g = BASE_GUID;
                    g.data2 = g.data2.wrapping_add(i as u16) + GUID_UPDATE_PREFIX;
                    (NSID_OFFSET + i as u32, g)
                })
                .collect::<Vec<_>>();

            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .add_luns(
                    device_ids
                        .iter()
                        .map(|(nsid, guid)| {
                            Vtl2LunBuilder::disk()
                                // Add 1 so as to avoid any confusion with booting from LUN 0 (on the implicit SCSI
                                // controller created by the above `config.with_vmbus_redirect` call above).
                                .with_location((*nsid - NSID_OFFSET) + 1)
                                .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                                    ControllerType::Nvme,
                                    *guid,
                                    *nsid,
                                ))
                        })
                        .collect(),
                )
                .build()
        })
        .run()
        .await?;

    for _ in 0..3 {
        agent.ping().await?;

        // Test that inspect serialization works with the old version.
        vm.test_inspect_openhcl().await?;

        vm.restart_openhcl(igvm_file.clone(), flags).await?;

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
        .with_memory(MemoryConfig {
            startup_bytes: vm_ram_size,
            ..Default::default()
        })
        .with_vtl2_base_address_type(Vtl2BaseAddressType::Vtl2Allocate {
            size: Some(vtl2_ram_size),
        })
        .with_openhcl_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=off") // Private pool steals from reported memory usage, disable it for this test.
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

async fn read_sysfs_dt_string(agent: &PipetteClient, path: &str) -> Result<String, anyhow::Error> {
    let string = agent
        .unix_shell()
        .read_file(format!("/sys/firmware/devicetree/base/{}", path))
        .await?;
    // Strip the ending null terminator.
    Ok(string.trim_end_matches('\0').to_owned())
}

async fn read_sysfs_dt_raw(agent: &PipetteClient, path: &str) -> Result<Vec<u8>, anyhow::Error> {
    agent
        .unix_shell()
        .read_file_raw(format!("/sys/firmware/devicetree/base/{}", path))
        .await
}

async fn read_sysfs_dt<T: FromBytes>(
    agent: &PipetteClient,
    path: &str,
) -> Result<T, anyhow::Error> {
    let raw = read_sysfs_dt_raw(agent, path).await?;
    T::read_from_bytes(&raw).map_err(|_| {
        anyhow::anyhow!(
            "failed to read value of type {} from sysfs dt path {}",
            std::any::type_name::<T>(),
            path
        )
    })
}

async fn parse_vmbus_mmio(
    agent: &PipetteClient,
    path: &str,
) -> Result<Vec<MemoryRange>, anyhow::Error> {
    // Read the raw ranges which are u64 (start, start, len) tuples.
    let raw = read_sysfs_dt_raw(agent, format!("{}/ranges", path).as_str()).await?;
    let mut mmio_ranges = Vec::new();
    let raw_u64 = <[zerocopy::big_endian::U64]>::ref_from_bytes_with_elems(&raw, raw.len() / 8)
        .map_err(|_| {
            anyhow::anyhow!(
                "failed to read mmio ranges from sysfs dt path {}/ranges",
                path
            )
        })?;
    for chunk in raw_u64.chunks_exact(3) {
        let start: u64 = chunk[0].into();
        let len: u64 = chunk[2].into();
        let end = start + len;
        mmio_ranges.push(MemoryRange::new(start..end));
    }

    Ok(mmio_ranges)
}

async fn parse_openhcl_memory_node(
    agent: &PipetteClient,
    start: u64,
) -> Result<MemoryRange, anyhow::Error> {
    // Read the openhcl memory node with format "memory@start", with a u64 reg field of (start, len).
    // The openhcl memory type should be 5 (VTL0_MMIO).
    let raw = read_sysfs_dt_raw(agent, format!("openhcl/memory@{:x}/reg", start).as_str()).await?;
    let raw_u64 = <[zerocopy::big_endian::U64]>::ref_from_bytes_with_elems(&raw, raw.len() / 8)
        .map_err(|_| {
            anyhow::anyhow!(
                "failed to read mmio range from sysfs dt path openhcl/memory@{:x}/reg",
                start
            )
        })?;
    if raw_u64.len() != 2 {
        return Err(anyhow::anyhow!(
            "expected 2 u64 values in reg field, got {}",
            raw_u64.len()
        ));
    }

    let memory_type: u32 = read_sysfs_dt::<zerocopy::big_endian::U32>(
        agent,
        format!("openhcl/memory@{:x}/openhcl,memory-type", start).as_str(),
    )
    .await?
    .into();
    const VTL0_MMIO: u32 = 5;
    assert_eq!(memory_type, VTL0_MMIO);

    let range_start: u64 = raw_u64[0].into();
    let range_len: u64 = raw_u64[1].into();
    let range_end = range_start + range_len;
    Ok(MemoryRange::new(range_start..range_end))
}

/// Test VTL2 memory allocation mode, and validate that VTL0 saw the correct
/// amount of mmio, when the host provides a VTL2 mmio range.
///
/// TODO: onboard Hyper-V support in petri for custom mmio config once Hyper-V
/// supports this.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn openhcl_linux_vtl2_mmio_self_allocate(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    // Use the OpenVMM default which has a 1GB mmio gap for VTL2. This should
    // cause the whole gap to be given to VTL2, as we should report 128MB for
    // self allocation.
    let expected_mmio_ranges: Vec<MemoryRange> =
        openvmm_defs::config::DEFAULT_MMIO_GAPS_X86_WITH_VTL2.into();
    let (mut vm, agent) = config
        .with_memory(MemoryConfig {
            mmio_gaps: petri::MmioConfig::Custom(expected_mmio_ranges.clone()),
            ..Default::default()
        })
        .with_vtl2_base_address_type(Vtl2BaseAddressType::Vtl2Allocate { size: None })
        .run()
        .await?;

    let vtl2_agent = vm.wait_for_vtl2_agent().await?;

    // Read the bootloader provided fdt via sysfs to verify that the VTL2 and
    // VTL0 mmio ranges are as expected, along with the allocated mmio size
    // being 128 MB.
    let memory_allocation_mode: String =
        read_sysfs_dt_string(&vtl2_agent, "openhcl/memory-allocation-mode").await?;
    assert_eq!(memory_allocation_mode, "vtl2");

    let mmio_size: u64 =
        read_sysfs_dt::<zerocopy::big_endian::U64>(&vtl2_agent, "openhcl/mmio-size")
            .await?
            .into();
    // NOTE: This value is hardcoded in openvmm today to report this to the
    // guest provided device tree.
    const EXPECTED_MMIO_SIZE: u64 = 128 * 1024 * 1024;
    assert_eq!(mmio_size, EXPECTED_MMIO_SIZE);

    // Read the bootloader provided dt via sysfs to verify the VTL0 and VTL2
    // mmio ranges are as expected.
    let vtl2_mmio = parse_vmbus_mmio(&vtl2_agent, "bus/vmbus").await?;
    assert_eq!(vtl2_mmio, expected_mmio_ranges[2..]);
    let mut vtl0_mmio = Vec::new();
    for range_start in expected_mmio_ranges[..2].iter().map(|r| r.start()) {
        let range = parse_openhcl_memory_node(&vtl2_agent, range_start).await?;
        vtl0_mmio.push(range);
    }
    assert_eq!(vtl0_mmio, expected_mmio_ranges[..2]);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
