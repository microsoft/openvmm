// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Consomme user-mode TCP stack.
//!
//! This crate implements a user-mode TCP stack designed for use with
//! virtualization. The guest operating system sends Ethernet frames, and this
//! crate parses them and distributes the data streams to individual TCP and UDP
//! sockets.
//!
//! The current implementation supports OS-backed TCP and UDP sockets,
//! essentially causing this stack to act as a NAT implementation, providing
//! guest OS networking by leveraging the host's network stack.
//!
//! This implementation includes a small DHCP server for address assignment.

mod arp;
mod dhcp;
mod dhcpv6;
#[cfg_attr(unix, path = "dns_unix.rs")]
#[cfg_attr(windows, path = "dns_windows.rs")]
mod dns;
mod dns_resolver;
mod icmp;
mod local_addr_map;
mod ndp;
mod tcp;
mod udp;

mod unix;
mod windows;

/// Standard DNS port number.
const DNS_PORT: u16 = 53;

/// Default per-connection TCP ring buffer bounds: start small and autotune up
/// to a few MiB so idle/short connections stay cheap while bulk flows ramp.
const DEFAULT_TCP_BUFFER_BOUNDS: TcpBufferBounds = TcpBufferBounds {
    initial: 16 * 1024,
    max: 4 * 1024 * 1024,
};

use inspect::Inspect;
use inspect::InspectMut;
use pal_async::driver::Driver;
use smoltcp::phy::Checksum;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::DhcpMessageType;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::EthernetRepr;
use smoltcp::wire::IPV4_HEADER_LEN;
use smoltcp::wire::Icmpv6Message;
use smoltcp::wire::Icmpv6Packet;
use smoltcp::wire::IpAddress;
use smoltcp::wire::IpProtocol;
pub use smoltcp::wire::IpVersion;
use smoltcp::wire::Ipv4Address;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv6Address;
use smoltcp::wire::Ipv6Packet;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::task::Context;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
struct FourTuple {
    src: SocketAddr,
    dst: SocketAddr,
}

impl core::fmt::Display for FourTuple {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}-{}", self.src, self.dst)
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
struct PortForwardKey {
    family: IpVersion,
    guest_port: u16,
}

impl PortForwardKey {
    fn new(family: IpVersion, guest_port: u16) -> Self {
        Self { family, guest_port }
    }

    fn from_socket_addr(addr: SocketAddr, guest_port: u16) -> Self {
        let family = match addr {
            SocketAddr::V4(_) => IpVersion::Ipv4,
            SocketAddr::V6(_) => IpVersion::Ipv6,
        };
        Self::new(family, guest_port)
    }
}

impl core::fmt::Display for PortForwardKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.family, self.guest_port)
    }
}

/// A consomme instance.
#[derive(InspectMut)]
pub struct Consomme {
    state: ConsommeState,
    #[inspect(mut)]
    tcp: tcp::Tcp,
    #[inspect(mut)]
    udp: udp::Udp,
    icmp: icmp::Icmp,
    dns: Option<dns_resolver::DnsResolver>,
    host_has_ipv6: bool,
}

#[derive(Inspect)]
struct ConsommeState {
    params: ConsommeParams,
    #[inspect(skip)]
    buffer: Box<[u8]>,
    local_addr_map: local_addr_map::LocalAddrMap,
}

/// Dynamic networking properties of a consomme endpoint.
#[derive(Inspect, Clone)]
pub struct ConsommeParams {
    /// Current IPv4 network mask.
    #[inspect(display)]
    pub net_mask: Ipv4Address,
    /// Current Ipv4 gateway address.
    #[inspect(display)]
    pub gateway_ip: Ipv4Address,
    /// Current Ipv4 gateway MAC address.
    #[inspect(display)]
    pub gateway_mac: EthernetAddress,
    /// Current Ipv4 address assigned to endpoint.
    #[inspect(display)]
    pub client_ip: Ipv4Address,
    /// Current client MAC address.
    #[inspect(display)]
    pub client_mac: EthernetAddress,
    /// Current list of DNS resolvers.
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsDisplay)")]
    pub nameservers: Vec<IpAddress>,
    /// Current IPv6 network mask (if any).
    pub prefix_len_ipv6: u8,
    /// If true, advertise an autonomous IPv6 prefix so guests create a
    /// routable IPv6 address with SLAAC.
    pub advertise_routable_ipv6: bool,
    /// Current IPv6 gateway MAC address (if any).
    #[inspect(display)]
    pub gateway_mac_ipv6: EthernetAddress,
    /// Gateway's link-local IPv6 address (derived from gateway_mac_ipv6).
    ///
    /// This is the address used as the source for NDP Router Advertisements
    /// and as the target for Neighbor Solicitations.
    #[inspect(display)]
    pub gateway_link_local_ipv6: Ipv6Address,
    /// Current IPv6 address learned from guest via SLAAC (if any).
    ///
    /// With SLAAC (Stateless Address Autoconfiguration), the guest generates
    /// its own IPv6 address using the advertised prefix and its interface identifier.
    /// This field is learned from incoming IPv6 traffic from the guest.
    #[inspect(with = "|x| x.map(inspect::AsDisplay)")]
    pub client_ip_ipv6: Option<Ipv6Address>,
    /// Current routable IPv6 address learned from guest via SLAAC (if any).
    /// The guest will typically have two addresses: a local and a routable one.
    /// This field is learned from incoming IPv6 traffic from the guest.
    #[inspect(with = "|x| x.map(inspect::AsDisplay)")]
    pub client_ip_ipv6_routable: Option<Ipv6Address>,
    /// Idle timeout for UDP connections.
    pub udp_timeout: Duration,
    /// If true, skip checks for host IPv6 support and assume the host has a
    /// routable IPv6 address.
    pub skip_ipv6_checks: bool,
    /// If true, allow guest traffic destined for host-local addresses
    /// (loopback, unspecified, link-local).
    pub allow_host_local_access: bool,
    /// Per-connection TCP receive ring buffer bounds (guest-to-host).
    pub tcp_rx_buffer: TcpBufferBounds,
    /// Per-connection TCP transmit ring buffer bounds (host-to-guest).
    pub tcp_tx_buffer: TcpBufferBounds,
}

