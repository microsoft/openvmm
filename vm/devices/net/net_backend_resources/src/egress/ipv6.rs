// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IPv6 networks, the IPv6 identity of a dual-stack link, and the
//! authorization of IPv6 frames.

use super::ETHERNET_UNSPECIFIED;
use super::EgressDenied;
use super::EgressPolicy;
use super::EgressPolicyMode;
use super::EgressTransport;
use super::InvalidEgressPolicy;
use super::PacketTransport;
use super::ensure_available;
use super::read_u16;
use crate::mac_address::MacAddress;
use mesh::MeshPayload;
use std::net::IpAddr;
use std::net::Ipv6Addr;
use std::str::FromStr;
use thiserror::Error;

/// The Ethernet type of IPv6.
pub(super) const ETHER_TYPE_IPV6: u16 = 0x86dd;
const HEADER_LENGTH: usize = 40;
const NEXT_HEADER_TCP: u8 = 6;
const NEXT_HEADER_UDP: u8 = 17;
const NEXT_HEADER_ICMPV6: u8 = 58;
/// The IANA IPv6 extension header types: hop-by-hop options, routing,
/// fragment, ESP, AH, destination options, mobility, HIP, shim6, and the two
/// experimental types.
const EXTENSION_HEADERS: [u8; 11] = [0, 43, 44, 50, 51, 60, 135, 139, 140, 253, 254];
const ROUTER_SOLICITATION: u8 = 133;
const NEIGHBOR_SOLICITATION: u8 = 135;
const REDIRECT: u8 = 137;
/// The ICMPv6 header, the reserved field, and the target address.
const NEIGHBOR_SOLICITATION_LENGTH: usize = 24;
const SOURCE_LINK_LAYER_ADDRESS_OPTION: u8 = 1;
const NEIGHBOR_DISCOVERY_HOP_LIMIT: u8 = 255;

/// A canonical IPv6 network prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub struct Ipv6Cidr {
    network: Ipv6Addr,
    prefix_length: u8,
}

impl Ipv6Cidr {
    /// Returns whether `address` belongs to this prefix.
    pub fn contains(self, address: Ipv6Addr) -> bool {
        u128::from(address) & prefix_mask(self.prefix_length) == u128::from(self.network)
    }

    /// Returns the canonical network address.
    pub fn network(self) -> Ipv6Addr {
        self.network
    }

    /// Returns the prefix length.
    pub fn prefix_length(self) -> u8 {
        self.prefix_length
    }
}

/// Error returned when parsing an IPv6 address or CIDR policy rule.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseIpv6CidrError {
    /// The address portion is not IPv6.
    #[error("invalid IPv6 address '{0}'")]
    InvalidAddress(String),
    /// The prefix portion is not an integer.
    #[error("invalid IPv6 prefix '{0}'")]
    InvalidPrefix(String),
    /// The prefix is greater than 128.
    #[error("IPv6 prefix /{0} is outside the supported range /0 through /128")]
    PrefixOutOfRange(u8),
    /// The rule has more than one prefix separator.
    #[error("invalid IPv6 CIDR '{0}'")]
    InvalidFormat(String),
}

impl FromStr for Ipv6Cidr {
    type Err = ParseIpv6CidrError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) if !prefix.contains('/') => (address, Some(prefix)),
            Some(_) => return Err(ParseIpv6CidrError::InvalidFormat(value.to_owned())),
            None => (value, None),
        };
        let address = address
            .parse::<Ipv6Addr>()
            .map_err(|_| ParseIpv6CidrError::InvalidAddress(address.to_owned()))?;
        let prefix_length = prefix
            .map(|prefix| {
                prefix
                    .parse::<u8>()
                    .map_err(|_| ParseIpv6CidrError::InvalidPrefix(prefix.to_owned()))
            })
            .transpose()?
            .unwrap_or(128);
        if prefix_length > 128 {
            return Err(ParseIpv6CidrError::PrefixOutOfRange(prefix_length));
        }
        Ok(Self {
            network: Ipv6Addr::from(u128::from(address) & prefix_mask(prefix_length)),
            prefix_length,
        })
    }
}

/// The IPv6 identity of a dual-stack microVM link: the guest's address and
/// subnet, whose first address after the network address is the gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq, MeshPayload)]
pub struct EgressIpv6Link {
    guest: Ipv6Addr,
    prefix_length: u8,
    gateway: Ipv6Addr,
}

