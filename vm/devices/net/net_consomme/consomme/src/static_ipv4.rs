// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exact static IPv4 identity and gateway loopback translation.
//!
//! [`ConsommeParams::set_static_ipv4`] replaces the default or CIDR-derived
//! addresses with an exact guest and gateway identity and enables the
//! translation of guest traffic to the gateway onto host loopback, which
//! [`ConsommeParams::map_gateway_to_host_loopback`] and
//! [`ConsommeParams::gateway_loopback_proxy_port`] control. The routable IPv6
//! gateway of a static IPv6 identity translates onto host IPv6 loopback under
//! the general mapping.

use crate::ConsommeParams;
use crate::ConsommeState;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use thiserror::Error;

/// An error indicating that a static IPv4 identity is internally inconsistent.
#[derive(Debug, Error)]
#[error("invalid static IPv4 identity")]
pub struct InvalidStaticIpv4;

impl ConsommeParams {
    /// Sets an exact static IPv4 client and gateway identity.
    pub fn set_static_ipv4(
        &mut self,
        guest_ipv4: Ipv4Addr,
        prefix_length: u8,
        gateway_ipv4: Ipv4Addr,
        gateway_mac: [u8; 6],
    ) -> Result<(), InvalidStaticIpv4> {
        if !(1..=30).contains(&prefix_length) {
            return Err(InvalidStaticIpv4);
        }
        let mask = u32::MAX << (32 - prefix_length);
        let guest = u32::from(guest_ipv4);
        let gateway = u32::from(gateway_ipv4);
        let network = guest & mask;
        let broadcast = network | !mask;
        if guest == network || guest == broadcast || gateway != network + 1 || guest == gateway {
            return Err(InvalidStaticIpv4);
        }

        self.client_ip = Ipv4Address::from(guest_ipv4.octets());
        self.gateway_ip = Ipv4Address::from(gateway_ipv4.octets());
        self.net_mask = Ipv4Address::from(Ipv4Addr::from(mask).octets());
        self.gateway_mac = EthernetAddress(gateway_mac);
        self.advertise_routable_ipv6 = false;
        self.map_gateway_to_host_loopback = true;
        Ok(())
    }
}

impl ConsommeState {
    /// Resolves the host destination of a guest flow of `protocol`.
    ///
    /// Guest traffic to the IPv4 gateway is translated onto host loopback when
    /// the gateway mapping or the gateway proxy port allows it and is rejected
    /// (`None`) otherwise. Guest traffic to the routable IPv6 gateway is
    /// translated onto host IPv6 loopback when the gateway mapping allows it
    /// and is rejected otherwise; the proxy port is an IPv4 exception. Other
    /// destinations are resolved as virtual mapped addresses.
    pub(crate) fn resolve_flow_destination(
        &self,
        addr: &SocketAddr,
        protocol: IpProtocol,
    ) -> Option<SocketAddr> {
        if let SocketAddr::V4(address) = addr
            && *address.ip() == self.params.gateway_ip
        {
            return (self.params.map_gateway_to_host_loopback
                || (protocol == IpProtocol::Tcp
                    && self.params.gateway_loopback_proxy_port == Some(address.port())))
            .then(|| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, address.port())));
        }
        if let SocketAddr::V6(address) = addr
            && self.params.gateway_ipv6 == Some(*address.ip())
        {
            return self.params.map_gateway_to_host_loopback.then(|| {
                SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, address.port(), 0, 0))
            });
        }
        Some(self.resolve_destination(addr))
    }
}
