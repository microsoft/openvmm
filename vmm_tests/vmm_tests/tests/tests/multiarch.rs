// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests that run on more than one architecture.

use anyhow::Context as _;
use hyperv_ic_resources::kvp::KvpRpc;
use mesh::rpc::RpcSend as _;
use petri::PetriVmConfig;
use petri::openvmm::NIC_MAC_ADDRESS;
use petri::openvmm::PetriVmConfigOpenVmm;
use vmm_core_defs::HaltReason;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;

/// Boot through the UEFI firmware, it will shut itself down after booting.
#[openvmm_test(uefi_x64(none), openhcl_uefi_x64(none), uefi_aarch64(none))]
async fn frontpage(config: PetriVmConfigOpenVmm) -> anyhow::Result<()> {
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
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_pcat_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))
)]
async fn boot(config: Box<dyn PetriVmConfig>) -> anyhow::Result<()> {
    let (vm, agent) = config.run().await?;
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
    assert_eq!(ip_info.ipv4_addresses.len(), 1);
    let ip = &ip_info.ipv4_addresses[0];
    assert_eq!(ip.address.to_string(), "10.0.0.2");
    assert_eq!(ip.subnet.to_string(), "255.255.255.0");
    assert_eq!(ip_info.ipv4_gateways.len(), 1);
    let gateway = &ip_info.ipv4_gateways[0];
    assert_eq!(gateway.to_string(), "10.0.0.1");

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}