impl EgressIpv6Link {
    pub(super) fn new(
        guest: Ipv6Addr,
        prefix_length: u8,
        gateway: Ipv6Addr,
    ) -> Result<Self, InvalidEgressPolicy> {
        if !(1..=126).contains(&prefix_length) {
            return Err(InvalidEgressPolicy::Ipv6PrefixOutOfRange(prefix_length));
        }
        if guest.is_unspecified()
            || guest.is_loopback()
            || guest.is_multicast()
            || is_link_local(guest)
            || guest.to_ipv4_mapped().is_some()
        {
            return Err(InvalidEgressPolicy::InvalidGuestIpv6(guest));
        }
        let network = u128::from(guest) & prefix_mask(prefix_length);
        if u128::from(guest) == network {
            return Err(InvalidEgressPolicy::GuestIpv6NetworkAddress(guest));
        }
        let expected = Ipv6Addr::from(network + 1);
        if guest == expected {
            return Err(InvalidEgressPolicy::GuestIpv6GatewayCollision(guest));
        }
        if gateway != expected {
            return Err(InvalidEgressPolicy::Ipv6GatewayMismatch {
                actual: gateway,
                expected,
            });
        }
        Ok(Self {
            guest,
            prefix_length,
            gateway,
        })
    }

    /// Returns the guest IPv6 address.
    pub fn guest(self) -> Ipv6Addr {
        self.guest
    }

    /// Returns the subnet prefix length.
    pub fn prefix_length(self) -> u8 {
        self.prefix_length
    }

    /// Returns the gateway IPv6 address.
    pub fn gateway(self) -> Ipv6Addr {
        self.gateway
    }

    /// Returns whether `address` belongs to the link's subnet.
    pub fn contains(self, address: Ipv6Addr) -> bool {
        let mask = prefix_mask(self.prefix_length);
        u128::from(address) & mask == u128::from(self.guest) & mask
    }
}

impl EgressPolicy {
    /// Authorizes an IPv6 frame, which only rules on a dual-stack link admit.
    pub(super) fn authorize_ipv6(
        &self,
        frame: &[u8],
        frame_length: usize,
        offset: usize,
    ) -> Result<(), EgressDenied> {
        let (Some(link), EgressPolicyMode::Rules { .. }) = (self.ipv6, &self.mode) else {
            return Err(EgressDenied::UnsupportedEtherType(ETHER_TYPE_IPV6));
        };
        ensure_available(frame, frame_length, offset + HEADER_LENGTH, "IPv6 header")?;
        if frame[offset] >> 4 != 6 {
            return Err(EgressDenied::Malformed("IPv6 header"));
        }
        let payload_offset = offset + HEADER_LENGTH;
        let payload_length = usize::from(read_u16(frame, offset + 4));
        if payload_offset + payload_length > frame_length {
            return Err(EgressDenied::Malformed("IPv6 payload length"));
        }
        let next_header = frame[offset + 6];
        if next_header == NEXT_HEADER_ICMPV6 && payload_length > 0 {
            ensure_available(frame, frame_length, payload_offset + 1, "ICMPv6 header")?;
            if (ROUTER_SOLICITATION..=REDIRECT).contains(&frame[payload_offset]) {
                return self.authorize_neighbor_discovery(
                    link,
                    frame,
                    frame_length,
                    offset,
                    payload_length,
                );
            }
        }
        if read_ipv6(frame, offset + 8) != link.guest {
            return Err(EgressDenied::SourceAddressDenied);
        }
        if EXTENSION_HEADERS.contains(&next_header) {
            return Err(EgressDenied::Ipv6ExtensionHeaderDenied);
        }
        let destination = read_ipv6(frame, offset + 24);
        if destination.to_ipv4_mapped().is_some() {
            return Err(EgressDenied::Ipv4MappedDestinationDenied);
        }
        let packet = match next_header {
            NEXT_HEADER_TCP => {
                ensure_available(frame, frame_length, payload_offset + 20, "TCP header")?;
                if payload_length < 20 {
                    return Err(EgressDenied::Malformed("TCP length"));
                }
                let header_length = usize::from(frame[payload_offset + 12] >> 4) * 4;
                if header_length < 20 || header_length > payload_length {
                    return Err(EgressDenied::Malformed("TCP header"));
                }
                Some(PacketTransport {
                    transport: EgressTransport::Tcp,
                    port: Some(read_u16(frame, payload_offset + 2)),
                })
            }
            NEXT_HEADER_UDP => {
                ensure_available(frame, frame_length, payload_offset + 8, "UDP header")?;
                let length = usize::from(read_u16(frame, payload_offset + 4));
                if length < 8 || length > payload_length {
                    return Err(EgressDenied::Malformed("UDP length"));
                }
                Some(PacketTransport {
                    transport: EgressTransport::Udp,
                    port: Some(read_u16(frame, payload_offset + 2)),
                })
            }
            NEXT_HEADER_ICMPV6 => {
                ensure_available(frame, frame_length, payload_offset + 8, "ICMPv6 header")?;
                if payload_length < 8 {
                    return Err(EgressDenied::Malformed("ICMPv6 length"));
                }
                Some(PacketTransport {
                    transport: EgressTransport::Icmp,
                    port: None,
                })
            }
            _ => None,
        };
        if self.rules_allow_destination(IpAddr::V6(destination), packet) {
            Ok(())
        } else {
            Err(EgressDenied::DestinationDenied)
        }
    }