/// Bounds for a per-connection TCP ring buffer.
///
/// The buffer is allocated at `initial` and may grow up to `max` as demand
/// warrants. Both values are clamped to `[16 KiB, 4 MiB]` and rounded up to a
/// power of two, then `initial` is clamped to be no greater than `max`. The
/// advertised TCP window scale is derived from `max`, so `max` is fixed for
/// the connection's lifetime.
///
/// Note that if the peer does not negotiate window scaling, the effective
/// receive window (and thus the useful rx capacity) is capped at `u16::MAX`
/// regardless of `max`.
#[derive(Inspect, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpBufferBounds {
    /// Starting capacity, allocated up front.
    pub initial: usize,
    /// Ceiling for autotuned growth.
    pub max: usize,
}

impl TcpBufferBounds {
    /// Use the same value for `initial` and `max`. Disables autotune growth.
    pub const fn fixed(n: usize) -> Self {
        Self { initial: n, max: n }
    }
}

/// An error indicating that the CIDR is invalid.
#[derive(Debug, Error)]
#[error("invalid CIDR")]
pub struct InvalidCidr;

impl ConsommeParams {
    /// Create default dynamic network state. The default state is
    ///     IP address: 10.0.0.2 / 24
    ///     gateway: 10.0.0.1 with MAC address 52-55-10-0-0-1
    ///     IPv6 address: is not assigned by us, we expect the guest to assign it via SLAAC
    ///     gateway IPv6 link-local address: fe80::5055:aff:fe00:102 (EUI-64 derived from
    ///     gateway MAC address 52-55-0A-00-01-02)
    pub fn new() -> Result<Self, Error> {
        let nameservers = dns::nameservers()?;
        let gateway_mac_ipv6 = EthernetAddress([0x52, 0x55, 0x0A, 0x00, 0x01, 0x02]);

        Ok(Self {
            gateway_ip: Ipv4Address::new(10, 0, 0, 1),
            gateway_mac: EthernetAddress([0x52, 0x55, 10, 0, 0, 1]),
            client_ip: Ipv4Address::new(10, 0, 0, 2),
            client_mac: EthernetAddress([0x0, 0x0, 0x0, 0x0, 0x1, 0x0]),
            net_mask: Ipv4Address::new(255, 255, 255, 0),
            nameservers,
            prefix_len_ipv6: 64,
            advertise_routable_ipv6: true,
            gateway_mac_ipv6,
            gateway_link_local_ipv6: Self::compute_link_local_address(gateway_mac_ipv6),
            client_ip_ipv6: None,
            client_ip_ipv6_routable: None,
            // Per RFC 4787, UDP NAT bindings, by default, should timeout after 5 minutes, but can be configured.
            udp_timeout: Duration::from_secs(300),
            skip_ipv6_checks: false,
            allow_host_local_access: false,
            tcp_rx_buffer: DEFAULT_TCP_BUFFER_BOUNDS,
            tcp_tx_buffer: DEFAULT_TCP_BUFFER_BOUNDS,
        })
    }

    /// Sets the cidr for the network.
    ///
    /// Setting, for example, 192.168.0.0/24 will set the gateway to
    /// 192.168.0.1 and the client IP to 192.168.0.2.
    pub fn set_cidr(&mut self, cidr: &str) -> Result<(), InvalidCidr> {
        let cidr: smoltcp::wire::Ipv4Cidr = cidr.parse().map_err(|()| InvalidCidr)?;
        let base_address = cidr.network().address();
        let mut gateway_octets = base_address.octets();
        gateway_octets[3] += 1;
        self.gateway_ip = Ipv4Address::from(gateway_octets);
        let mut client_octets = base_address.octets();
        client_octets[3] += 2;
        self.client_ip = Ipv4Address::from(client_octets);
        self.net_mask = cidr.netmask();
        Ok(())
    }

