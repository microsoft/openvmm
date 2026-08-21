// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MANA integration tests for x86_64 Linux direct boot with OpenHCL.

use petri::OpenvmmLogConfig;
use petri::PetriVmBuilder;
use petri::openvmm::ManaTestControl;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri::pipette::cmd;
use vmm_test_macros::openvmm_test;

fn configure_mana_vf_diagnostics(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> PetriVmBuilder<OpenVmmPetriBackend> {
    config
        .with_vtl0_kernel_command_line(
            "rcupdate.rcu_cpu_stall_timeout=10 rcupdate.rcu_cpu_stall_cputime=1 mana.dyndbg=+p hv_netvsc.dyndbg=+p pci_hyperv.dyndbg=+p",
        )
        .with_host_log_levels(OpenvmmLogConfig::Custom(
            [
                (
                    "OPENVMM_LOG".to_owned(),
                    "debug,gdma=trace,vpci=trace,hv1_emulator::message_queues=trace".to_owned(),
                ),
                ("OPENVMM_SHOW_SPANS".to_owned(), "true".to_owned()),
            ]
            .into(),
        ))
        .with_openhcl_log_levels(OpenvmmLogConfig::Custom(
            [
                (
                    "OPENVMM_LOG".to_owned(),
                    "debug,underhill_core::emuplat::netvsp=trace,netvsp=trace,mana_driver=trace"
                        .to_owned(),
                ),
                ("OPENVMM_SHOW_SPANS".to_owned(), "true".to_owned()),
            ]
            .into(),
        ))
}

/// Validates that the nic can get an IP address via consomme's DHCP implementation.
/// Validates ICMP by testing that the nic can ping consomme's IP address.
///
/// FUTURE: TCP / UDP traffic?
async fn validate_mana_nic(
    agent: &PipetteClient,
    eth0_is_mana_vf: bool,
) -> Result<(), anyhow::Error> {
    let sh = agent.unix_shell();
    cmd!(sh, "ifconfig eth0 up").run().await?;
    cmd!(sh, "udhcpc eth0").run().await?;
    let output = cmd!(sh, "ifconfig eth0").read().await?;
    // Validate that we see a mana nic with the expected MAC address and IPs.
    assert!(output.contains("HWaddr 00:15:5D:12:12:12"));
    assert!(output.contains("inet addr:10.0.0.2"));
    if eth0_is_mana_vf {
        cmd!(sh, "ifconfig eth1").ignore_status().run().await?;
    } else {
        assert!(output.contains("inet6 addr: fe80::215:5dff:fe12:1212/64"));
    }
    cmd!(sh, "ping -c 1 -W 5 -I eth0 10.0.0.1").run().await?;

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

    validate_mana_nic(&agent, false).await?;

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

    validate_mana_nic(&agent, false).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

async fn mana_nic_vf_reconfig(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    revoke_vtl0_vf: bool,
) -> Result<(), anyhow::Error> {
    let (mana, mana_config) = ManaTestControl::new();
    let config = configure_mana_vf_diagnostics(config)
        .with_vmbus_redirect(true)
        .modify_backend(move |b| b.with_nic_test_control(mana_config));

    let (vm, agent) = config.run().await?;

    validate_mana_nic(&agent, true).await?;

    let sh = agent.unix_shell();

    mana.inject_vf_reset(revoke_vtl0_vf).await?;
    cmd!(sh, "sleep 5").run().await?;
    validate_mana_nic(&agent, true).await?;

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
    let config = configure_mana_vf_diagnostics(config)
        .with_vmbus_redirect(true)
        .modify_backend(move |b| b.with_nic_test_control(mana_config));

    let (vm, agent) = config.run().await?;
    validate_mana_nic(&agent, true).await?;

    let sh = agent.unix_shell();
    mana.set_vport_link_state(0, false).await?;
    cmd!(
        sh,
        "timeout 30 sh -c 'until [ \"$(cat /sys/class/net/eth0/carrier)\" = 0 ]; do sleep 1; done'"
    )
    .run()
    .await?;

    mana.set_vport_link_state(0, true).await?;
    cmd!(
        sh,
        "timeout 30 sh -c 'until [ \"$(cat /sys/class/net/eth0/carrier)\" = 1 ]; do sleep 1; done'"
    )
    .run()
    .await?;
    validate_mana_nic(&agent, true).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
