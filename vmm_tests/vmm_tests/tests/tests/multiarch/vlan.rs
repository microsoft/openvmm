// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for VLAN (802.1Q) sub-interface configuration on virtual NICs.
//!
//! These tests validate that guest operating systems can create and configure
//! VLAN sub-interfaces on the VMM's synthetic NIC (netvsp). They exercise
//! the guest driver's VLAN support and verify the TX path with VLAN PPI
//! metadata does not error or crash.
//!
//! **Scope:** These tests validate guest-side VLAN configuration and TX smoke
//! behavior. Full end-to-end VLAN datapath validation (verifying that the VMM
//! backend correctly processes VLAN-tagged traffic) would require a
//! VLAN-aware backend; the current consomme backend ignores VLAN metadata.
//! Unit-level VLAN PPI parsing is covered by `netvsp/src/test.rs`.

use anyhow::Context;
use petri::PetriVmBuilder;
use petri::openvmm::NIC_MAC_ADDRESS;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use pipette_client::shell::UnixShell;
use vmm_test_macros::openvmm_test;

/// Find the network interface matching [`NIC_MAC_ADDRESS`] by scanning sysfs.
async fn find_nic_by_mac(sh: &UnixShell<'_>) -> anyhow::Result<String> {
    let expected_mac = NIC_MAC_ADDRESS.to_string().replace('-', ":");
    let ifaces = cmd!(sh, "ls /sys/class/net").read().await?;
    for iface in ifaces.lines() {
        let iface = iface.trim();
        if iface.is_empty() {
            continue;
        }
        let addr_path = format!("/sys/class/net/{iface}/address");
        if let Ok(mac) = cmd!(sh, "cat {addr_path}").read().await {
            if mac.trim() == expected_mac {
                return Ok(iface.to_string());
            }
        }
    }
    anyhow::bail!("no interface found with MAC address {expected_mac}")
}

/// Test VLAN sub-interface creation and configuration on the guest NIC.
///
/// Validates that the guest can:
/// 1. Create an 802.1Q VLAN sub-interface on the synthetic NIC
/// 2. Configure it with a specific VLAN ID, IP address, and bring it up
/// 3. Transmit packets through it (TX smoke test via ARP/ping)
/// 4. Maintain the parent interface in operational state throughout
///
/// The TX smoke step exercises the netvsp VLAN PPI (Per-Packet Information)
/// path: the guest's netvsc driver emits VLAN metadata that netvsp extracts
/// into `TxMetadata`. The ping itself is expected to fail because the
/// consomme backend does not route VLAN-tagged traffic, but the TX operation
/// must not error or crash.
#[openvmm_test(
    uefi_x64(vhd(ubuntu_2504_server_x64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn vlan_guest_config(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config.modify_backend(|c| c.with_nic()).run().await?;
    let sh = agent.unix_shell();

    // Find the NIC interface by its known MAC address.
    let nic_name = find_nic_by_mac(&sh).await?;
    tracing::info!(nic_name, "found NIC interface");

    // Ensure the parent interface is up.
    cmd!(sh, "ip link set {nic_name} up").run().await?;

    // Load the 8021q kernel module for VLAN support. This will actually get
    // put to the test with the following `ip` commands, so modprobe is only
    // a best-effort action.
    cmd!(sh, "modprobe 8021q").ignore_status().run().await?;

    // Create a VLAN sub-interface with VLAN ID 100.
    let vlan_id = "100";
    let vlan_iface = format!("{nic_name}.{vlan_id}");
    cmd!(
        sh,
        "ip link add link {nic_name} name {vlan_iface} type vlan id {vlan_id}"
    )
    .run()
    .await?;

    // Verify the VLAN interface was created with correct 802.1Q configuration.
    let vlan_info = cmd!(sh, "ip -d link show {vlan_iface}").read().await?;
    tracing::info!(vlan_info, "VLAN interface details");
    assert!(
        vlan_info.contains("vlan protocol 802.1Q"),
        "interface should use 802.1Q VLAN protocol, got: {vlan_info}"
    );
    assert!(
        vlan_info.contains(&format!("id {vlan_id}")),
        "VLAN ID should be {vlan_id}, got: {vlan_info}"
    );

    // Configure the VLAN interface with an IP address and bring it up.
    cmd!(sh, "ip addr add 10.100.0.2/24 dev {vlan_iface}")
        .run()
        .await?;
    cmd!(sh, "ip link set {vlan_iface} up").run().await?;

    // Verify the VLAN interface is up.
    let link_brief = cmd!(sh, "ip -br link show {vlan_iface}").read().await?;
    tracing::info!(link_brief, "VLAN interface link state");
    assert!(
        link_brief.contains("UP"),
        "VLAN interface should be in UP state, got: {link_brief}"
    );

    // Verify the IP address was assigned.
    let addr_info = cmd!(sh, "ip -br addr show {vlan_iface}").read().await?;
    assert!(
        addr_info.contains("10.100.0.2"),
        "VLAN interface should have the assigned IP address, got: {addr_info}"
    );

    // TX smoke test: send traffic through the VLAN interface. This exercises
    // the netvsc → netvsp path with VLAN PPI metadata. The ping will fail
    // (consomme doesn't handle VLAN-tagged ARP), but the TX must not crash.
    let baseline_tx_packets: u64 =
        cmd!(sh, "cat /sys/class/net/{vlan_iface}/statistics/tx_packets")
            .read()
            .await?
            .trim()
            .parse()
            .context("failed to parse tx_packets")?;

    let _ = cmd!(sh, "ping -I {vlan_iface} -c 1 -W 2 10.100.0.1")
        .read()
        .await;

    // Verify that at least one packet was transmitted through the VLAN
    // interface (the ARP request for the ping target).
    let tx_packets: u64 = cmd!(sh, "cat /sys/class/net/{vlan_iface}/statistics/tx_packets")
        .read()
        .await?
        .trim()
        .parse()
        .context("failed to parse tx_packets")?;

    assert!(
        tx_packets > baseline_tx_packets,
        "expected at least one TX packet through the VLAN interface"
    );

    // Verify the parent interface is still operational.
    let parent_state = cmd!(sh, "ip -br link show {nic_name}").read().await?;
    assert!(
        parent_state.contains("UP"),
        "parent interface should remain UP after VLAN operations, got: {parent_state}"
    );

    // Clean up: remove the VLAN interface.
    cmd!(sh, "ip link del {vlan_iface}").run().await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