    /// Authorizes Neighbor Discovery, which admits only the guest's Neighbor
    /// Solicitation of an on-link neighbor that the rules let it resolve,
    /// sent to the neighbor's solicited-node multicast group or, to confirm
    /// reachability, to the neighbor itself.
    fn authorize_neighbor_discovery(
        &self,
        link: EgressIpv6Link,
        frame: &[u8],
        frame_length: usize,
        offset: usize,
        payload_length: usize,
    ) -> Result<(), EgressDenied> {
        let icmp = offset + HEADER_LENGTH;
        if frame[icmp] != NEIGHBOR_SOLICITATION {
            return Err(EgressDenied::NeighborDiscoveryDenied);
        }
        if payload_length < NEIGHBOR_SOLICITATION_LENGTH {
            return Err(EgressDenied::Malformed("Neighbor Solicitation"));
        }
        ensure_available(
            frame,
            frame_length,
            icmp + NEIGHBOR_SOLICITATION_LENGTH,
            "Neighbor Solicitation",
        )?;
        if frame[offset + 7] != NEIGHBOR_DISCOVERY_HOP_LIMIT || frame[icmp + 1] != 0 {
            return Err(EgressDenied::Malformed("Neighbor Solicitation"));
        }
        let source = read_ipv6(frame, offset + 8);
        let destination = read_ipv6(frame, offset + 24);
        let target = read_ipv6(frame, icmp + 8);
        let ethernet_destination = &frame[..6];
        let addressed = if destination == solicited_node_address(target) {
            ethernet_destination == multicast_mac_address(destination)
        } else {
            destination == target
                && ethernet_destination[0] & 1 == 0
                && ethernet_destination != ETHERNET_UNSPECIFIED
        };
        if !(source == link.guest || source == link_local_address(self.guest_mac))
            || !addressed
            || target == link.guest
            || !link.contains(target)
            || !self.rules_allow_neighbor(IpAddr::V6(target), IpAddr::V6(link.gateway))
        {
            return Err(EgressDenied::NeighborDiscoveryDenied);
        }
        let end = icmp + payload_length;
        let mut option = icmp + NEIGHBOR_SOLICITATION_LENGTH;
        while option < end {
            ensure_available(frame, frame_length, option + 2, "Neighbor Discovery option")?;
            let length = usize::from(frame[option + 1]) * 8;
            if length == 0 || option + length > end {
                return Err(EgressDenied::Malformed("Neighbor Discovery option"));
            }
            if frame[option] == SOURCE_LINK_LAYER_ADDRESS_OPTION {
                ensure_available(frame, frame_length, option + 8, "Neighbor Discovery option")?;
                if length != 8 || frame[option + 2..option + 8] != self.guest_mac.to_bytes() {
                    return Err(EgressDenied::NeighborDiscoveryDenied);
                }
            }
            option += length;
        }
        Ok(())
    }
}

fn prefix_mask(prefix_length: u8) -> u128 {
    if prefix_length == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix_length))
    }
}

fn is_link_local(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xffc0 == 0xfe80
}

fn read_ipv6(bytes: &[u8], offset: usize) -> Ipv6Addr {
    let mut octets = [0; 16];
    octets.copy_from_slice(&bytes[offset..offset + 16]);
    Ipv6Addr::from(octets)
}

/// Returns the link-local address that `mac` forms as a modified EUI-64
/// interface identifier (RFC 4291, appendix A).
fn link_local_address(mac: MacAddress) -> Ipv6Addr {
    let mac = mac.to_bytes();
    Ipv6Addr::from([
        0xfe,
        0x80,
        0,
        0,
        0,
        0,
        0,
        0,
        mac[0] ^ 0x02,
        mac[1],
        mac[2],
        0xff,
        0xfe,
        mac[3],
        mac[4],
        mac[5],
    ])
}

/// Returns the solicited-node multicast address of `target` (RFC 4291,
/// section 2.7.1).
fn solicited_node_address(target: Ipv6Addr) -> Ipv6Addr {
    let target = target.octets();
    Ipv6Addr::from([
        0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0xff, target[13], target[14], target[15],
    ])
}

/// Returns the Ethernet address of the IPv6 multicast `group` (RFC 2464,
/// section 7).
fn multicast_mac_address(group: Ipv6Addr) -> [u8; 6] {
    let group = group.octets();
    [0x33, 0x33, group[12], group[13], group[14], group[15]]
}

#[cfg(test)]
mod tests;
