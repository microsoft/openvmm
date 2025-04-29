// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests that run on more than one architecture.

use anyhow::Context;
use get_resources::ged::FirmwareEvent;
use hyperv_ic_resources::kvp::KvpRpc;
use jiff::SignedDuration;
use mesh::rpc::RpcSend;
use petri::PetriVmConfig;
use petri::ProcessorTopology;
use petri::ResolvedArtifact;
use petri::SIZE_1_GB;
use petri::ShutdownKind;
use petri::openvmm::NIC_MAC_ADDRESS;
use petri::openvmm::PetriVmConfigOpenVmm;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY;
use std::time::Duration;
use vmm_core_defs::HaltReason;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;

/// Boot through the UEFI firmware, it will shut itself down after booting.
#[vmm_test(
    openvmm_uefi_x64(none),
    openvmm_openhcl_uefi_x64(none),
    openvmm_uefi_aarch64(none),
    hyperv_openhcl_uefi_aarch64(none),
    hyperv_openhcl_uefi_x64(none)
)]
async fn frontpage(config: Box<dyn PetriVmConfig>) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic boot test.
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_pcat_x64(vhd(ubuntu_2204_server_x64)),
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_pcat_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn boot(config: Box<dyn PetriVmConfig>) -> anyhow::Result<()> {
    let (vm, agent) = config.run().await?;
    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic boot test for guests that are expected to reboot
// TODO: Remove this test and other enable Windows 11 ARM OpenVMM tests
// once we figure out how to get the guest to not reboot via IMC or other
// means. At that point, we can also use Windows Server 2025 for x64 tests.
// Hyper-V VMs work for now since we don't notice that they reboot
#[openvmm_test(openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)))]
async fn boot_reset_expected(config: PetriVmConfigOpenVmm) -> anyhow::Result<()> {
    let mut vm = config.run_with_lazy_pipette().await?;
    assert_eq!(vm.wait_for_halt().await?, HaltReason::Reset);
    vm.reset().await?;
    let agent = vm.wait_for_agent().await?;
    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Test the KVP IC.
///
/// Windows-only right now, because the Linux images do not include the KVP IC
/// daemon.
#[openvmm_test(uefi_x64(vhd(windows_datacenter_core_2022_x64)))]
async fn kvp_ic(config: PetriVmConfigOpenVmm) -> anyhow::Result<()> {
    // Run with a NIC to perform IP address tests.
    let (mut vm, agent) = config.with_nic().run().await?;
    let kvp = vm.wait_for_kvp().await?;

    // Perform a basic set and enumerate test.
    let test_key = "test_key";
    let test_value = hyperv_ic_resources::kvp::Value::String("test_value".to_string());
    kvp.call_failable(
        KvpRpc::Set,
        hyperv_ic_resources::kvp::SetParams {
            pool: hyperv_ic_resources::kvp::KvpPool::External,
            key: test_key.to_string(),
            value: test_value.clone(),
        },
    )
    .await?;
    let value = kvp
        .call_failable(
            KvpRpc::Enumerate,
            hyperv_ic_resources::kvp::EnumerateParams {
                pool: hyperv_ic_resources::kvp::KvpPool::External,
                index: 0,
            },
        )
        .await?
        .context("missing value")?;
    assert_eq!(value.key, test_key);
    assert_eq!(value.value, test_value.clone());

    let value = kvp
        .call_failable(
            KvpRpc::Enumerate,
            hyperv_ic_resources::kvp::EnumerateParams {
                pool: hyperv_ic_resources::kvp::KvpPool::External,
                index: 1,
            },
        )
        .await?;

    assert!(value.is_none());

    // Get IP information for the NIC.
    let ip_info = kvp
        .call_failable(
            KvpRpc::GetIpInfo,
            hyperv_ic_resources::kvp::GetIpInfoParams {
                adapter_id: NIC_MAC_ADDRESS.to_string().replace('-', ":"),
            },
        )
        .await?;

    // Validate the IP information against the default consomme confiugration.
    tracing::info!(?ip_info, "ip information");

    // Filter out link-local addresses, since Windows seems to enumerate one for
    // a little while after boot sometimes.
    let non_local_ipv4_addresses = ip_info
        .ipv4_addresses
        .iter()
        .filter(|ip| !ip.address.is_link_local())
        .collect::<Vec<_>>();

    assert_eq!(non_local_ipv4_addresses.len(), 1);
    let ip = &non_local_ipv4_addresses[0];
    assert_eq!(ip.address.to_string(), "10.0.0.2");
    assert_eq!(ip.subnet.to_string(), "255.255.255.0");
    assert_eq!(ip_info.ipv4_gateways.len(), 1);
    let gateway = &ip_info.ipv4_gateways[0];
    assert_eq!(gateway.to_string(), "10.0.0.1");

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Test the timesync IC.
#[openvmm_test(
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2204_server_x64)),
    // uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    linux_direct_x64
)]
async fn timesync_ic(config: PetriVmConfigOpenVmm) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_custom_config(|c| {
            // Start with the clock half a day in the past so that the clock is
            // initially wrong.
            c.rtc_delta_milliseconds = -(Duration::from_secs(40000).as_millis() as i64)
        })
        .run()
        .await?;

    let mut saw_time_sync = false;
    for _ in 0..30 {
        let time = agent.get_time().await?;
        let time = jiff::Timestamp::new(time.seconds, time.nanos).unwrap();
        tracing::info!(%time, "guest time");
        if time.duration_since(jiff::Timestamp::now()).abs() < SignedDuration::from_secs(10) {
            saw_time_sync = true;
            break;
        }
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(1))
            .cancelled()
            .await;
    }

    if !saw_time_sync {
        anyhow::bail!("time never synchronized");
    }

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Validate we can reboot a VM and reconnect to pipette.
// TODO: Reenable guests that use the framebuffer once #74 is fixed.
#[openvmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    // openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_pcat_x64(vhd(ubuntu_2204_server_x64)),
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    // openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_uefi_x64(vhd(ubuntu_2204_server_x64)),
    // openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn reboot(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config.run().await?;

    agent.ping().await?;

    agent.reboot().await?;
    assert_eq!(vm.wait_for_halt().await?, HaltReason::Reset);
    vm.reset().await?;

    let agent = vm.wait_for_agent().await?;

    agent.ping().await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Basic boot test without agent
