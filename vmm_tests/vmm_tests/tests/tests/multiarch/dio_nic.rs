// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end test for the Windows vmswitch DirectIO (`-net dio`)
//! network backend.
//!
//! This is the only whole-VM test that exercises the `net_dio` endpoint,
//! resolver, and queue, and the vmswitch `SwitchPort` interop. All other
//! petri NIC helpers go through the userspace `Consomme` backend.
//!
//! **Scope:** Boot a Linux UEFI guest with a synthetic NIC bridged to a
//! Hyper-V vmswitch via DirectIO. DHCP an IPv4 lease from the switch's
//! NAT and verify a default route exists. Ping the gateway to drive
//! packets through `netvsp` → `DioEndpoint::tx_avail` → `vmswitch` and
//! back, which is the meaningful regression signal for `-net dio`.
//!
//! **Host requirements:** Windows host with Hyper-V installed and at
//! least one vmswitch (the Default Switch by preference). The test
//! discovers a switch at runtime — preferring the well-known Default
//! Switch GUID, falling back to the first switch reported by HCN — and
//! fails fast (rather than silently skipping) when no switch can be
//! found, so that a missing Hyper-V install in CI is reported as a
//! regression instead of being mistaken for success. On non-Windows
//! hosts the test is gated out at compile time.

#![cfg(windows)]

use anyhow::Context;
use petri::PetriVmBuilder;
use petri::openvmm::NIC_MAC_ADDRESS;
use petri::openvmm::OpenVmmPetriBackend;
use petri::openvmm::find_switch;
use petri::pipette::cmd;
use pipette_client::shell::UnixShell;
use vmm_test_macros::openvmm_test;

/// Find the network interface matching [`NIC_MAC_ADDRESS`] by scanning
/// sysfs.
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
            if mac.trim().eq_ignore_ascii_case(&expected_mac) {
                return Ok(iface.to_string());
            }
        }
    }
    anyhow::bail!("no interface found with MAC address {expected_mac}")
}

/// Parse the IPv4 gateway from `ip route show default` output, requiring
/// an exact `dev <iface>` match so we do not pick up a sibling interface
/// whose name is a substring of `iface` (e.g. `eth0` vs `eth0.100`).
fn parse_default_gw(route: &str, iface: &str) -> anyhow::Result<String> {
    for line in route.lines() {
        let mut tokens = line.split_whitespace();
        let mut gateway: Option<&str> = None;
        let mut dev_matches = false;
        while let Some(tok) = tokens.next() {
            match tok {
                "via" => gateway = tokens.next(),
                "dev" => {
                    if tokens.next() == Some(iface) {
                        dev_matches = true;
                    }
                }
                _ => {}
            }
        }
        if dev_matches {
            if let Some(gw) = gateway {
                return Ok(gw.to_string());
            }
        }
    }
    anyhow::bail!("no default route via {iface} found in: {route}")
}

/// End-to-end test for `-net dio`.
#[openvmm_test(uefi_x64(vhd(ubuntu_2504_server_x64)))]
async fn dio_nic(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let switch = find_switch().ok_or_else(|| {
        anyhow::anyhow!(
            "no Hyper-V vmswitch could be opened on this host (Default Switch absent and \
             HCN enumeration returned nothing); DIO test cannot run. If the runner \
             intentionally lacks Hyper-V, exclude this test by filter rather than letting \
             it silently no-op."
        )
    })?;
    tracing::info!(%switch, "using vmswitch for DIO test");

    let (vm, agent) = config
        .modify_backend(move |c| c.with_dio_nic(Some(switch)))
        .run()
        .await?;
    let sh = agent.unix_shell();

    let iface = find_nic_by_mac(&sh).await?;
    tracing::info!(iface, "found DIO-backed NIC interface");

    // Bring the interface up and request a DHCP lease from the Default
    // Switch's NAT. The image ships busybox `udhcpc`.
    cmd!(sh, "ip link set {iface} up").run().await?;
    cmd!(sh, "udhcpc -i {iface} -q -f -n -t 10 -T 3")
        .run()
        .await
        .context("DHCP failed on DIO-backed NIC")?;

    let addr = cmd!(sh, "ip -4 -br addr show {iface}").read().await?;
    tracing::info!(addr, "ipv4 lease on DIO-backed NIC");
    assert!(
        addr.contains('/'),
        "expected an IPv4 lease on {iface}, got: {addr}"
    );

    let route = cmd!(sh, "ip route show default").read().await?;
    tracing::info!(route, "default route");
    let gw = parse_default_gw(&route, &iface)?;
    tracing::info!(gw, "pinging gateway");

    // The ping is the meaningful regression signal: it pushes packets
    // through the guest netvsc → host netvsp → DioEndpoint → vmswitch
    // path and validates a response comes back.
    cmd!(sh, "ping -c 3 -W 5 -I {iface} {gw}")
        .run()
        .await
        .context("ping to gateway via DIO failed")?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
