// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests that run on more than one architecture.

use anyhow::Context;
use futures::StreamExt;
use hyperv_ic_resources::kvp::KvpRpc;
use jiff::SignedDuration;
use mesh::rpc::RpcSend;
use petri::MemoryConfig;
use petri::PetriGuestStateLifetime;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::ResolvedArtifact;
use petri::SIZE_1_GB;
use petri::ShutdownKind;
use petri::openvmm::NIC_MAC_ADDRESS;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY;
use std::str::FromStr;
use std::time::Duration;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::openvmm_test_no_agent;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_no_agent;

// Servicing tests.
pub(crate) mod openhcl_servicing;

/// Boot through the UEFI firmware, it will shut itself down after booting.
#[vmm_test_no_agent(
    openvmm_uefi_x64(none),
    openvmm_openhcl_uefi_x64(none),
    openvmm_uefi_aarch64(none),
    hyperv_openhcl_uefi_aarch64(none),
    hyperv_openhcl_uefi_x64(none)
)]
async fn frontpage<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let vm = config.run_without_agent().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test.
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_pcat_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
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
async fn boot<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config.run().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[cfg(windows)] // requires VPCI support, which is only on Windows right now
#[vmm_test(
    // TODO: virt_whp is missing VPCI LPI interrupt support, used by Windows (but not Linux)
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // TODO: Linux image is missing VPCI driver in its initrd
    // openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    // openvmm_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn boot_nvme<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_boot_device_type(petri::BootDeviceType::Nvme)
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Tests NVMe boot with OpenHCL VPCI relaying enabled.
#[cfg(windows)] // requires VPCI support, which is only on Windows right now
#[vmm_test(
    // TODO: aarch64 support (WHP missing ARM64 VTL2 support)
    // openvmm_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // TODO: Linux image is missing VPCI driver in its initrd
    // openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn boot_nvme_vpci_relay<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_boot_device_type(petri::BootDeviceType::Nvme)
        .with_openhcl_command_line("OPENHCL_ENABLE_VPCI_RELAY=1")
        .with_vmbus_redirect(true)
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

// Basic vp "heavy" boot test with 16 VPs.
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_pcat_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
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
async fn boot_heavy<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let is_openhcl = config.is_openhcl();
    let (vm, agent) = config
        .with_processor_topology(ProcessorTopology {
            vp_count: 16,
            ..Default::default()
        })
        // multiarch::openvmm_uefi_x64_windows_datacenter_core_2022_x64_boot_heavy
        // fails with 4GB of RAM (the default), and openhcl tests fail with 1GB.
        .with_memory(MemoryConfig {
            startup_bytes: if is_openhcl { 4 * SIZE_1_GB } else { SIZE_1_GB },
            ..Default::default()
        })
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test with secure boot enabled and a valid template.
#[vmm_test(
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn secure_boot<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config.with_secure_boot().run().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Verify that secure boot fails with a mismatched template.
/// TODO: Allow Hyper-V VMs to load a UEFI firmware per VM, not system wide.
#[vmm_test_no_agent(
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    // hyperv_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // hyperv_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    // hyperv_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // hyperv_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn secure_boot_mismatched_template<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let config = config
        .with_expect_boot_failure()
        .with_secure_boot()
        .with_uefi_frontpage(false);
    let config = match config.os_flavor() {
        OsFlavor::Windows => config.with_uefi_ca_secure_boot_template(),
        OsFlavor::Linux => config.with_windows_secure_boot_template(),
        _ => anyhow::bail!("Unsupported OS flavor for test: {:?}", config.os_flavor()),
    };
    let vm = config.run_without_agent().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test EFI diagnostics with no boot devices on OpenVMM.
