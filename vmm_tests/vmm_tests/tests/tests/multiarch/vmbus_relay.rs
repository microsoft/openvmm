// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use std::str::FromStr;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;

// Test for vmbus relay
// TODO: VBS isolation was failing and other targets too
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))
)]
async fn vmbus_relay<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config.with_vmbus_redirect(true).run().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Openhcl boot test with MNF enabled in vmbus relay.
///
/// TODO: Remove the no_agent version below once agents are supported in CVMs.
///
/// TODO: validate in this test that MNF actually works by querying guests
/// properties via the agent.
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
)]
async fn vmbus_relay_force_mnf<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .with_openhcl_command_line("OPENHCL_VMBUS_ENABLE_MNF=1")
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

// Test for vmbus relay
// TODO: VBS isolation was failing and other targets too
#[vmm_test(
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))
)]
#[cfg_attr(not(windows), expect(dead_code))]
async fn vmbus_relay_heavy<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .with_processor_topology(ProcessorTopology {
            vp_count: 16,
            vps_per_socket: Some(8),
            ..Default::default()
        })
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// MNF guest support: capture and print recursive listing of vmbus drivers.
/// TODO: add entries for CVM guests once MNF support in CVMs is added. Tracked by  #1940
/// TODO: investigate flakiness for openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)). Tracked by: #2100
#[openvmm_test(
    openvmm_openhcl_linux_direct_x64,
    // openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn validate_mnf_usage_in_guest(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    // So far, NetVSC uses MNF, StorVSC doesn't hence attach a nic to the vm.
    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .with_openhcl_command_line("OPENHCL_VMBUS_ENABLE_MNF=1")
        .modify_backend(|c| c.with_nic())
        .run()
        .await?;

    let netvsc_path = "/sys/bus/vmbus/drivers/hv_netvsc";
    let mut sh = agent.unix_shell();
    sh.change_dir(netvsc_path);

    // List directory contents for visibility.
    let contents = cmd!(sh, "ls -la {netvsc_path}").read().await?;
    tracing::info!("Listing all contents of {}:\n{}", netvsc_path, contents);

    // Pure helpers for parsing and path resolution.
    fn is_guid(s: &str) -> bool {
        guid::Guid::from_str(s).is_ok()
    }

    // Extract absolute target dirs from GUID-named symlink entries in ls output.
    // Each symlink entry points to a device instance within /sys/bus/vmbus/devices/
    // The GUIDs are the instance ID.
    let device_dirs: Vec<String> = contents
        .lines()
        .filter(|line| line.starts_with('l'))
        .filter_map(|line| {
            let (left, target) = line.rsplit_once(" -> ")?;
            let name = left.split_whitespace().last()?;
            is_guid(name).then(|| target.to_string())
        })
        .collect();

    tracing::info!("Devices:\n{}", device_dirs.join("\n"));

    // For each device, ensure at least one monitor_id file exists.
    for device in device_dirs {
        sh.change_dir(&device);
        let find_out = cmd!(sh, "find . -type f -name 'monitor_id'").read().await?;
        let has_monitor = find_out.lines().any(|s| !s.trim().is_empty());
        assert!(has_monitor, "no monitor_id files found in {}", device);
        sh.change_dir(netvsc_path);
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