// TODO: investigate why the shutdown ic doesn't work reliably with hyper-v
// in our ubuntu image
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    openvmm_pcat_x64(vhd(freebsd_13_2_x64)),
    openvmm_pcat_x64(iso(freebsd_13_2_x64)),
    openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_pcat_x64(vhd(ubuntu_2204_server_x64)),
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64))
)]
async fn boot_no_agent(config: Box<dyn PetriVmConfig>) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic boot test without agent and with a single VP.
#[vmm_test(
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64))
)]
async fn boot_no_agent_single_proc(config: Box<dyn PetriVmConfig>) -> anyhow::Result<()> {
    let mut vm = config
        .with_processor_topology(ProcessorTopology {
            vp_count: 1,
            ..Default::default()
        })
        .run_without_agent()
        .await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic reboot test without agent
// TODO: Reenable guests that use the framebuffer once #74 is fixed.
#[openvmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    // openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_pcat_x64(vhd(ubuntu_2204_server_x64)),
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    // openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_uefi_x64(vhd(ubuntu_2204_server_x64)),
    // openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64)),
)]
async fn reboot_no_agent(config: PetriVmConfigOpenVmm) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Reboot).await?;
    assert_eq!(vm.wait_for_halt().await?, HaltReason::Reset);
    vm.reset().await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Boot our guest-test UEFI image, which will run some tests,
/// and then purposefully triple fault itself via an expiring
/// watchdog timer.
#[vmm_test(
    openvmm_uefi_x64(guest_test_uefi_x64),
    openvmm_uefi_aarch64(guest_test_uefi_aarch64),
    openvmm_openhcl_uefi_x64(guest_test_uefi_x64)
)]
async fn guest_test_uefi(config: Box<dyn PetriVmConfig>) -> anyhow::Result<()> {
    let vm = config
        .with_windows_secure_boot_template()
        .run_without_agent()
        .await?;
    let arch = vm.arch();
    // No boot event check, UEFI watchdog gets fired before ExitBootServices
    let halt_reason = vm.wait_for_teardown().await?;
    tracing::debug!("vm halt reason: {halt_reason:?}");
    match arch {
        MachineArch::X86_64 => assert!(matches!(halt_reason, HaltReason::TripleFault { .. })),
        MachineArch::Aarch64 => assert!(matches!(halt_reason, HaltReason::Reset)),
    }
    Ok(())
}

/// Test transferring a file to the guest.
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn file_transfer_test(config: Box<dyn PetriVmConfig>) -> Result<(), anyhow::Error> {
    const TEST_CONTENT: &str = "hello world!";
    const FILE_NAME: &str = "test.txt";

    let (vm, agent) = config.run().await?;

    agent.write_file(FILE_NAME, TEST_CONTENT.as_bytes()).await?;
    assert_eq!(agent.read_file(FILE_NAME).await?, TEST_CONTENT.as_bytes());

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Boot Linux and have it write the visible memory size.
#[openvmm_test(linux_direct_x64, uefi_aarch64(vhd(ubuntu_2404_server_aarch64)))]
async fn five_gb(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    let configured_size = 5 * SIZE_1_GB;
    let expected_size = configured_size - configured_size / 10; // 10% buffer; TODO-figure out where this goes

    let (vm, agent) = config
        .with_custom_config(|c| c.memory.mem_size = configured_size)
        .run()
        .await?;

    // Validate that the RAM size is appropriate.
    // Skip the first 9 characters, which are "MemTotal:", and the last two,
    // which are the units.
    let output = agent.unix_shell().read_file("/proc/meminfo").await?;
    let memtotal_line = output
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .context("couldn't find memtotal")?;
    let size_kb: u64 = memtotal_line
        .strip_suffix("kB")
        .context("memtotal units should be in kB")?
        .trim()
        .parse()
        .context("couldn't parse size")?;
    assert!(
        size_kb * 1024 >= expected_size,
        "memory size {} >= {}",
        size_kb * 1024,
        expected_size
    );

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Verify that UEFI default boots even if invalid boot entries exist
/// when `default_boot_always_attempt` is enabled.
#[openvmm_test(
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY]
)]
async fn default_boot(
    config: PetriVmConfigOpenVmm,
    (initial_vmgs,): (ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,),
) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_vmgs(initial_vmgs)
        .with_default_boot_always_attempt(true)
        .run()
        .await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Verify that UEFI fails to boot if invalid boot entries exist
///
/// This test exists to ensure we are not getting a false positive for
/// the `default_boot` test above.
#[openvmm_test(
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY]
)]
async fn no_default_boot(
    config: PetriVmConfigOpenVmm,
    (initial_vmgs,): (ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,),
) -> Result<(), anyhow::Error> {
    let mut vm = config.with_vmgs(initial_vmgs).run_without_agent().await?;

    assert_eq!(vm.wait_for_boot_event().await?, FirmwareEvent::BootFailed);
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}
