// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NDP (Neighbor Discovery Protocol) implementation for IPv6 SLAAC (Stateless Address Autoconfiguration)
//!
//! This module implements RFC 4861 (Neighbor Discovery) and RFC 4862 (IPv6 Stateless Address Autoconfiguration).
//! The implementation is stateless - we advertise prefixes via Router Advertisements and let clients
//! autoconfigure their own addresses using SLAAC.

use super::Access;
use super::Client;
use super::DropReason;
use crate::ChecksumState;
use crate::MIN_MTU;
use smoltcp::phy::Medium;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::EthernetRepr;
use smoltcp::wire::HardwareAddress;
use smoltcp::wire::Icmpv6Packet;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv6Address;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::Ipv6Repr;
use smoltcp::wire::NdiscNeighborFlags;
use smoltcp::wire::NdiscPrefixInfoFlags;
use smoltcp::wire::NdiscPrefixInformation;
use smoltcp::wire::NdiscRepr;
use smoltcp::wire::NdiscRouterFlags;
use smoltcp::wire::RawHardwareAddress;

const NETWORK_PREFIX_BASE: Ipv6Address = Ipv6Address::new(0x2001, 0xabcd, 0, 0, 0, 0, 0, 0);
const LINK_LOCAL_ALL_NODES: Ipv6Address = Ipv6Address::from_octets([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

#[derive(Debug)]
pub enum NdpMessageType {
    RouterSolicit,
    RouterAdvert,
    NeighborSolicit,
    NeighborAdvert,
    Redirect,
}

impl<T: Client> Access<'_, T> {
    /// Handle NDP messages from the guest
    pub(crate) fn handle_ndp(
        &mut self,
        frame: &EthernetRepr,
        payload: &[u8],
        ipv6_src_addr: Ipv6Address,
    ) -> Result<(), DropReason> {
        let icmpv6_packet = Icmpv6Packet::new_unchecked(payload);
        let ndp = NdiscRepr::parse(&icmpv6_packet)?;

        match ndp {
            NdiscRepr::RouterSolicit { lladdr } => {
                self.handle_router_solicit(frame, ipv6_src_addr, lladdr)
            }
            NdiscRepr::NeighborSolicit {
                target_addr,
                lladdr: source_lladdr,
            } => self.handle_neighbor_solicit(frame, ipv6_src_addr, target_addr, source_lladdr),
            NdiscRepr::NeighborAdvert { .. } => {
                tracing::trace!("received unsolicited Neighbor Advertisement, ignoring");
                Ok(())
            }
            NdiscRepr::RouterAdvert { .. } => {
                tracing::trace!("received Router Advertisement, ignoring");
                Ok(())
            }
            NdiscRepr::Redirect { .. } => {
                tracing::trace!("received Redirect, ignoring");
                Ok(())
            }
        }
    }

    /// Handle Router Solicitation (RFC 4861 Section 6.2.6)
    ///
    /// Router Solicitations are sent by hosts to discover routers on the link.
    /// We respond with a Router Advertisement containing prefix information for SLAAC.
    fn handle_router_solicit(
        &mut self,
        frame: &EthernetRepr,
        ipv6_src_addr: Ipv6Address,
        lladdr: Option<RawHardwareAddress>,
    ) -> Result<(), DropReason> {
        // RFC 4861 Section 6.1.1: Validate source link-layer address option
        // If source is unspecified (::), there must be no source link-layer address option
        if ipv6_src_addr.is_unspecified() && lladdr.is_some() {
            tracelimit::warn_ratelimited!(
                "invalid RS: source is :: but source link-layer address present"
            );
            return Err(DropReason::MalformedPacket);
        }

        // Verify this is from the expected client MAC (if link-layer address is provided)
        if let Some(lladdr) = lladdr {
            if let Ok(hw_addr) = lladdr.parse(Medium::Ethernet) {
                let HardwareAddress::Ethernet(eth_addr) = hw_addr;
                if eth_addr != self.inner.state.params.client_mac {
                    tracelimit::warn_ratelimited!(
                        "Router Solicitation from unexpected MAC, ignoring"
                    );
                    return Ok(());
                }
            }
        }

        // Determine destination address for the reply
        // RFC 4861 Section 6.2.6: If RS has source link-layer address option,
        // unicast to source. Otherwise, use all-nodes multicast.
        let reply_dst_addr = if lladdr.is_some() && !ipv6_src_addr.is_unspecified() {
            ipv6_src_addr
        } else {
            LINK_LOCAL_ALL_NODES
        };

        // Determine Ethernet destination
        let eth_dst_addr = if reply_dst_addr.is_multicast() {
            // Multicast IPv6 to Ethernet address mapping (RFC 2464)
            // 33:33:xx:xx:xx:xx where xx:xx:xx:xx are the low-order 32 bits of the IPv6 multicast address
            let octets = reply_dst_addr.octets();
            EthernetAddress([
                0x33,
                0x33,
                octets[12],
                octets[13],
                octets[14],
                octets[15],
            ])
        } else {
            frame.src_addr
        };

        self.send_router_advertisement(reply_dst_addr, eth_dst_addr)
    }

    /// Send a Router Advertisement (RFC 4861 Section 4.2)
    ///
    /// Router Advertisements contain prefix information for SLAAC. Clients will use
    /// the advertised prefix to generate their own IPv6 addresses.
    fn send_router_advertisement(
        &mut self,
        dst_addr: Ipv6Address,
        eth_dst_addr: EthernetAddress,
    ) -> Result<(), DropReason> {
        // Compute the network prefix from our configured IPv6 parameters
        // This is the prefix that clients will use for SLAAC
        let prefix = self
            .compute_network_prefix(NETWORK_PREFIX_BASE, self.inner.state.params.prefix_len_ipv6);

        // RFC 4861 Section 4.6.2: Router Advertisement with Prefix Information
        // We set the ADDRCONF flag to enable SLAAC and ON_LINK flag to indicate
        // that addresses with this prefix are on-link.
        let ndp_repr = NdiscRepr::RouterAdvert {
            hop_limit: 255,
            flags: NdiscRouterFlags::empty(),
            router_lifetime: smoltcp::time::Duration::from_secs(9000), // https://www.rfc-editor.org/rfc/rfc4861#section-4.2
            reachable_time: smoltcp::time::Duration::from_millis(30000), // https://www.rfc-editor.org/rfc/rfc4861#section-6
            retrans_time: smoltcp::time::Duration::from_millis(1000), // https://www.rfc-editor.org/rfc/rfc4861#section-6
            lladdr: Some(RawHardwareAddress::from(
                self.inner.state.params.gateway_mac_ipv6,
            )),
            mtu: None,
            prefix_info: Some(NdiscPrefixInformation {
                prefix_len: self.inner.state.params.prefix_len_ipv6,
                prefix,
                valid_lifetime: smoltcp::time::Duration::from_secs(2592000), // https://www.rfc-editor.org/rfc/rfc4861#section-6.2.1
                preferred_lifetime: smoltcp::time::Duration::from_secs(604800), // https://www.rfc-editor.org/rfc/rfc4861#section-6.2.1
                flags: NdiscPrefixInfoFlags::ON_LINK | NdiscPrefixInfoFlags::ADDRCONF,
            }),
        };

        // Build IPv6 header
        let ipv6_repr = Ipv6Repr {
            src_addr: self.inner.state.params.gateway_link_local_ipv6,
            dst_addr,
            next_header: IpProtocol::Icmpv6,
            payload_len: ndp_repr.buffer_len(),
            hop_limit: 255, // Router advertisements must have a hop limit of 255 to indicate the packet was not forwarded by another router.
        };

        let eth_repr = EthernetRepr {
            src_addr: self.inner.state.params.gateway_mac_ipv6,
            dst_addr: eth_dst_addr,
            ethertype: EthernetProtocol::Ipv6,
        };

        let mut buffer = [0; MIN_MTU];
        let mut eth_frame = EthernetFrame::new_unchecked(&mut buffer);
        eth_repr.emit(&mut eth_frame);

        let mut ipv6_packet = Ipv6Packet::new_unchecked(eth_frame.payload_mut());
        ipv6_repr.emit(&mut ipv6_packet);

        let mut icmpv6_packet = Icmpv6Packet::new_unchecked(ipv6_packet.payload_mut());
        ndp_repr.emit(&mut icmpv6_packet);
        icmpv6_packet.fill_checksum(
            &ipv6_repr.src_addr,
            &ipv6_repr.dst_addr,
        );

        let total_len = eth_repr.buffer_len() + ipv6_repr.buffer_len() + ndp_repr.buffer_len();

        self.client.recv(&buffer[..total_len], &ChecksumState::NONE);
        Ok(())
    }

    /// Handle Neighbor Solicitation (RFC 4861 Section 7.2.3)
    ///
    /// Neighbor Solicitations are used for:
    /// 1. Address resolution (discovering link-layer address of a neighbor)
    /// 2. Duplicate Address Detection (DAD) - verifying address uniqueness
    /// 3. Neighbor Unreachability Detection (NUD)
    fn handle_neighbor_solicit(
        &mut self,
        frame: &EthernetRepr,
        ipv6_src_addr: Ipv6Address,
        target_addr: Ipv6Address,
        source_lladdr: Option<RawHardwareAddress>,
    ) -> Result<(), DropReason> {
        // RFC 4861 Section 7.1.1: If source is unspecified, there must be no
        // source link-layer address option
        if ipv6_src_addr.is_unspecified() && source_lladdr.is_some() {
            tracelimit::warn_ratelimited!(
                "invalid NS: source is :: but source link-layer address present"
            );
            return Err(DropReason::MalformedPacket);
        }

        // RFC 4862 Section 5.4.3: Handle Duplicate Address Detection (DAD)
        // If source is unspecified (::), this is DAD - we should NOT respond
        // to avoid interfering with the client's address configuration
        if ipv6_src_addr.is_unspecified() {
            tracelimit::warn_ratelimited!(
                target_addr = %target_addr,
                "received DAD Neighbor Solicitation, silently ignoring per RFC 4862"
            );
            return Ok(());
        }

        // Verify this is from the expected client MAC
        let client_mac_matches = source_lladdr
            .and_then(|addr| addr.parse(Medium::Ethernet).ok())
            .map(|hw_addr| match hw_addr {
                HardwareAddress::Ethernet(eth_addr) => {
                    eth_addr == self.inner.state.params.client_mac
                }
                #[allow(unreachable_patterns)]
                _ => false,
            })
            .unwrap_or(false);

        if !client_mac_matches {
            tracelimit::warn_ratelimited!("Neighbor Solicitation from unexpected MAC, ignoring");
            return Ok(());
        }

        // Learn client IPv6 address from Neighbor Solicitation
        // When the client performs address resolution using their SLAAC-configured
        // global address, we learn it here. We only learn global unicast addresses
        // (not link-local, multicast, or unspecified).
        if !ipv6_src_addr.is_unicast_link_local()
            && !ipv6_src_addr.is_multicast()
            && !ipv6_src_addr.is_unspecified()
        {
            if self.inner.state.params.client_ip_ipv6.is_none()
                || self.inner.state.params.client_ip_ipv6 != Some(ipv6_src_addr)
            {
                tracing::debug!(
                    client_ipv6 = %ipv6_src_addr,
                    "learned client IPv6 address from Neighbor Solicitation"
                );
                self.inner.state.params.client_ip_ipv6 = Some(ipv6_src_addr);
            }
        }

        // Only respond if the target is our link-local address
        // In a stateless NAT implementation, the gateway only responds for its own
        // link-local address, not for global addresses that clients autoconfigure
        if target_addr != self.inner.state.params.gateway_link_local_ipv6 {
            tracing::debug!(
                target_addr = %target_addr,
                our_link_local = %self.inner.state.params.gateway_link_local_ipv6,
                "NS target is not our link-local address, ignoring"
            );
            return Ok(());
        }

        // Send Neighbor Advertisement
        self.send_neighbor_advertisement(ipv6_src_addr, frame.src_addr, target_addr, true)
    }

    /// Send a Neighbor Advertisement (RFC 4861 Section 7.2.4)
    ///
    /// Neighbor Advertisements are sent in response to Neighbor Solicitations
    /// to provide our link-layer address for address resolution.
    fn send_neighbor_advertisement(
        &mut self,
        dst_addr: Ipv6Address,
        eth_dst_addr: EthernetAddress,
        target_addr: Ipv6Address,
        solicited: bool,
    ) -> Result<(), DropReason> {
        // RFC 4861 Section 7.2.4: Neighbor Advertisement format
        // Solicited flag = 1 (this is a response to a solicitation)
        // Override flag = 1 (we're authoritative for this address)
        // Router flag = 1 (we are a router)
        let mut flags = NdiscNeighborFlags::OVERRIDE;
        if solicited {
            flags |= NdiscNeighborFlags::SOLICITED;
        }
        flags |= NdiscNeighborFlags::ROUTER;

        let ndp_repr = NdiscRepr::NeighborAdvert {
            flags,
            target_addr,
            lladdr: Some(RawHardwareAddress::from(
                self.inner.state.params.gateway_mac_ipv6,
            )),
        };

        // Build IPv6 header - destination is the source of the solicitation
        let ipv6_repr = Ipv6Repr {
            src_addr: target_addr, // Our address (the one being asked about)
            dst_addr,              // Respond to the solicitation's source
            next_header: IpProtocol::Icmpv6,
            payload_len: ndp_repr.buffer_len(),
            hop_limit: 255, // RFC 4861: Neighbor Advertisements must have a hop limit of 255 to indicate the packet was not forwarded.
        };

        // Build Ethernet header
        let eth_repr = EthernetRepr {
            src_addr: self.inner.state.params.gateway_mac_ipv6,
            dst_addr: eth_dst_addr,
            ethertype: EthernetProtocol::Ipv6,
        };

        // Construct the complete packet
        let mut buffer = [0; MIN_MTU];
        let mut eth_frame = EthernetFrame::new_unchecked(&mut buffer);
        eth_repr.emit(&mut eth_frame);

        let mut ipv6_packet = Ipv6Packet::new_unchecked(eth_frame.payload_mut());
        ipv6_repr.emit(&mut ipv6_packet);

        let mut icmpv6_packet = Icmpv6Packet::new_unchecked(ipv6_packet.payload_mut());
        ndp_repr.emit(&mut icmpv6_packet);
        icmpv6_packet.fill_checksum(
            &ipv6_repr.src_addr,
            &ipv6_repr.dst_addr,
        );

        let total_len = eth_repr.buffer_len() + ipv6_repr.buffer_len() + ndp_repr.buffer_len();

        self.client.recv(&buffer[..total_len], &ChecksumState::NONE);
        Ok(())
    }

    /// Compute the network prefix from an IPv6 address and prefix length
    ///
    /// This extracts the network portion of an IPv6 address by applying
    /// a mask based on the prefix length.
    fn compute_network_prefix(&self, addr: Ipv6Address, prefix_len: u8) -> Ipv6Address {
        if prefix_len >= 128 {
            return addr;
        }

        let addr_u128 = u128::from_be_bytes(addr.octets());
        let mask = if prefix_len == 0 {
            0u128
        } else {
            (!0u128) << (128 - prefix_len)
        };

        Ipv6Address::from_octets((addr_u128 & mask).to_be_bytes())
    }
}
