// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exact static IPv4 identity and gateway loopback translation.
//!
//! [`ConsommeParams::set_static_ipv4`] replaces the default or CIDR-derived
//! addresses with an exact guest and gateway identity and enables the
//! translation of guest traffic to the gateway onto host loopback, which
//! [`ConsommeParams::map_gateway_to_host_loopback`] and
//! [`ConsommeParams::gateway_loopback_proxy_port`] control.

use crate::ConsommeParams;
use crate::ConsommeState;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
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
    /// (`None`) otherwise. Other destinations are resolved as virtual mapped
    /// addresses.
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
        Some(self.resolve_destination(addr))
    }
}
