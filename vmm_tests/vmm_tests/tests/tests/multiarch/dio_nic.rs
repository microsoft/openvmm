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
//! NAT and verify a default route exists. Resolve the gateway's MAC via
//! ARP to drive packets through `netvsp` → `DioEndpoint::tx_avail` →
//! `vmswitch` and back, which is the meaningful regression signal for
//! `-net dio`. ICMP echo is intentionally *not* used as the assertion:
//! the Hyper-V Default Switch gateway (the host vNIC) does not reliably
//! answer pings — the host firewall blocks inbound echo by default — so a
//! successful ARP resolution is the reliable, environment-independent
//! signal that bidirectional traffic traversed the DIO datapath.
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
use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use petri::PetriVmBuilder;
use petri::openvmm::NIC_MAC_ADDRESS;
use petri::openvmm::OpenVmmPetriBackend;
use petri::openvmm::find_switch;
use petri::pipette::cmd;
use pipette_client::shell::UnixShell;
use std::time::Duration;
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

/// Parse the link-layer (MAC) address of a resolved neighbor from a
/// single `ip neigh` entry, returning `Some(lladdr)` only when the entry
/// proves an ARP reply was received: it must carry an `lladdr` token and
/// must not be in a negative state (`FAILED`/`INCOMPLETE`). Entries that
/// are still unresolved — or empty output when no entry exists yet —
/// yield `None` so the caller can keep polling.
fn parse_neigh_lladdr(neigh: &str) -> Option<String> {
    // Reject negative states even if a stale lladdr happens to be present.
    if neigh.contains("INCOMPLETE") || neigh.contains("FAILED") {
        return None;
    }
    let mut tokens = neigh.split_whitespace();
    let mut lladdr = None;
    while let Some(tok) = tokens.next() {
        if tok == "lladdr" {
            lladdr = tokens.next().map(|s| s.to_string());
        }
    }
    lladdr
}

/// End-to-end test for `-net dio`.
#[openvmm_test(uefi_x64(vhd(ubuntu_2504_server_x64)))]
async fn dio_nic(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
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

    // Configure systemd-networkd to DHCP this interface and reload. The
    // Ubuntu cloud image runs systemd-networkd as its default network
    // manager, and cloud-init's network-config drop-in for `eth0` may
    // have raced the link bring-up of the DIO NIC, so we install an
    // explicit drop-in matched by name and ask networkd to reconfigure.
    cmd!(sh, "ip link set {iface} up").run().await?;
    let drop_in = format!("[Match]\nName={iface}\n\n[Network]\nDHCP=ipv4\n");
    cmd!(sh, "mkdir -p /run/systemd/network").run().await?;
    cmd!(sh, "tee /run/systemd/network/99-petri-dio.network")
        .stdin(drop_in)
        .ignore_stdout()
        .run()
        .await
        .context("failed to write systemd-networkd drop-in")?;
    cmd!(sh, "networkctl reload")
        .run()
        .await
        .context("networkctl reload failed")?;
    cmd!(sh, "networkctl reconfigure {iface}")
        .run()
        .await
        .context("networkctl reconfigure failed")?;

    // Reconfiguration is asynchronous. Wait for networkd to finish the
    // DHCP transaction, including installing its routes, before inspecting
    // the resulting network state.
    cmd!(
        sh,
        "/usr/lib/systemd/systemd-networkd-wait-online --interface={iface}:routable --ipv4 --timeout=30"
    )
    .run()
    .await
    .context("systemd-networkd did not configure the DIO-backed NIC")?;

    let addr = cmd!(sh, "ip -4 -br addr show {iface}").read().await?;
    anyhow::ensure!(
        addr.split_whitespace()
            .any(|w| w.contains('/') && w.contains('.')),
        "no IPv4 lease on {iface}; current addrs: {addr}"
    );
    tracing::info!(addr, "ipv4 lease on DIO-backed NIC");

    let route = cmd!(sh, "ip route show default").read().await?;
    tracing::info!(route, "default route");
    let gw = parse_default_gw(&route, &iface)?;
    tracing::info!(gw, "resolving gateway via DIO");

    // Drive ARP resolution of the gateway across the DIO datapath. This is
    // the meaningful regression signal: resolving the gateway's MAC pushes
    // an ARP request through the guest netvsc → host netvsp → DioEndpoint →
    // vmswitch path and requires a reply to come back along the same path.
    //
    // We deliberately do not assert on ICMP echo replies: the Hyper-V
    // Default Switch gateway (the host vNIC) does not reliably answer pings
    // and the host firewall blocks inbound echo by default. The ping below
    // is only used to provoke neighbor resolution, so its exit status is
    // ignored — the assertion is on the resulting neighbor entry.
    let mut timer = PolledTimer::new(&driver);
    let mut neigh = String::new();
    let mut gw_mac = None;
    for _ in 0..15 {
        cmd!(sh, "ping -c 1 -W 1 -I {iface} {gw}")
            .ignore_status()
            .ignore_stdout()
            .run()
            .await?;
        neigh = cmd!(sh, "ip neigh show {gw} dev {iface}").read().await?;
        if let Some(mac) = parse_neigh_lladdr(&neigh) {
            gw_mac = Some(mac);
            break;
        }
        timer.sleep(Duration::from_secs(1)).await;
    }
    let gw_mac = gw_mac.with_context(|| {
        format!(
            "gateway {gw} not ARP-reachable via {iface} through DIO; last neigh entry: {neigh:?}"
        )
    })?;
    tracing::info!(gw, gw_mac, "gateway resolved via DIO (ARP reply received)");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn neigh_reachable_yields_lladdr() {
        let line = "172.22.160.1 dev eth0 lladdr 00:15:5d:01:02:03 REACHABLE";
        assert_eq!(
            super::parse_neigh_lladdr(line).as_deref(),
            Some("00:15:5d:01:02:03")
        );
    }

    #[test]
    fn neigh_stale_still_yields_lladdr() {
        // STALE entries still prove a reply was once received.
        let line = "172.22.160.1 dev eth0 lladdr 00:15:5d:0a:0b:0c STALE";
        assert_eq!(
            super::parse_neigh_lladdr(line).as_deref(),
            Some("00:15:5d:0a:0b:0c")
        );
    }

    #[test]
    fn neigh_incomplete_is_unresolved() {
        assert_eq!(
            super::parse_neigh_lladdr("172.22.160.1 dev eth0 INCOMPLETE"),
            None
        );
    }

    #[test]
    fn neigh_failed_is_unresolved() {
        // A stale lladdr paired with FAILED must not count as resolved.
        let line = "172.22.160.1 dev eth0 lladdr 00:15:5d:01:02:03 FAILED";
        assert_eq!(super::parse_neigh_lladdr(line), None);
    }

    #[test]
    fn neigh_empty_is_unresolved() {
        assert_eq!(super::parse_neigh_lladdr(""), None);
    }

    #[test]
    fn default_gw_requires_exact_dev_match() {
        let route = "default via 172.22.160.1 dev eth0 proto dhcp src 172.22.160.34 metric 100";
        assert_eq!(
            super::parse_default_gw(route, "eth0").unwrap(),
            "172.22.160.1"
        );
        // A sibling interface whose name is a substring must not match.
        assert!(super::parse_default_gw(route, "eth0.100").is_err());
    }
}