    /// Compute a link-local IPv6 address from a MAC address using EUI-64 format.
    ///
    /// RFC 4291 Section 2.5.6: Link-local addresses are formed by combining
    /// the link-local prefix (fe80::/64) with an interface identifier derived
    /// from the MAC address using the EUI-64 format.
    ///
    /// EUI-64 format (RFC 2464 Section 4):
    /// - Insert 0xFFFE in the middle of the 48-bit MAC address
    /// - Invert the universal/local bit (bit 6 of the first byte)
    pub fn compute_link_local_address(mac: EthernetAddress) -> Ipv6Address {
        const LINK_LOCAL_PREFIX: [u8; 8] = [0xfe, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let mut addr = [0u8; 16];

        // Set link-local prefix (fe80::/64)
        addr[0..8].copy_from_slice(&LINK_LOCAL_PREFIX);

        // Create EUI-64 interface identifier from MAC address
        // MAC: AB:CD:EF:11:22:33
        // EUI-64: AB:CD:EF:FF:FE:11:22:33 with universal/local bit flipped
        addr[8] = mac.0[0] ^ 0x02; // Flip the universal/local bit
        addr[9] = mac.0[1];
        addr[10] = mac.0[2];
        addr[11] = 0xFF;
        addr[12] = 0xFE;
        addr[13] = mac.0[3];
        addr[14] = mac.0[4];
        addr[15] = mac.0[5];

        Ipv6Address::from_octets(addr)
    }

    /// Infer the guest's link-local IPv6 address from a learned routable SLAAC address.
    ///
    /// If the routable address uses an EUI-64 address derived from the guest MAC, we can infer a
    /// matching link-local address.
    ///
    /// If the routable address uses a different interface identifier (for example,
    /// privacy or stable-secret addressing), this leaves the link-local address
    /// unknown and waits to learn it from traffic or NDP instead.
    pub(crate) fn infer_client_link_local_from_routable(
        &mut self,
        routable: Ipv6Address,
        source: &'static str,
    ) {
        // Link local address is already known.
        if self.client_ip_ipv6.is_some() {
            return;
        }

        let link_local = Self::compute_link_local_address(self.client_mac);
        if routable.octets()[8..] != link_local.octets()[8..] {
            return;
        }

        tracing::debug!(
            client_ipv6 = %link_local,
            client_ipv6_routable = %routable,
            source,
            "inferred client link-local IPv6 address from routable SLAAC address"
        );
        self.client_ip_ipv6 = Some(link_local);
    }

    /// Returns the list of IPv6 nameservers suitable for advertisement to
    /// guests via NDP RDNSS or DHCPv6.
    ///
    /// Filters out addresses that are not useful as DNS servers in a
    /// guest-facing context: unspecified, loopback, multicast, unique-local
    /// (fc00::/7), and deprecated site-local (fec0::/10) addresses.
    pub fn filtered_ipv6_nameservers(&self) -> Vec<Ipv6Address> {
        self.nameservers
            .iter()
            .filter_map(|ip| match ip {
                IpAddress::Ipv6(addr) => Some(*addr),
                _ => None,
            })
            .filter(|addr| {
                let octets = addr.octets();
                !(addr.is_unspecified()
                    || addr.is_loopback()
                    || addr.is_multicast()
                    || matches!(octets[0], 0xfc | 0xfd) // unique local address
                    || octets.starts_with(&[0xfe, 0xc0])) // deprecated site-local
            })
            .collect()
    }

    /// Returns the default internal nameserver list for use when the DNS
    /// resolver is active. Includes the IPv6 gateway only when the host
    /// has a routable IPv6 address.
    fn internal_nameservers(&self, host_has_ipv6: bool) -> Vec<IpAddress> {
        let mut ns = vec![self.gateway_ip.into()];
        if host_has_ipv6 {
            ns.push(self.gateway_link_local_ipv6.into());
        }
        ns
    }

    fn is_local_address(&self, addr: &SocketAddr) -> bool {
        match addr {
            SocketAddr::V4(v4) => v4.ip().is_loopback() || v4.ip() == &self.client_ip,
            SocketAddr::V6(v6) => {
                v6.ip().is_loopback()
                    || self.client_ip_ipv6.is_some_and(|ip| v6.ip() == &ip)
                    || self
                        .client_ip_ipv6_routable
                        .is_some_and(|ip| v6.ip() == &ip)
            }
        }
    }
}

impl ConsommeState {
    fn try_ft_from_remote_address(
        &mut self,
        remote_addr: &SocketAddr,
        dst_port: u16,
    ) -> Option<FourTuple> {
        Self::translate_remote_address(
            &self.params,
            &mut self.local_addr_map,
            remote_addr,
            dst_port,
        )
    }

