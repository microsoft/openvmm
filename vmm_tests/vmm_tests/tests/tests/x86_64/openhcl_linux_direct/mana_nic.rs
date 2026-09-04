// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MANA integration tests for x86_64 Linux direct boot with OpenHCL.

use petri::PetriVmBuilder;
use petri::openvmm::ManaTestControl;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri::pipette::cmd;
use vmm_test_macros::openvmm_test;

/// Get an IP address via consomme's DHCP implementation.
async fn configure_mana_nic(agent: &PipetteClient, interface: &str) -> Result<(), anyhow::Error> {
    let sh = agent.unix_shell();
    cmd!(sh, "ifconfig {interface} up").run().await?;
    cmd!(sh, "udhcpc -i {interface}").run().await?;

    Ok(())
}

/// Validates ICMP by testing that the nic can ping consomme's IP address.
///
/// FUTURE: TCP / UDP traffic?
async fn validate_mana_nic(agent: &PipetteClient, interface: &str) -> Result<(), anyhow::Error> {
    let sh = agent.unix_shell();
    let output = cmd!(sh, "ifconfig {interface}").read().await?;
    // Validate that we see a mana nic with the expected MAC address and IPs.
    assert!(output.contains("HWaddr 00:15:5D:12:12:12"));
    assert!(output.contains("inet addr:10.0.0.2"));
    cmd!(sh, "ping -c 1 -W 5 -I {interface} 10.0.0.1")
        .run()
        .await?;

    Ok(())
}

async fn validate_vtl0_mana_vf(agent: &PipetteClient) -> Result<(), anyhow::Error> {
    let sh = agent.unix_shell();
    let vf_output = cmd!(sh, "ifconfig eth0").read().await?;
    assert!(vf_output.contains("HWaddr 00:15:5D:12:12:12"));
    let vf_master = cmd!(sh, "readlink /sys/class/net/eth0/master")
        .read()
        .await?;
    assert_eq!(vf_master.rsplit('/').next(), Some("eth1"));

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

    configure_mana_nic(&agent, "eth0").await?;
    validate_mana_nic(&agent, "eth0").await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a MANA nic assigned to VTL2 (backed by
/// the MANA emulator), and vmbus relay.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn mana_nic_shared_pool(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(|b| b.with_nic())
        .run()
        .await?;

    configure_mana_nic(&agent, "eth0").await?;
    validate_mana_nic(&agent, "eth0").await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

#[openvmm_test(openhcl_linux_direct_x64)]
async fn mana_nic_with_vtl0_vf(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let (mana, mana_config) = ManaTestControl::new();
    let config = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| b.with_nic_test_control(mana_config));

    let (vm, agent) = config.run().await?;
    configure_mana_nic(&agent, "eth1").await?;
    validate_mana_nic(&agent, "eth1").await?;
    validate_vtl0_mana_vf(&agent).await?;

    mana.shutdown().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

#[openvmm_test(openhcl_linux_direct_x64)]
async fn mana_nic_unbind_mana_driver(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let (mana, mana_config) = ManaTestControl::new();
    let config = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| b.with_nic_test_control(mana_config));

    let (vm, agent) = config.run().await?;
    configure_mana_nic(&agent, "eth1").await?;
    validate_mana_nic(&agent, "eth1").await?;
    validate_vtl0_mana_vf(&agent).await?;

    let sh = agent.unix_shell();
    cmd!(sh, "sh")
    .args([
        "-c",
        "bdf=$(basename $(readlink -f /sys/class/net/eth0/device)); echo $bdf > /sys/bus/pci/drivers/mana/unbind",
    ])
    .run()
    .await?;
    cmd!(sh, "test ! -e /sys/class/net/eth0").run().await?;

    mana.shutdown().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

async fn mana_nic_vf_reconfig(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    revoke_vtl0_vf: bool,
) -> Result<(), anyhow::Error> {
    let (mana, mana_config) = ManaTestControl::new();
    let config = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| b.with_nic_test_control(mana_config));

    let (vm, agent) = config.run().await?;

    configure_mana_nic(&agent, "eth1").await?;
    validate_mana_nic(&agent, "eth1").await?;
    validate_vtl0_mana_vf(&agent).await?;

    let sh = agent.unix_shell();

    mana.inject_vf_reset(revoke_vtl0_vf).await?;
    cmd!(
        sh,
        "timeout 30 sh -c 'until [ \"$(basename \"$(readlink /sys/class/net/eth0/master)\")\" = eth1 ] && [ \"$(cat /sys/class/net/eth1/carrier)\" = 1 ] && ping -c 1 -W 1 -I eth1 10.0.0.1 >/dev/null 2>&1; do sleep 1; done'"
    )
    .run()
    .await?;
    validate_mana_nic(&agent, "eth1").await?;
    validate_vtl0_mana_vf(&agent).await?;

    mana.shutdown().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test VF reconfiguration while retaining the VTL0 VF.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn mana_nic_vf_reconfig_keep_vtl0_vf(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    mana_nic_vf_reconfig(config, false).await
}

/// Test VF reconfiguration while revoking the VTL0 VF.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn mana_nic_vf_reconfig_revoke_vtl0_vf(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    mana_nic_vf_reconfig(config, true).await
}

/// Test guest-visible vport link disconnect and reconnect events.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn mana_nic_vport_link_state(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let (mana, mana_config) = ManaTestControl::new();
    let config = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| b.with_nic_test_control(mana_config));

    let (vm, agent) = config.run().await?;
    configure_mana_nic(&agent, "eth1").await?;
    validate_mana_nic(&agent, "eth1").await?;
    validate_vtl0_mana_vf(&agent).await?;

    let sh = agent.unix_shell();
    mana.set_vport_link_state(0, false).await?;
    cmd!(
        sh,
        "timeout 30 sh -c 'until [ \"$(cat /sys/class/net/eth1/carrier)\" = 0 ]; do sleep 1; done'"
    )
    .run()
    .await?;

    mana.set_vport_link_state(0, true).await?;
    cmd!(
        sh,
        "timeout 30 sh -c 'until [ \"$(cat /sys/class/net/eth1/carrier)\" = 1 ]; do sleep 1; done'"
    )
    .run()
    .await?;
    validate_mana_nic(&agent, "eth1").await?;
    validate_vtl0_mana_vf(&agent).await?;

    mana.shutdown().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
