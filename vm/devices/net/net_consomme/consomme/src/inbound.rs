// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Admission of the peers that reach host listeners.
//!
//! An explicit port forward admits every peer. A listener that publishes a
//! guest port to the host's private networks admits only peers whose
//! addresses are not globally reachable, and the host itself only when the
//! host may reach the guest. A listener that publishes a guest port on host
//! loopback alone admits only the host. The host is a peer on loopback, at
//! the address that it connected to, or at one of the host's own unicast
//! addresses. Admission refuses peers that the guest could not answer
//! unambiguously: those in the guest's own networks, which the guest takes for
//! its gateway or its neighbors, and IPv6 link-local peers, whose replies
//! would leave from the guest's link-local address and lose their interface
//! scope.

use crate::ConsommeParams;
#[cfg(unix)]
use crate::unix as platform;
#[cfg(windows)]
use crate::windows as platform;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::time::Duration;
use std::time::Instant;

/// How long a list of the host's addresses remains current.
const HOST_ADDRESS_LIFETIME: Duration = Duration::from_secs(1);

/// The peers that a host listener admits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundAdmission {
    /// Every peer, as for an explicit port forward.
    Any,
    /// Peers on the host's private networks, whose addresses
    /// [`is_private_network_address`] accepts. The listener publishes a guest
    /// port: the guest's datagrams from that port leave through it.
    PrivateNetwork {
        /// Whether to admit the host itself.
        host: bool,
    },
    /// The host alone, as for a listener bound to host loopback that publishes
    /// a guest port: the guest's datagrams from that port leave through it.
    Host,
}

impl InboundAdmission {
    /// Returns whether the listener publishes a guest port, to the host's
    /// private networks or to the host alone.
    pub(crate) fn publishes_guest_port(self) -> bool {
        matches!(self, Self::PrivateNetwork { .. } | Self::Host)
    }

    /// Returns whether to admit `peer`, which reached the listener at
    /// `local` when that address is known, to a guest with `params`.
    pub(crate) fn admits(
        self,
        peer: IpAddr,
        local: Option<IpAddr>,
        params: &ConsommeParams,
        host_addresses: &mut HostAddresses,
    ) -> bool {
        let (private_networks, host) = match self {
            Self::Any => return true,
            Self::PrivateNetwork { host } => (true, host),
            Self::Host => (false, true),
        };
        let peer = unmapped(peer);
        if params.is_guest_network_address(peer)
            || matches!(peer, IpAddr::V6(address) if address.is_unicast_link_local())
        {
            return false;
        }
        if peer.is_loopback() || local.map(unmapped) == Some(peer) {
            return host;
        }
        match host_addresses.contains(peer) {
            Some(true) => host,
            Some(false) => private_networks && is_private_network_address(peer),
            // Any peer might be the host, so a private-network peer is
            // admitted only when the host would be.
            None => private_networks && host && is_private_network_address(peer),
        }
    }
}

/// Returns `address`, or the IPv4 address that an IPv4-mapped IPv6 address
/// embeds.
fn unmapped(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
        IpAddr::V4(_) => address,
    }
}

/// Returns whether `address` is a unicast address of a private network: one
/// of the special-purpose ranges that IANA does not list as globally
/// reachable, other than loopback.
///
/// These are the IPv4 private (RFC 1918), shared (RFC 6598), link-local (RFC
/// 3927), benchmarking (RFC 2544), and documentation (RFC 5737) ranges, and
/// the IPv6 unique local (RFC 4193), link-local and deprecated site-local (RFC
/// 4291), documentation (RFC 3849), and benchmarking (RFC 5180) ranges. An
/// IPv4-mapped IPv6 address is classified by the IPv4 address that it embeds.
pub fn is_private_network_address(address: IpAddr) -> bool {
    match unmapped(address) {
        IpAddr::V4(address) => is_private_ipv4(address),
        IpAddr::V6(address) => is_private_ipv6(address),
    }
}

fn is_private_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    first == 10
        || (first == 172 && second & 0xf0 == 16)
        || (first == 192 && second == 168)
        || (first == 100 && second & 0xc0 == 64)
        || (first == 169 && second == 254)
        || (first == 198 && second & 0xfe == 18)
        || (first == 192 && second == 0 && third == 2)
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
}

fn is_private_ipv6(address: Ipv6Addr) -> bool {
    let [first, second, third, ..] = address.segments();
    first & 0xfe00 == 0xfc00
        || first & 0xffc0 == 0xfe80
        || first & 0xffc0 == 0xfec0
        || (first == 0x2001 && second == 0x0db8)
        || (first == 0x2001 && second == 0x0002 && third == 0)
}