/// TODO:
///   - kmsg support in Hyper-V
///   - openhcl_uefi_aarch64 support
///   - uefi_x64 + uefi_aarch64 trace searching support
#[openvmm_test_no_agent(openhcl_uefi_x64(none))]
async fn efi_diagnostics_no_boot(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let vm = config.with_uefi_frontpage(true).run_without_agent().await?;

    // Expected no-boot message.
    const NO_BOOT_MSG: &str = "[Bds] Unable to boot!";

    // Get kmsg stream
    let mut kmsg = vm.kmsg().await?;

    // Search for the message
    while let Some(data) = kmsg.next().await {
        let data = data.context("reading kmsg")?;
        let msg = kmsg::KmsgParsedEntry::new(&data).unwrap();
        let raw = msg.message.as_raw();
        if raw.contains(NO_BOOT_MSG) {
            return Ok(());
        }
    }

    anyhow::bail!("Did not find expected message in kmsg");
}

/// Test the KVP IC.
///
/// Windows-only right now, because the Linux images do not include the KVP IC
/// daemon.
#[openvmm_test(uefi_x64(vhd(windows_datacenter_core_2022_x64)))]
async fn kvp_ic(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    // Run with a NIC to perform IP address tests.
    let (mut vm, agent) = config.modify_backend(|c| c.with_nic()).run().await?;
    let kvp = vm.backend().wait_for_kvp().await?;

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
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test the timesync IC.
#[openvmm_test(
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2204_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    linux_direct_x64
)]
async fn timesync_ic(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                // Start with the clock half a day in the past so that the clock is
                // initially wrong.
                c.rtc_delta_milliseconds = -(Duration::from_secs(40000).as_millis() as i64)
            })
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
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Validate we can reboot a VM and reconnect to pipette.
// TODO: Reenable guests that use the framebuffer once #74 is fixed.
#[vmm_test(
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
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn reboot<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config.run().await?;
    agent.ping().await?;
    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;
    agent.ping().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test without agent
// TODO: investigate why the shutdown ic doesn't work reliably with hyper-v
// in our ubuntu image
// TODO: re-enable TDX ubuntu tests once issues are resolved (here and below)
#[vmm_test_no_agent(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    openvmm_pcat_x64(vhd(freebsd_13_2_x64)),
    openvmm_pcat_x64(iso(freebsd_13_2_x64)),
    openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_pcat_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
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
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64)),
    // hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2404_server_x64))
)]
async fn boot_no_agent<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

// Basic vp "heavy" boot test without agent with 16 VPs.
#[vmm_test_no_agent(
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64)),
    // hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2404_server_x64))
)]
async fn boot_no_agent_heavy<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let mut vm = config
        .with_processor_topology(ProcessorTopology {
            vp_count: 16,
            ..Default::default()
        })
        .run_without_agent()
        .await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

// Test for vmbus relay
// TODO: VBS isolation was failing and other targets too
#[vmm_test_no_agent(
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64))
)]
#[cfg_attr(not(windows), expect(dead_code))]
async fn vmbus_relay<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let mut vm = config.with_vmbus_redirect(true).run_without_agent().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
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
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
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

// Test for vmbus relay, with MNF enabled via cmdline on TDX.
//
// TODO: Shortened test name to make it work on Hyper-V, but it should use the
// full name once petri is fixed.
#[vmm_test_no_agent(
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64))
)]
#[cfg_attr(not(windows), expect(dead_code))]
async fn vmbr_force_mnf_no_agent<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let mut vm = config
        .with_vmbus_redirect(true)
        .with_openhcl_command_line("OPENHCL_VMBUS_ENABLE_MNF=1")
        .run_without_agent()
        .await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

// Test for vmbus relay
// TODO: VBS isolation was failing and other targets too
#[vmm_test_no_agent(
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64))
)]
#[cfg_attr(not(windows), expect(dead_code))]
async fn vmbus_relay_heavy<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let mut vm = config
        .with_vmbus_redirect(true)
        .with_processor_topology(ProcessorTopology {
            vp_count: 16,
            ..Default::default()
        })
        .run_without_agent()
        .await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test without agent and with a single VP.
