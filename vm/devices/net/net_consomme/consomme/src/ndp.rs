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
use smoltcp::wire::IpAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv6Address;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::Ipv6Repr;
use smoltcp::wire::NdiscNeighborFlags;
use smoltcp::wire::NdiscPrefixInformation;
use smoltcp::wire::NdiscPrefixInfoFlags;
use smoltcp::wire::NdiscRepr;
use smoltcp::wire::NdiscRouterFlags;
use smoltcp::wire::RawHardwareAddress;

/// Well-known IPv6 link-local prefix
const LINK_LOCAL_PREFIX: [u8; 8] = [0xfe, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

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
        full_frame: &[u8],
    ) -> Result<(), DropReason> {
        let icmpv6_packet = Icmpv6Packet::new_unchecked(payload);
        let ndp = NdiscRepr::parse(&icmpv6_packet)?;

        match ndp {
            NdiscRepr::RouterSolicit { lladdr } => {
                self.handle_router_solicit(frame, ipv6_src_addr, lladdr, full_frame)
            }
            NdiscRepr::NeighborSolicit {
                target_addr,
                lladdr: source_lladdr,
            } => self.handle_neighbor_solicit(
                frame,
                ipv6_src_addr,
                target_addr,
                source_lladdr,
                full_frame,
            ),
            NdiscRepr::NeighborAdvert { .. } => {
                tracing::debug!("received unsolicited Neighbor Advertisement, ignoring");
                Ok(())
            }
            NdiscRepr::RouterAdvert { .. } => {
                tracing::debug!("received Router Advertisement, ignoring");
                Ok(())
            }
            NdiscRepr::Redirect { .. } => {
                tracing::debug!("received Redirect, ignoring");
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
        full_frame: &[u8],
    ) -> Result<(), DropReason> {
        let hex_frame = full_frame
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();
        tracing::info!(
            src_addr = %ipv6_src_addr,
            frame_len = full_frame.len(),
            has_lladdr = lladdr.is_some(),
            frame = %hex_frame,
            "received Router Solicitation"
        );

        // RFC 4861 Section 6.1.1: Validate source link-layer address option
        // If source is unspecified (::), there must be no source link-layer address option
        if ipv6_src_addr.is_unspecified() && lladdr.is_some() {
            tracing::warn!("invalid RS: source is :: but source link-layer address present");
            return Err(DropReason::Packet(smoltcp::Error::Malformed));
        }

        // Verify this is from the expected client MAC (if link-layer address is provided)
        if let Some(lladdr) = lladdr {
            if let Ok(hw_addr) = lladdr.parse(Medium::Ethernet) {
                let HardwareAddress::Ethernet(eth_addr) = hw_addr; 
                if eth_addr != self.inner.state.params.client_mac {
                    tracing::info!("Router Solicitation from unexpected MAC, ignoring");
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
            Ipv6Address::LINK_LOCAL_ALL_NODES
        };

        // Determine Ethernet destination
        let eth_dst_addr = if reply_dst_addr.is_multicast() {
            // Multicast IPv6 to Ethernet address mapping (RFC 2464)
            // 33:33:xx:xx:xx:xx where xx:xx:xx:xx are the low-order 32 bits of the IPv6 multicast address
            EthernetAddress([
                0x33,
                0x33,
                reply_dst_addr.0[12],
                reply_dst_addr.0[13],
                reply_dst_addr.0[14],
                reply_dst_addr.0[15],
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
        let prefix = self.compute_network_prefix(
            self.inner.state.params.gateway_ip_ipv6,
            self.inner.state.params.prefix_len_ipv6,
        );

        // Compute our link-local address for the source
        // The gateway uses a link-local address as the source of Router Advertisements
        let link_local_src = self.compute_link_local_address(self.inner.state.params.gateway_mac_ipv6);

        // RFC 4861 Section 4.6.2: Router Advertisement with Prefix Information
        // We set the AUTONOMOUS flag to enable SLAAC and ON_LINK flag to indicate
        // that addresses with this prefix are on-link.
        let ndp_repr = NdiscRepr::RouterAdvert {
            hop_limit: 64, // Default hop limit for outgoing packets
            flags: NdiscRouterFlags::empty(), // No MANAGED or OTHER flags (stateless only)
            router_lifetime: smoltcp::time::Duration::from_secs(1800), // 30 minutes
            reachable_time: smoltcp::time::Duration::from_millis(0),   // Unspecified
            retrans_time: smoltcp::time::Duration::from_millis(0),     // Unspecified
            lladdr: Some(RawHardwareAddress::from(
                self.inner.state.params.gateway_mac_ipv6,
            )),
            mtu: Some(1500), // Standard Ethernet MTU
            prefix_info: Some(NdiscPrefixInformation {
                prefix_len: self.inner.state.params.prefix_len_ipv6,
                prefix,
                valid_lifetime: smoltcp::time::Duration::from_secs(86400),     // 24 hours
                preferred_lifetime: smoltcp::time::Duration::from_secs(14400), // 4 hours
                // AUTONOMOUS: clients can use SLAAC to generate addresses
                // ON_LINK: addresses with this prefix are on this link
                flags: NdiscPrefixInfoFlags::ON_LINK | NdiscPrefixInfoFlags::ADDRCONF,
            }),
        };

        // Build IPv6 header
        // RFC 4861 Section 4.2: Router Advertisements MUST have hop limit 255
        let ipv6_repr = Ipv6Repr {
            src_addr: link_local_src,
            dst_addr,
            next_header: IpProtocol::Icmpv6,
            payload_len: ndp_repr.buffer_len(),
            hop_limit: 255, // MUST be 255 per RFC 4861
        };

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
            &IpAddress::Ipv6(ipv6_repr.src_addr),
            &IpAddress::Ipv6(ipv6_repr.dst_addr),
        );

        let total_len = eth_repr.buffer_len() + ipv6_repr.buffer_len() + ndp_repr.buffer_len();

        let hex_frame = buffer[..total_len]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();
        tracing::info!(
            dst_addr = %dst_addr,
            prefix = %prefix,
            prefix_len = self.inner.state.params.prefix_len_ipv6,
            frame_len = total_len,
            frame = %hex_frame,
            "sending Router Advertisement with SLAAC prefix"
        );

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
        full_frame: &[u8],
    ) -> Result<(), DropReason> {
        let hex_frame = full_frame
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();
        tracing::info!(
            src_addr = %ipv6_src_addr,
            target_addr = %target_addr,
            frame_len = full_frame.len(),
            has_lladdr = source_lladdr.is_some(),
            frame = %hex_frame,
            "received Neighbor Solicitation"
        );

        // RFC 4862 Section 5.4.3: Handle Duplicate Address Detection (DAD)
        // If source is unspecified (::), this is DAD - we should NOT respond
        // to avoid interfering with the client's address configuration
        if ipv6_src_addr.is_unspecified() {
            tracing::info!(
                target_addr = %target_addr,
                "received DAD Neighbor Solicitation, silently ignoring per RFC 4862"
            );
            return Ok(());
        }

        // RFC 4861 Section 7.1.1: If source is unspecified, there must be no
        // source link-layer address option
        if ipv6_src_addr.is_unspecified() && source_lladdr.is_some() {
            tracing::warn!("invalid NS: source is :: but source link-layer address present");
            return Err(DropReason::Packet(smoltcp::Error::Malformed));
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
            tracing::info!("Neighbor Solicitation from unexpected MAC, ignoring");
            return Ok(());
        }

        // Compute our link-local address
        let our_link_local = self.compute_link_local_address(self.inner.state.params.gateway_mac_ipv6);

        // Only respond if the target is our link-local address
        // In a stateless NAT implementation, the gateway only responds for its own
        // link-local address, not for global addresses that clients autoconfigure
        if target_addr != our_link_local {
            tracing::debug!(
                target_addr = %target_addr,
                our_link_local = %our_link_local,
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
            hop_limit: 255, // RFC 4861: NDP messages MUST have hop limit 255
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
            &IpAddress::Ipv6(ipv6_repr.src_addr),
            &IpAddress::Ipv6(ipv6_repr.dst_addr),
        );

        let total_len = eth_repr.buffer_len() + ipv6_repr.buffer_len() + ndp_repr.buffer_len();

        let hex_frame = buffer[..total_len]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();
        tracing::info!(
            target_addr = %target_addr,
            dst_addr = %dst_addr,
            frame_len = total_len,
            solicited = solicited,
            frame = %hex_frame,
            "sending Neighbor Advertisement"
        );

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

        let addr_u128 = u128::from_be_bytes(addr.0);
        let mask = if prefix_len == 0 {
            0u128
        } else {
            (!0u128) << (128 - prefix_len)
        };

        Ipv6Address((addr_u128 & mask).to_be_bytes())
    }

    /// Compute a link-local IPv6 address from a MAC address
    ///
    /// RFC 4291 Section 2.5.6: Link-local addresses are formed by combining
    /// the link-local prefix (fe80::/64) with an interface identifier derived
    /// from the MAC address using the EUI-64 format.
    ///
    /// EUI-64 format (RFC 2464 Section 4):
    /// - Insert 0xFFFE in the middle of the 48-bit MAC address
    /// - Invert the universal/local bit (bit 6 of the first byte)
    fn compute_link_local_address(&self, mac: EthernetAddress) -> Ipv6Address {
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

        Ipv6Address(addr)
    }
}