    fn translate_remote_address(
        params: &ConsommeParams,
        local_addr_map: &mut local_addr_map::LocalAddrMap,
        remote_addr: &SocketAddr,
        dst_port: u16,
    ) -> Option<FourTuple> {
        // Pick the best destination (guest) address based on the origination of the remote packet.
        let dst = match remote_addr {
            SocketAddr::V4(_) => SocketAddr::V4(SocketAddrV4::new(params.client_ip, dst_port)),
            SocketAddr::V6(v6) => {
                let client_ipv6 = if !v6.ip().is_unicast_link_local()
                    && let Some(routable) = params.client_ip_ipv6_routable
                {
                    routable
                } else if let Some(ipv6) = params.client_ip_ipv6 {
                    ipv6
                } else if let Some(routable) = params.client_ip_ipv6_routable {
                    routable
                } else {
                    tracelimit::warn_ratelimited!(addr = %remote_addr, "Client IPv6 address is not known, dropping packet");
                    return None;
                };

                SocketAddr::V6(SocketAddrV6::new(client_ipv6, dst_port, 0, 0))
            }
        };

        // If the remote IP is loopback or matches the client IP address, replace
        // it with a unique virtual address so that the guest routes the reply
        // back through the virtual adapter and we can reverse-translate it on the
        // outgoing path.
        //
        // This is skipped for endpoints that are themselves dedicated to
        // loopback traffic (their `client_ip` is a loopback address, as used by
        // WSL's VirtioProxy localhost forwarding). The guest routes those
        // replies back through this adapter based on the loopback source, so
        // rewriting the source out of the loopback range would break the return
        // path.
        let is_loopback_adapter = params.client_ip.is_loopback();
        let src = match remote_addr {
            SocketAddr::V4(v4) if params.is_local_address(remote_addr) && !is_loopback_adapter => {
                let subnet_base =
                    Ipv4Addr::from(u32::from(params.gateway_ip) & u32::from(params.net_mask));
                let virtual_ip = local_addr_map.get_or_allocate_v4(
                    *v4.ip(),
                    subnet_base,
                    params.net_mask,
                    params.gateway_ip,
                    params.client_ip,
                );
                match virtual_ip {
                    Some(ip) => SocketAddr::V4(SocketAddrV4::new(ip, v4.port())),
                    None => {
                        // Pool exhausted, fall back to gateway IP.
                        SocketAddr::V4(SocketAddrV4::new(params.gateway_ip, v4.port()))
                    }
                }
            }
            SocketAddr::V6(v6) if params.is_local_address(remote_addr) && !is_loopback_adapter => {
                let virtual_ip = local_addr_map.get_or_allocate_v6(
                    *v6.ip(),
                    params.gateway_link_local_ipv6,
                    params.client_ip_ipv6,
                    params.client_ip_ipv6_routable,
                );
                match virtual_ip {
                    Some(ip) => SocketAddr::V6(SocketAddrV6::new(ip, v6.port(), 0, 0)),
                    None => {
                        // Pool exhausted, fall back to gateway link-local.
                        SocketAddr::V6(SocketAddrV6::new(
                            params.gateway_link_local_ipv6,
                            v6.port(),
                            0,
                            0,
                        ))
                    }
                }
            }
            SocketAddr::V6(v6) => {
                // Remove flow info and scope id from the source address.
                SocketAddr::V6(SocketAddrV6::new(*v6.ip(), v6.port(), 0, 0))
            }
            _ => *remote_addr,
        };

        Some(FourTuple { src, dst })
    }

    /// Resolve a destination address that the guest is sending to. If it is a
    /// virtual mapped address, return the real host address. Otherwise return
    /// the address unchanged.
    fn resolve_destination(&self, addr: &SocketAddr) -> SocketAddr {
        let ip = addr.ip();
        if let Some(real_ip) = self.local_addr_map.resolve_virtual(&ip) {
            SocketAddr::new(real_ip, addr.port())
        } else {
            *addr
        }
    }
}

/// An accessor for consomme.
pub struct Access<'a, T> {
    inner: &'a mut Consomme,
    client: &'a mut T,
}

/// A consomme client.
pub trait Client {
    /// Gets the driver to use for handling new connections.
    ///
    /// TODO: generalize connection creation to allow pluggable model (not just
    /// OS sockets) and remove this.
    fn driver(&self) -> &dyn Driver;

    /// Transmits a packet to the client.
    ///
    /// If `checksum.ipv4`, `checksum.tcp`, or `checksum.udp` are set, then the
    /// packet contains an IPv4 header, TCP header, and/or UDP header with a
    /// valid checksum.
    ///
    /// TODO: support >MTU sized packets (RSC/LRO/GRO).
    fn recv(&mut self, data: &[u8], checksum: &ChecksumState);

    /// Transmits a packet whose bytes are provided as multiple discontiguous
    /// segments, delivered in order as a single logical packet.
    ///
    /// This lets callers avoid linearizing a frame into a scratch buffer when
    /// its payload is not contiguous (for example, a header segment followed by
    /// TCP window bytes that wrap a ring buffer). The `checksum` argument has
    /// the same meaning as for [`Client::recv`].
    ///
    /// The default implementation copies the segments into a contiguous buffer
    /// and forwards to [`Client::recv`]. Clients that can write discontiguous
    /// data directly to the guest should override this to eliminate the copy.
    fn recv_segments(&mut self, segments: &[&[u8]], checksum: &ChecksumState) {
        if let [segment] = segments {
            self.recv(segment, checksum);
            return;
        }
        let total = segments.iter().map(|s| s.len()).sum();
        let mut data = Vec::with_capacity(total);
        for segment in segments {
            data.extend_from_slice(segment);
        }
        self.recv(&data, checksum);
    }