#[vmm_test_no_agent(
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64)),
    // hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2404_server_x64))
)]
async fn boot_no_agent_single_proc<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let mut vm = config
        .with_processor_topology(ProcessorTopology {
            vp_count: 1,
            ..Default::default()
        })
        .run_without_agent()
        .await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic reboot test without agent
// TODO: Reenable guests that use the framebuffer once #74 is fixed.
#[vmm_test_no_agent(
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
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64)),
    // hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2404_server_x64))
)]
async fn reboot_no_agent<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Reboot).await?;
    vm.wait_for_reset_no_agent().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot our guest-test UEFI image, which will run some tests,
/// and then purposefully triple fault itself via an expiring
/// watchdog timer.
#[vmm_test_no_agent(
    openvmm_uefi_x64(guest_test_uefi_x64),
    openvmm_uefi_aarch64(guest_test_uefi_aarch64),
    openvmm_openhcl_uefi_x64(guest_test_uefi_x64)
)]
async fn guest_test_uefi<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let vm = config
        .with_windows_secure_boot_template()
        .run_without_agent()
        .await?;
    let arch = vm.arch();
    // No boot event check, UEFI watchdog gets fired before ExitBootServices
    let halt_reason = vm.wait_for_teardown().await?;
    tracing::debug!("vm halt reason: {halt_reason:?}");
    match arch {
        MachineArch::X86_64 => assert!(matches!(halt_reason, PetriHaltReason::TripleFault)),
        MachineArch::Aarch64 => assert!(matches!(halt_reason, PetriHaltReason::Reset)),
    }
    Ok(())
}

/// Test transferring a file to the guest.
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
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
async fn file_transfer_test<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> Result<(), anyhow::Error> {
    const TEST_CONTENT: &str = "hello world!";
    const FILE_NAME: &str = "test.txt";

    let (vm, agent) = config.run().await?;

    agent.write_file(FILE_NAME, TEST_CONTENT.as_bytes()).await?;
    assert_eq!(agent.read_file(FILE_NAME).await?, TEST_CONTENT.as_bytes());

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Boot Linux and have it write the visible memory size.
#[openvmm_test(linux_direct_x64, uefi_aarch64(vhd(ubuntu_2404_server_aarch64)))]
async fn five_gb(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let configured_size = 5 * SIZE_1_GB;
    let expected_size = configured_size - configured_size / 10; // 10% buffer; TODO-figure out where this goes

    let (vm, agent) = config
        .modify_backend(move |b| b.with_custom_config(|c| c.memory.mem_size = configured_size))
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
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Verify that UEFI default boots even if invalid boot entries exist
/// when `default_boot_always_attempt` is enabled.
#[openvmm_test(
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY]
)]
async fn default_boot(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (initial_vmgs,): (ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,),
) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_backing_vmgs(initial_vmgs)
        .modify_backend(|b| b.with_default_boot_always_attempt(true))
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Verify that UEFI successfully boots an operating system after reprovisioning
/// the VMGS when invalid boot entries existed initially.
#[openvmm_test(
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY]
)]
async fn clear_vmgs(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (initial_vmgs,): (ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,),
) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Reprovision)
        .with_backing_vmgs(initial_vmgs)
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Verify that UEFI fails to boot if invalid boot entries exist
///
/// This test exists to ensure we are not getting a false positive for
/// the `default_boot` and `clear_vmgs` test above.
#[openvmm_test_no_agent(
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY]
)]
async fn boot_expect_fail(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (initial_vmgs,): (ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,),
) -> Result<(), anyhow::Error> {
    let vm = config
        .with_expect_boot_failure()
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_backing_vmgs(initial_vmgs)
        .run_without_agent()
        .await?;

    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// MNF guest support: capture and print recursive listing of vmbus drivers.
/// TODO: add entries for CVM guests once MNF support in CVMs is added. Tracked by  #1940
#[openvmm_test(
    openvmm_openhcl_linux_direct_x64,
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
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