/// The host's own unicast addresses, listed again once they are older than
/// [`HOST_ADDRESS_LIFETIME`].
pub(crate) struct HostAddresses {
    /// The addresses, or `None` when they could not be listed.
    addresses: Option<Vec<IpAddr>>,
    listed: Option<Instant>,
}

impl HostAddresses {
    pub(crate) fn new() -> Self {
        Self {
            addresses: Some(Vec::new()),
            listed: None,
        }
    }

    /// Returns whether `address` belongs to the host, or `None` when the
    /// host's addresses cannot be listed.
    fn contains(&mut self, address: IpAddr) -> Option<bool> {
        let now = Instant::now();
        if self
            .listed
            .is_none_or(|listed| now.duration_since(listed) >= HOST_ADDRESS_LIFETIME)
        {
            self.addresses = match platform::host_unicast_addresses() {
                Ok(addresses) => Some(addresses),
                Err(error) => {
                    tracelimit::warn_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "failed to list the host's addresses; treating every private-network peer as the host"
                    );
                    None
                }
            };
            self.listed = Some(now);
        }
        self.addresses.as_ref().map(|addresses| {
            addresses
                .iter()
                .any(|&host_address| unmapped(host_address) == address)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn private_network_addresses_are_not_globally_reachable() {
        for private in [
            "10.0.0.1",
            "10.255.255.254",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.50",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.1.1",
            "198.18.0.1",
            "198.19.2.30",
            "192.0.2.7",
            "198.51.100.7",
            "203.0.113.7",
            "fd00::a00:9",
            "fc00::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "2001:db8::7",
            "2001:2::1",
            "::ffff:192.168.1.50",
        ] {
            assert!(is_private_network_address(address(private)), "{private}");
        }
        for global in [
            "1.1.1.1",
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.1",
            "192.169.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "198.17.255.255",
            "198.20.0.1",
            "192.0.0.9",
            "192.88.99.1",
            "203.0.114.1",
            "127.0.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "2001:4860::8888",
            "2001::1",
            "2001:3::1",
            "2002:c000:201::1",
            "64:ff9b::808:808",
            "::1",
            "::",
            "ff02::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_private_network_address(address(global)), "{global}");
        }
    }

    /// Returns the parameters of a guest at `10.0.0.2/24` and
    /// `fd00::a00:2/120`.
    fn guest_params() -> ConsommeParams {
        let mut params = ConsommeParams::new().unwrap();
        params
            .set_static_ipv6(
                "fd00::a00:2".parse().unwrap(),
                120,
                "fd00::a00:1".parse().unwrap(),
            )
            .unwrap();
        params
    }

    #[test]
    fn admission_separates_the_host_from_its_private_networks() {
        let params = guest_params();
        let mut host_addresses = HostAddresses {
            addresses: Some(vec![address("192.168.1.10"), address("fd12::10")]),
            listed: Some(Instant::now()),
        };
        let lan = InboundAdmission::PrivateNetwork { host: false };
        let lan_and_host = InboundAdmission::PrivateNetwork { host: true };
        let local = Some(address("192.168.1.10"));
        for (peer, from_host, private) in [
            ("192.168.1.50", false, true),
            ("::ffff:192.168.1.50", false, true),
            ("fd12::50", false, true),
            ("8.8.8.8", false, false),
            ("2001:4860::8888", false, false),
            ("127.0.0.1", true, true),
            ("::1", true, true),
            ("::ffff:127.0.0.1", true, true),
            ("192.168.1.10", true, true),
            ("fd12::10", true, true),
        ] {
            let peer = address(peer);
            assert!(InboundAdmission::Any.admits(peer, local, &params, &mut host_addresses));
            assert_eq!(
                lan.admits(peer, local, &params, &mut host_addresses),
                private && !from_host,
                "{peer}"
            );
            assert_eq!(
                lan_and_host.admits(peer, local, &params, &mut host_addresses),
                private || from_host,
                "{peer}"
            );
        }

        // The address that a peer connected to belongs to the host, even when
        // the host's addresses do not list it.
        let peer = address("10.1.2.3");
        assert!(lan.admits(peer, None, &params, &mut host_addresses));
        assert!(!lan.admits(peer, Some(peer), &params, &mut host_addresses));
        assert!(lan_and_host.admits(peer, Some(peer), &params, &mut host_addresses));
    }

    #[test]
    fn admission_refuses_peers_that_the_guest_cannot_answer() {
        let params = guest_params();
        let mut host_addresses = HostAddresses {
            addresses: Some(vec![address("10.0.0.1"), address("fe80::10")]),
            listed: Some(Instant::now()),
        };
        // Peers in the guest's networks, including the host at the gateway's
        // address, and IPv6 link-local peers, including the host's own.
        for peer in [
            "10.0.0.1",
            "10.0.0.2",
            "10.0.0.77",
            "::ffff:10.0.0.77",
            "fd00::a00:1",
            "fd00::a00:ff",
            "fe80::1",
            "fe80::10",
        ] {
            let peer = address(peer);
            for host in [false, true] {
                assert!(
                    !InboundAdmission::PrivateNetwork { host }.admits(
                        peer,
                        Some(peer),
                        &params,
                        &mut host_addresses
                    ),
                    "{peer}"
                );
            }
            assert!(InboundAdmission::Any.admits(peer, None, &params, &mut host_addresses));
        }
        // The neighboring networks stay private networks.
        for peer in ["10.0.1.1", "fd00::a01:1", "169.254.1.1"] {
            assert!(
                InboundAdmission::PrivateNetwork { host: false }.admits(
                    address(peer),
                    None,
                    &params,
                    &mut host_addresses
                ),
                "{peer}"
            );
        }
    }

    #[test]
    fn admission_without_the_host_addresses_never_admits_global_peers() {
        let params = guest_params();
        let mut unknown = HostAddresses {
            addresses: None,
            listed: Some(Instant::now()),
        };
        let lan = InboundAdmission::PrivateNetwork { host: false };
        let lan_and_host = InboundAdmission::PrivateNetwork { host: true };
        // Any private-network peer might be the host.
        assert!(!lan.admits(address("192.168.1.50"), None, &params, &mut unknown));
        assert!(lan_and_host.admits(address("192.168.1.50"), None, &params, &mut unknown));
        // Globally reachable peers are refused even when the host is admitted.
        for peer in ["8.8.8.8", "2001:4860::8888"] {
            assert!(!lan.admits(address(peer), None, &params, &mut unknown));
            assert!(!lan_and_host.admits(address(peer), None, &params, &mut unknown));
        }
        // Loopback and the address that a peer connected to remain the host.
        assert!(lan_and_host.admits(address("127.0.0.1"), None, &params, &mut unknown));
        assert!(!lan.admits(address("127.0.0.1"), None, &params, &mut unknown));
    }

    #[test]
    fn host_admission_admits_the_host_alone() {
        let params = guest_params();
        let mut host_addresses = HostAddresses {
            addresses: Some(vec![address("192.168.1.10"), address("fd12::10")]),
            listed: Some(Instant::now()),
        };
        assert!(InboundAdmission::Host.publishes_guest_port());
        for (peer, local, from_host) in [
            ("127.0.0.1", "127.0.0.1", true),
            ("127.0.0.2", "127.0.0.1", true),
            ("::ffff:127.0.0.1", "::ffff:127.0.0.1", true),
            ("::1", "::1", true),
            ("192.168.1.10", "127.0.0.1", true),
            ("fd12::10", "::1", true),
            ("192.168.1.50", "127.0.0.1", false),
            ("fd12::50", "::1", false),
            ("8.8.8.8", "127.0.0.1", false),
            ("2001:4860::8888", "::1", false),
            ("10.0.0.77", "127.0.0.1", false),
            ("fe80::10", "::1", false),
        ] {
            assert_eq!(
                InboundAdmission::Host.admits(
                    address(peer),
                    Some(address(local)),
                    &params,
                    &mut host_addresses
                ),
                from_host,
                "{peer}"
            );
        }

        // Without the host's addresses, only loopback remains the host.
        let mut unknown = HostAddresses {
            addresses: None,
            listed: Some(Instant::now()),
        };
        assert!(InboundAdmission::Host.admits(address("::1"), None, &params, &mut unknown));
        for peer in ["192.168.1.50", "fd12::50", "8.8.8.8"] {
            assert!(
                !InboundAdmission::Host.admits(address(peer), None, &params, &mut unknown),
                "{peer}"
            );
        }
    }

    #[test]
    fn the_host_addresses_include_loopback_interfaces() {
        let mut host_addresses = HostAddresses::new();
        assert_eq!(host_addresses.contains(address("127.0.0.1")), Some(true));
        assert_eq!(host_addresses.contains(address("192.0.2.254")), Some(false));
    }
}