    /// Specifies the maximum size for the next call to `recv`.
    ///
    /// This is the MTU including the Ethernet frame header. This must be at
    /// least [`MIN_MTU`].
    ///
    /// Return 0 to indicate that there are no buffers available for receiving
    /// data.
    fn rx_mtu(&mut self) -> usize;
}

/// Specifies the checksum state for a packet being transmitted.
#[derive(Debug, Copy, Clone)]
pub struct ChecksumState {
    /// On receive, the data has a valid IPv4 header checksum. On send, the
    /// checksum should be ignored.
    pub ipv4: bool,
    /// On receive, the data has a valid TCP checksum. On send, the checksum
    /// should be ignored.
    pub tcp: bool,
    /// On receive, the data has a valid UDP checksum. On send, the checksum
    /// should be ignored.
    pub udp: bool,
    /// The data consists of multiple TCP segments, each with the provided
    /// segment size.
    ///
    /// The IP header's length field may be invalid and should be ignored.
    pub tso: Option<u16>,
    /// The data is a large UDP payload that should be sent with OS-level UDP
    /// GSO, splitting into UDP datagrams of this segment size.
    pub gso: Option<u16>,
}

impl ChecksumState {
    const NONE: Self = Self {
        ipv4: false,
        tcp: false,
        udp: false,
        tso: None,
        gso: None,
    };
    const IPV4_ONLY: Self = Self {
        ipv4: true,
        tcp: false,
        udp: false,
        tso: None,
        gso: None,
    };
    const TCP4: Self = Self {
        ipv4: true,
        tcp: true,
        udp: false,
        tso: None,
        gso: None,
    };
    const UDP4: Self = Self {
        ipv4: true,
        tcp: false,
        udp: true,
        tso: None,
        gso: None,
    };
    const TCP6: Self = Self {
        ipv4: false,
        tcp: true,
        udp: false,
        tso: None,
        gso: None,
    };

    fn caps(&self) -> ChecksumCapabilities {
        let mut caps = ChecksumCapabilities::default();
        if self.ipv4 {
            caps.ipv4 = Checksum::None;
        }
        if self.tcp {
            caps.tcp = Checksum::None;
        }
        if self.udp {
            caps.udp = Checksum::None;
        }
        caps
    }
}

/// The minimum MTU for receives supported by Consomme (including the Ethernet
/// frame).
pub const MIN_MTU: usize = 1514;

