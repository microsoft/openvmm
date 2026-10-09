// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exact static IPv6 identity of a dual-stack link.
//!
//! [`ConsommeParams::set_static_ipv6`] adds an exact guest and gateway IPv6
//! identity to an exact static IPv4 identity. The gateway keeps one Ethernet
//! address for both families, answers Neighbor Discovery for its subnet and
//! echo requests at its address, and its address is translated onto host
//! IPv6 loopback under the same posture as the IPv4 gateway.

use crate::Access;
use crate::ChecksumState;
use crate::Client;
use crate::ConsommeParams;
use crate::DropReason;
use crate::Ipv6Addresses;
use crate::MIN_MTU;
use smoltcp::wire::ETHERNET_HEADER_LEN;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::EthernetRepr;
use smoltcp::wire::IPV6_HEADER_LEN;
use smoltcp::wire::Icmpv6Message;
use smoltcp::wire::Icmpv6Packet;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv6Address;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::Ipv6Repr;
use std::net::Ipv6Addr;
use thiserror::Error;

/// The ICMPv6 header and the echo identifier and sequence number.
const ICMPV6_ECHO_HEADER_LEN: usize = 8;

/// An error indicating that a static IPv6 identity is internally inconsistent.
#[derive(Debug, Error)]
#[error("invalid static IPv6 identity")]
pub struct InvalidStaticIpv6;

impl ConsommeParams {
    /// Adds an exact static IPv6 client and gateway identity to the exact
    /// static IPv4 identity that [`ConsommeParams::set_static_ipv4`] set, and
    /// whose gateway Ethernet address the IPv6 gateway shares.
    ///
    /// The gateway is the first address after the subnet's network address.
    /// IPv6 traffic is handled whether or not the host has a routable IPv6
    /// address, since the gateway's own services do not need one.
    pub fn set_static_ipv6(
        &mut self,
        guest_ipv6: Ipv6Addr,
        prefix_length: u8,
        gateway_ipv6: Ipv6Addr,
    ) -> Result<(), InvalidStaticIpv6> {
        if !(1..=126).contains(&prefix_length)
            || guest_ipv6.is_unspecified()
            || guest_ipv6.is_loopback()
            || guest_ipv6.is_multicast()
            || guest_ipv6.is_unicast_link_local()
            || guest_ipv6.to_ipv4_mapped().is_some()
        {
            return Err(InvalidStaticIpv6);
        }
        let mask = u128::MAX << (128 - u32::from(prefix_length));
        let guest = u128::from(guest_ipv6);
        let network = guest & mask;
        if guest == network || u128::from(gateway_ipv6) != network + 1 || guest_ipv6 == gateway_ipv6
        {
            return Err(InvalidStaticIpv6);
        }

        self.client_ip_ipv6_routable = Some(guest_ipv6);
        self.client_ip_ipv6 = Some(Self::compute_link_local_address(self.client_mac));
        self.prefix_len_ipv6 = prefix_length;
        self.gateway_ipv6 = Some(gateway_ipv6);
        self.gateway_mac_ipv6 = self.gateway_mac;
        self.gateway_link_local_ipv6 = Self::compute_link_local_address(self.gateway_mac);
        self.skip_ipv6_checks = true;
        Ok(())
    }
}

impl<T: Client> Access<'_, T> {
    /// Answers an echo request to the static IPv6 gateway directly, as the
    /// IPv4 gateway answers pings, without involving a host socket.
    pub(crate) fn handle_icmpv6_gateway_echo(
        &mut self,
        frame: &EthernetRepr,
        addresses: &Ipv6Addresses,
        payload: &[u8],
    ) -> Result<(), DropReason> {
        let total_len = ETHERNET_HEADER_LEN + IPV6_HEADER_LEN + payload.len();
        if payload.len() < ICMPV6_ECHO_HEADER_LEN || total_len > MIN_MTU {
            return Err(DropReason::MalformedPacket);
        }
        let mut buffer = [0u8; MIN_MTU];
        let mut eth = EthernetFrame::new_unchecked(&mut buffer[..]);
        EthernetRepr {
            src_addr: self.inner.state.params.gateway_mac_ipv6,
            dst_addr: frame.src_addr,
            ethertype: EthernetProtocol::Ipv6,
        }
        .emit(&mut eth);
        let ipv6_repr = Ipv6Repr {
            src_addr: addresses.dst_addr,
            dst_addr: addresses.src_addr,
            next_header: IpProtocol::Icmpv6,
            payload_len: payload.len(),
            hop_limit: 64,
        };
        let mut ipv6 = Ipv6Packet::new_unchecked(eth.payload_mut());
        ipv6_repr.emit(&mut ipv6);
        let reply_buf = &mut ipv6.payload_mut()[..payload.len()];
        reply_buf.copy_from_slice(payload);
        let mut reply = Icmpv6Packet::new_unchecked(reply_buf);
        reply.set_msg_type(Icmpv6Message::EchoReply);
        reply.fill_checksum(&ipv6_repr.src_addr, &ipv6_repr.dst_addr);
        self.client.recv(&buffer[..total_len], &ChecksumState::NONE);
        Ok(())
    }
}

/// Returns whether `address` belongs to the static IPv6 subnet of `params`.
pub(crate) fn is_static_ipv6_neighbor(params: &ConsommeParams, address: Ipv6Address) -> bool {
    params
        .gateway_ipv6
        .is_some_and(|gateway| crate::is_same_ipv6_subnet(gateway, address, params.prefix_len_ipv6))
}
