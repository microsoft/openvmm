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
#[cfg_attr(unix, path = "dns_unix.rs")]
#[cfg_attr(windows, path = "dns_windows.rs")]
mod dns;

#[cfg_attr(unix, path = "dns/mod.rs")]
#[cfg_attr(windows, path = "dns/mod.rs")]
mod dns_resolver;

mod icmp;
mod tcp;
mod udp;
mod windows;

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
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use smoltcp::wire::Ipv4Packet;
use std::net::SocketAddrV4;
use std::task::Context;
use std::time::Duration;
use thiserror::Error;

/// A consomme instance.
#[derive(InspectMut)]
pub struct Consomme {
    state: ConsommeState,
    tcp: tcp::Tcp,
    #[inspect(mut)]
    udp: udp::Udp,
    icmp: icmp::Icmp,
    // TODO: implement inspect
    #[inspect(skip)]
    dns: Option<dns_resolver::DnsResolver>,
}

#[derive(Inspect)]
struct ConsommeState {
    params: ConsommeParams,
    #[inspect(skip)]
    buffer: Box<[u8]>,
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
    pub nameservers: Vec<Ipv4Address>,
    /// Idle timeout for UDP connections.
    #[inspect(debug)]
    pub udp_timeout: Duration,
}

/// An error indicating that the CIDR is invalid.
#[derive(Debug, Error)]
#[error("invalid CIDR")]
pub struct InvalidCidr;

impl ConsommeParams {
    /// Create default dynamic network state. The default state is
    ///     IP address: 10.0.0.2 / 24
    ///     gateway: 10.0.0.1 with MAC address 52-55-10-0-0-1
    ///     no DNS resolvers
    pub fn new() -> Result<Self, Error> {
        let nameservers = dns::nameservers()?;

        Ok(Self {
            gateway_ip: Ipv4Address::new(10, 0, 0, 1),
            gateway_mac: EthernetAddress([0x52, 0x55, 10, 0, 0, 1]),
            client_ip: Ipv4Address::new(10, 0, 0, 2),
            client_mac: EthernetAddress([0x0, 0x0, 0x0, 0x0, 0x1, 0x0]),
            net_mask: Ipv4Address::new(255, 255, 255, 0),
            nameservers,
            // Per RFC 4787, UDP NAT bindings, by default, should timeout after 5 minutes, but can be configured.
            udp_timeout: Duration::from_secs(300),
        })
    }

    /// Sets the cidr for the network.
    ///
    /// Setting, for example, 192.168.0.0/24 will set the gateway to
    /// 192.168.0.1 and the client IP to 192.168.0.2.
    pub fn set_cidr(&mut self, cidr: &str) -> Result<(), InvalidCidr> {
        let cidr: smoltcp::wire::Ipv4Cidr = cidr.parse().map_err(|()| InvalidCidr)?;
        let base_address = cidr.network().address();

        let mut gateway_ip = base_address.octets();
        gateway_ip[3] += 1;
        self.gateway_ip = Ipv4Address::from_octets(gateway_ip);

        let mut client_ip = base_address.octets();
        client_ip[3] += 2;
        self.client_ip = Ipv4Address::from_octets(client_ip);

        self.net_mask = cidr.netmask();
        Ok(())
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
    /// TODO:
    ///
    /// 1. support >MTU sized packets (RSC/LRO/GRO)
    /// 2. allow discontiguous data to eliminate the extra copy from the TCP
    ///    window.
    fn recv(&mut self, data: &[u8], checksum: &ChecksumState);

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
}

impl ChecksumState {
    const NONE: Self = Self {
        ipv4: false,
        tcp: false,
        udp: false,
        tso: None,
    };
    const IPV4_ONLY: Self = Self {
        ipv4: true,
        tcp: false,
        udp: false,
        tso: None,
    };
    const TCP4: Self = Self {
        ipv4: true,
        tcp: true,
        udp: false,
        tso: None,
    };
    const UDP4: Self = Self {
        ipv4: true,
        tcp: false,
        udp: true,
        tso: None,
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

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
struct SocketAddress {
    ip: Ipv4Address,
    port: u16,
}

impl From<SocketAddress> for SocketAddrV4 {
    fn from(addr: SocketAddress) -> Self {
        Self::new(addr.ip, addr.port)
    }
}

impl From<SocketAddress> for socket2::SockAddr {
    fn from(addr: SocketAddress) -> Self {
        socket2::SockAddr::from(SocketAddrV4::from(addr))
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
struct FourTuple {
    dst: SocketAddress,
    src: SocketAddress,
}

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
    /// Specified port is not bound.
    #[error("port is not bound")]
    PortNotBound,
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

impl Consomme {
    /// Creates a new consomme instance with specified state.
    pub fn new(mut params: ConsommeParams) -> Self {
        let dns = dns_resolver::DnsResolver::new().ok();

        params.nameservers = dns
            .as_ref()
            .map(|_| vec![Ipv4Address::from_octets([10, 0, 0, 1])])
            .unwrap_or(params.nameservers);
        
        let timeout = params.udp_timeout;
        Self {
            state: ConsommeState {
                params,
                buffer: Box::new([0; 65536]),
            },
            tcp: tcp::Tcp::new(),
            udp: udp::Udp::new(timeout),
            icmp: icmp::Icmp::new(),
            dns,
        }
    }

    /// Get access to the parameters to be updated.
    ///
    /// FUTURE: add support for updating only the parameters that can be safely
    /// changed at runtime.
    pub fn params_mut(&mut self) -> &mut ConsommeParams {
        &mut self.state.params
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
            || payload.len() < ipv4.total_len().into()
        {
            return Err(DropReason::Packet(smoltcp::wire::Error));
        }

        let total_len = if checksum.tso.is_some() {
            payload.len()
        } else {
            ipv4.total_len().into()
        };
        if total_len < ipv4.header_len().into() {
            return Err(DropReason::Packet(smoltcp::wire::Error));
        }

        if ipv4.more_frags() || ipv4.frag_offset() != 0 {
            return Err(DropReason::Packet(smoltcp::wire::Error));
        }

        if !checksum.ipv4 && !ipv4.verify_checksum() {
            return Err(DropReason::Ipv4Checksum);
        }

        let addresses = Ipv4Addresses {
            src_addr: ipv4.src_addr(),
            dst_addr: ipv4.dst_addr(),
        };

        let inner = &payload[ipv4.header_len().into()..total_len];

        match ipv4.next_header() {
            IpProtocol::Tcp => self.handle_tcp(&addresses, inner, checksum)?,
            IpProtocol::Udp => self.handle_udp(frame, &addresses, inner, checksum)?,
            IpProtocol::Icmp => {
                self.handle_icmp(frame, &addresses, inner, checksum, ipv4.hop_limit())?
            }
            p => return Err(DropReason::UnsupportedIpProtocol(p)),
        };
        Ok(())
    }
}