/// The reason a packet was dropped without being handled.
#[derive(Debug, Error)]
pub enum DropReason {
    /// The packet could not be parsed.
    #[error("packet parsing error")]
    Packet(#[from] smoltcp::wire::Error),
    /// The ethertype is unknown.
    #[error("unsupported ethertype {0}")]
    UnsupportedEthertype(EthernetProtocol),
    /// The ethertype is unknown.
    #[error("unsupported ip protocol {0}")]
    UnsupportedIpProtocol(IpProtocol),
    /// The ICMPv6 message type is unsupported.
    #[error("unsupported icmpv6 message type {0}")]
    UnsupportedIcmpv6(Icmpv6Message),
    /// The ARP type is unsupported.
    #[error("unsupported dhcp message type {0:?}")]
    UnsupportedDhcp(DhcpMessageType),
    /// The ARP type is unsupported.
    #[error("unsupported arp type")]
    UnsupportedArp,
    /// The IPv4 checksum was invalid.
    #[error("ipv4 checksum failure")]
    Ipv4Checksum,
    /// The send buffer is invalid.
    #[error("send buffer full")]
    SendBufferFull,
    /// There was an IO error.
    #[error("io error")]
    Io(#[source] std::io::Error),
    /// The TCP state is invalid.
    #[error("bad tcp state")]
    BadTcpState(#[from] tcp::TcpError),
    /// The DHCPv6 message type is unsupported.
    #[error("unsupported dhcpv6 message type {0:?}")]
    UnsupportedDhcpv6(dhcpv6::MessageType),
    /// The NDP message type is unsupported.
    #[error("unsupported ndp message type {0:?}")]
    UnsupportedNdp(ndp::NdpMessageType),
    /// An incoming packet was recognized but was self-contradictory.
    /// E.g. a TCP packet with both SYN and FIN flags set.
    #[error("packet is malformed")]
    MalformedPacket,
    /// The IP total-length field does not match the buffer size.
    #[error("ip length mismatch")]
    IpLengthMismatch,
    /// An incoming IP packet has been split into several IP fragments and was dropped,
    /// since IP reassembly is not supported.
    #[error("packet fragmentation is not supported")]
    FragmentedPacket,
    /// The destination address is not allowed (e.g., loopback, unspecified,
    /// or link-local when host-local access is disabled).
    #[error("destination address not allowed")]
    DestinationNotAllowed,
}

/// An error from a port bind or unbind operation.
#[derive(Debug, Error)]
pub enum BindError {
    /// The specified port is already bound.
    #[error("port {0} is already bound")]
    PortAlreadyBound(u16),
    /// The specified port is not bound.
    #[error("port is not bound")]
    PortNotBound,
    /// An IO error occurred during binding.
    #[error(transparent)]
    Io(std::io::Error),
}

/// An error to create a consomme instance.
#[derive(Debug, Error)]
pub enum Error {
    /// Could not get DNS nameserver information.
    #[error("failed to initialize nameservers")]
    Dns(#[from] dns::Error),
}

#[derive(Debug)]
struct Ipv4Addresses {
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
}

#[derive(Debug)]
struct Ipv6Addresses {
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
}

#[derive(Debug)]
enum IpAddresses {
    V4(Ipv4Addresses),
    V6(Ipv6Addresses),
}

impl IpAddresses {
    fn src_addr(&self) -> IpAddress {
        match self {
            IpAddresses::V4(addrs) => IpAddress::Ipv4(addrs.src_addr),
            IpAddresses::V6(addrs) => IpAddress::Ipv6(addrs.src_addr),
        }
    }

    fn dst_addr(&self) -> IpAddress {
        match self {
            IpAddresses::V4(addrs) => IpAddress::Ipv4(addrs.dst_addr),
            IpAddresses::V6(addrs) => IpAddress::Ipv6(addrs.dst_addr),
        }
    }
}

/// Returns `true` if the given IPv4 destination is host-local
/// (loopback, unspecified, or link-local) and should be blocked
/// when `allow_host_local_access` is disabled.
fn is_blocked_host_local_ipv4(addr: Ipv4Address) -> bool {
    addr.is_loopback() || addr.is_unspecified() || addr.is_link_local()
}

/// Returns `true` if the given IPv6 destination is host-local
/// (loopback, unspecified, or link-local) and should be blocked
/// when `allow_host_local_access` is disabled.
fn is_blocked_host_local_ipv6(addr: Ipv6Address) -> bool {
    addr.is_loopback() || addr.is_unspecified() || addr.is_unicast_link_local()
}

/// Returns `true` if two IPv4 addresses are in the same subnet given a mask.
pub(crate) fn is_same_ipv4_subnet(
    addr1: Ipv4Address,
    addr2: Ipv4Address,
    subnet_mask: Ipv4Address,
) -> bool {
    let subnet_mask = subnet_mask.to_bits();
    (addr1.to_bits() & subnet_mask) == (addr2.to_bits() & subnet_mask)
}

/// Returns `true` if two IPv6 addresses share the same prefix of the given length.
pub(crate) fn is_same_ipv6_subnet(addr1: Ipv6Address, addr2: Ipv6Address, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }
    if prefix_len >= 128 {
        return addr1 == addr2;
    }
    let mask = u128::MAX << (128 - prefix_len);
    (addr1.to_bits() & mask) == (addr2.to_bits() & mask)
}

/// Returns `true` if the given IPv6 address is a globally routable unicast
/// address (i.e., not loopback, unspecified, or link-local).
fn is_routable_ipv6(addr: &std::net::Ipv6Addr) -> bool {
    !addr.is_loopback() && !addr.is_unspecified() && !addr.is_unicast_link_local()
}

impl Consomme {
    /// Creates a new consomme instance with specified state.
    pub fn new(mut params: ConsommeParams) -> Self {
        let host_has_ipv6 = if params.skip_ipv6_checks {
            true
        } else {
            #[cfg(windows)]
            let host_has_ipv6_result = windows::host_has_ipv6_address().map_err(|e| e.to_string());
            #[cfg(unix)]
            let host_has_ipv6_result = unix::host_has_ipv6_address().map_err(|e| e.to_string());

            match host_has_ipv6_result {
                Ok(has_ipv6) => has_ipv6,
                Err(e) => {
                    tracelimit::warn_ratelimited!(
                        "failed to check for host IPv6 address, assuming no IPv6 support: {e}"
                    );
                    false
                }
            }
        };
        let dns =
            match dns_resolver::DnsResolver::new(dns_resolver::DEFAULT_MAX_PENDING_DNS_REQUESTS) {
                Ok(dns) => {
                    // When the DNS resolver is available, use the default internal nameserver.
                    params.nameservers = params.internal_nameservers(host_has_ipv6);
                    Some(dns)
                }
                Err(_) => {
                    tracelimit::warn_ratelimited!(
                        "failed to initialize DNS resolver, falling back to using host DNS settings"
                    );
                    None
                }
            };
        let timeout = params.udp_timeout;
        let tcp_rx_buffer = params.tcp_rx_buffer;
        let tcp_tx_buffer = params.tcp_tx_buffer;
        Self {
            state: ConsommeState {
                params,
                buffer: Box::new([0; 65536]),
                local_addr_map: local_addr_map::LocalAddrMap::new(),
            },
            tcp: tcp::Tcp::new(tcp_rx_buffer, tcp_tx_buffer),
            udp: udp::Udp::new(timeout),
            icmp: icmp::Icmp::new(),
            dns,
            host_has_ipv6,
        }
    }

    /// Get access to the parameters to be updated.
    ///
    /// FUTURE: add support for updating only the parameters that can be safely
    /// changed at runtime.
    pub fn params_mut(&mut self) -> &mut ConsommeParams {
        &mut self.state.params
    }

    /// Clears the local address mapping table. Call this after changing the
    /// network configuration (e.g., via [`ConsommeParams::set_cidr`]) to avoid
    /// stale or conflicting virtual address mappings.
    ///
    /// Some in-flight packets may be lost during the transition; this is
    /// acceptable.
    pub fn clear_local_addr_map(&mut self) {
        self.state.local_addr_map.clear();
    }

    /// Pairs the client with this instance to operate on the consomme instance.
    pub fn access<'a, T: Client>(&'a mut self, client: &'a mut T) -> Access<'a, T> {
        Access {
            inner: self,
            client,
        }
    }
}

impl<T: Client> Access<'_, T> {
    /// Gets the inner consomme object.
    pub fn get(&self) -> &Consomme {
        self.inner
    }

    /// Gets the inner consomme object.
    pub fn get_mut(&mut self) -> &mut Consomme {
        self.inner
    }

    /// Polls for work, transmitting any ready packets to the client.
    pub fn poll(&mut self, cx: &mut Context<'_>) {
        self.poll_udp(cx);
        self.poll_tcp(cx);
        self.poll_icmp(cx);
    }

    /// Update all sockets to use the new client's IO driver. This must be
    /// called if the previous driver is no longer usable or if the client
    /// otherwise wants existing connections to be polled on a new IO driver.
    pub fn refresh_driver(&mut self) {
        self.refresh_tcp_driver();
        self.refresh_udp_driver();
    }

    /// Sends an Ethernet frame to the network.
    ///
    /// If `checksum.ipv4`, `checksum.tcp`, or `checksum.udp` are set, then
    /// skips validating the IPv4, TCP, and UDP checksums. Otherwise, these
    /// checksums are validated as normal and packets with invalid checksums are
    /// dropped.
    ///
    /// If `checksum.tso.is_some()`, then perform TCP segmentation offset on the
    /// frame. Practically speaking, this means that the frame contains a TCP
    /// packet with these caveats:
    ///
    ///   * The IP header length may be invalid and will be ignored. The TCP
    ///     packet payload is assumed to end at the end of `data`.
    ///   * The TCP segment's payload size may be larger than the advertized TCP
    ///     MSS value.
    ///
    /// This allows for sending TCP data that is much larger than the MSS size
    /// via a single call.
    ///
    /// TODO:
    ///
    ///   1. allow for discontiguous packets
    ///   2. allow for packets in guest memory (including lifetime model, if
    ///      necessary--currently TCP transmits only happen in `poll`, but this
    ///      may not be necessary. If the underlying socket implementation
    ///      performs a copy (as the standard kernel socket APIs do), then no
    ///      lifetime model is necessary, but if an implementation wants
    ///      zerocopy support then some mechanism to allow the guest memory to
    ///      be released later will be necessary.
    pub fn send(&mut self, data: &[u8], checksum: &ChecksumState) -> Result<(), DropReason> {
        let frame_packet = EthernetFrame::new_unchecked(data);
        let frame = EthernetRepr::parse(&frame_packet)?;
        match frame.ethertype {
            EthernetProtocol::Ipv4 => self.handle_ipv4(&frame, frame_packet.payload(), checksum)?,
            EthernetProtocol::Ipv6 => {
                if self.inner.host_has_ipv6 {
                    self.handle_ipv6(&frame, frame_packet.payload(), checksum)?
                }
            }
            EthernetProtocol::Arp => self.handle_arp(&frame, frame_packet.payload())?,
            _ => return Err(DropReason::UnsupportedEthertype(frame.ethertype)),
        }
        Ok(())
    }

    fn handle_ipv4(
        &mut self,
        frame: &EthernetRepr,
        payload: &[u8],
        checksum: &ChecksumState,
    ) -> Result<(), DropReason> {
        let ipv4 = Ipv4Packet::new_unchecked(payload);
        if payload.len() < IPV4_HEADER_LEN
            || ipv4.version() != 4
            || payload.len() < ipv4.header_len().into()
        {
            return Err(DropReason::MalformedPacket);
        }

        // For segmentation offload (TSO/USO) the IP total_length field may
        // not reflect the actual buffer size (it can hold only one segment's
        // worth or wrap for payloads > 64 KiB). Use the real buffer length
        // instead.
        let segmentation_offload = checksum.tso.is_some() || checksum.gso.is_some();
        if !segmentation_offload && payload.len() < ipv4.total_len().into() {
            return Err(DropReason::IpLengthMismatch);
        }

        let total_len = if segmentation_offload {
            payload.len()
        } else {
            ipv4.total_len().into()
        };
        if total_len < ipv4.header_len().into() {
            return Err(DropReason::MalformedPacket);
        }

        if ipv4.more_frags() || ipv4.frag_offset() != 0 {
            return Err(DropReason::FragmentedPacket);
        }

        if !checksum.ipv4 && !ipv4.verify_checksum() {
            return Err(DropReason::Ipv4Checksum);
        }

        // Reject guest traffic to host-local-only destinations.
        if !self.inner.state.params.allow_host_local_access
            && is_blocked_host_local_ipv4(ipv4.dst_addr())
        {
            return Err(DropReason::DestinationNotAllowed);
        }

        let addresses = Ipv4Addresses {
            src_addr: ipv4.src_addr(),
            dst_addr: ipv4.dst_addr(),
        };

        let inner = &payload[ipv4.header_len().into()..total_len];

        match ipv4.next_header() {
            IpProtocol::Tcp => self.handle_tcp(&IpAddresses::V4(addresses), inner, checksum)?,
            IpProtocol::Udp => {
                self.handle_udp(frame, &IpAddresses::V4(addresses), inner, checksum)?
            }
            IpProtocol::Icmp => {
                self.handle_icmp(frame, &addresses, inner, checksum, ipv4.hop_limit())?
            }
            p => return Err(DropReason::UnsupportedIpProtocol(p)),
        };
        Ok(())
    }

    fn handle_ipv6(
        &mut self,
        frame: &EthernetRepr,
        payload: &[u8],
        checksum: &ChecksumState,
    ) -> Result<(), DropReason> {
        let ipv6 = Ipv6Packet::new_unchecked(payload);
        if payload.len() < smoltcp::wire::IPV6_HEADER_LEN || ipv6.version() != 6 {
            return Err(DropReason::MalformedPacket);
        }

        // For segmentation offload (TSO/USO) the IPv6 payload_length field
        // may not reflect the actual buffer size. Skip the length validation
        // and use the full buffer.
        let segmentation_offload = checksum.tso.is_some() || checksum.gso.is_some();
        if !segmentation_offload {
            let required_len = smoltcp::wire::IPV6_HEADER_LEN + ipv6.payload_len() as usize;
            if payload.len() < required_len {
                return Err(DropReason::MalformedPacket);
            }
        }

        // Reject guest traffic to host-local-only destinations.
        if !self.inner.state.params.allow_host_local_access
            && is_blocked_host_local_ipv6(ipv6.dst_addr())
        {
            return Err(DropReason::DestinationNotAllowed);
        }

        let next_header = ipv6.next_header();
        let src_addr = ipv6.src_addr();
        let inner = &payload[smoltcp::wire::IPV6_HEADER_LEN..];
        let addresses = Ipv6Addresses {
            src_addr,
            dst_addr: ipv6.dst_addr(),
        };

        // Learn the client's link-local IPv6 address from outgoing traffic.
        // This covers clients that do not perform DAD before using the address.
        if src_addr.is_unicast_link_local()
            && self.inner.state.params.client_ip_ipv6 != Some(src_addr)
        {
            tracing::debug!(
                client_ipv6 = %src_addr,
                "learned client link-local IPv6 address from outgoing traffic"
            );
            self.inner.state.params.client_ip_ipv6 = Some(src_addr);
        }

        // Learn the client's routable IPv6 address from outgoing traffic.
        // This is more reliable than relying solely on DAD Neighbor
        // Solicitations, which some clients skip on private virtual links.
        if !src_addr.is_unspecified()
            && !src_addr.is_multicast()
            && !src_addr.is_unicast_link_local()
            && self.inner.state.params.client_ip_ipv6_routable != Some(src_addr)
        {
            tracing::debug!(
                client_ipv6_routable = %src_addr,
                "learned client routable IPv6 address from outgoing traffic"
            );
            self.inner.state.params.client_ip_ipv6_routable = Some(src_addr);
            self.inner
                .state
                .params
                .infer_client_link_local_from_routable(src_addr, "outgoing traffic");
        }

        match next_header {
            IpProtocol::Udp => {
                self.handle_udp(frame, &IpAddresses::V6(addresses), inner, checksum)?
            }
            IpProtocol::Tcp => self.handle_tcp(&IpAddresses::V6(addresses), inner, checksum)?,
            IpProtocol::Icmpv6 => {
                // Check if this is an NDP packet
                let icmpv6_packet = Icmpv6Packet::new_unchecked(inner);
                let msg_type = icmpv6_packet.msg_type();

                if msg_type.is_ndisc() {
                    self.handle_ndp(frame, inner, ipv6.src_addr())?;
                } else {
                    tracing::trace!(
                        icmpv6_type = %msg_type,
                        src_addr = %src_addr,
                        dst_addr = %addresses.dst_addr,
                        "unsupported ICMPv6 message"
                    );
                    return Err(DropReason::UnsupportedIcmpv6(msg_type));
                }
            }

            p => return Err(DropReason::UnsupportedIpProtocol(p)),
        };
        Ok(())
    }

    /// Updates the DNS nameservers based on the current consomme parameters.
    pub fn update_dns_nameservers(&mut self) {
        if self.inner.dns.is_some() {
            self.inner.state.params.nameservers = self
                .inner
                .state
                .params
                .internal_nameservers(self.inner.host_has_ipv6);
        }
    }
}

#[cfg(test)]
mod tests;
