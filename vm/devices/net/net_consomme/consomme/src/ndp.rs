// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
use smoltcp::wire::NdiscRepr;
use smoltcp::wire::RawHardwareAddress;

#[derive(Debug)]
pub enum NdpMessageType {
    RouterSolicit,
    RouterAdvert,
    NeighborSolicit,
    NeighborAdvert,
    Redirect,
}

impl<T: Client> Access<'_, T> {
    pub(crate) fn handle_ndp(
        &mut self,
        frame: &EthernetRepr,
        payload: &[u8],
        client_ip: Ipv6Address
    ) -> Result<(), DropReason> {
        let ndp = NdiscRepr::parse(&Icmpv6Packet::new_unchecked(payload))?;
    
        match ndp {
            NdiscRepr::NeighborSolicit {
                target_addr,
                lladdr: source_lladdr,
            } => {
                // First NS message has a tentative local link address
                
                tracing::info!(
                    target_addr = %target_addr,
                    "received NDP Neighbor Solicitation"
                );

                if source_lladdr.is_none() {
                    // If the source link layer address option is missing, then this message is being used for
                    // duplicate address detection. In this case, we do not respond.
                    tracing::info!("received NDP Neighbor Solicitation without source link layer address");
                    return Ok(())
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
                    // Not from our client, ignore
                    return Ok(());
                }

                // Determine the source IPv6 address from the request
                let reply_dst_addr = source_lladdr
                    .and_then(|addr| {
                        addr.parse(Medium::Ethernet)
                            .ok()
                            .and_then(|hw_addr| match hw_addr {
                                HardwareAddress::Ethernet(eth_addr) => {
                                    // Use link-local address derived from source MAC
                                    Some(ipv6_link_local_from_mac(eth_addr))
                                }
                                #[allow(unreachable_patterns)]
                                _ => None,
                            })
                    })
                    .unwrap_or_else(|| {
                        // Fallback to solicited-node multicast address
                        ipv6_solicited_node_multicast(target_addr)
                    });

                // Build NDP Neighbor Advertisement
                let ndp_repr = NdiscRepr::NeighborAdvert {
                    flags: NdiscNeighborFlags::SOLICITED | NdiscNeighborFlags::OVERRIDE,
                    target_addr: self.inner.state.params.client_ip_ipv6,
                    lladdr: Some(RawHardwareAddress::from(
                        self.inner.state.params.gateway_mac_ipv6,
                    )),
                };

                // Build IPv6 header
                let ipv6_repr = Ipv6Repr {
                    src_addr: self.inner.state.params.gateway_ip_ipv6,
                    dst_addr: reply_dst_addr,
                    next_header: IpProtocol::Icmpv6,
                    payload_len: ndp_repr.buffer_len(),
                    hop_limit: 255,
                };

                // Build Ethernet header
                let eth_repr = EthernetRepr {
                    src_addr: self.inner.state.params.gateway_mac_ipv6,
                    dst_addr: frame.src_addr,
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

                let total_len = eth_repr.buffer_len()
                    + ipv6_repr.buffer_len()
                    + ndp_repr.buffer_len();

                tracing::info!(
                    target_addr = %target_addr,
                    dst_addr = %reply_dst_addr,
                    packet_len = total_len,
                    "sending NDP Neighbor Advertisement"
                );

                // Dump the packet to the log for debugging (in hex)
                let hex_packet = buffer[..total_len]
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>();
                tracing::info!(packet = %hex_packet, "sending NDP packet");

                self.client.recv(&buffer[..total_len], &ChecksumState::NONE);
            }

            NdiscRepr::RouterSolicit { lladdr } => {
                // Verify this is from the expected client MAC
                let client_mac_matches = lladdr
                    .and_then(|addr| addr.parse(Medium::Ethernet).ok())
                    .map(|hw_addr| match hw_addr {
                        HardwareAddress::Ethernet(eth_addr) => {
                            eth_addr == self.inner.state.params.client_mac
                        }
                        #[allow(unreachable_patterns)]
                        _ => false,
                    })
                    .unwrap_or(true); // If no lladdr provided, allow it anyway

                if !client_mac_matches {
                    tracing::info!(lladdr = ?lladdr, "NDP Router Solicitation from unexpected MAC, ignoring");
                    return Err(DropReason::UnsupportedNdp(NdpMessageType::RouterSolicit));
                }

                tracing::info!(
                    client_mac = %self.inner.state.params.client_mac,
                    "received NDP Router Solicitation, sending Router Advertisement"
                );

                let reply_dst_addr = if lladdr.is_some() {
                    client_ip
                } else {
                    // Fallback to all-nodes multicast (ff02::1)
                    Ipv6Address([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
                };

                // Build NDP Router Advertisement
                // Set Managed flag to indicate DHCPv6 should be used
                // Clear Other and don't include prefix info to disable SLAAC
                let ndp_repr = NdiscRepr::RouterAdvert {
                    hop_limit: 64,             // Standard hop limit
                    flags: smoltcp::wire::NdiscRouterFlags::MANAGED | smoltcp::wire::NdiscRouterFlags::OTHER, // Tell guest to use DHCPv6
                    router_lifetime: smoltcp::time::Duration::from_secs(1800), // Router lifetime
                    reachable_time: smoltcp::time::Duration::from_millis(0),   // No specific reachable time
                    retrans_time: smoltcp::time::Duration::from_millis(0),     // No specific retransmit time
                    lladdr: Some(RawHardwareAddress::from(
                        self.inner.state.params.gateway_mac_ipv6,
                    )),
                    mtu: None,                 // No MTU option
                    prefix_info: None,         // No prefix info to disable SLAAC
                };

                // Build IPv6 header
                let ipv6_repr = Ipv6Repr {
                    src_addr: self.inner.state.params.gateway_ip_ipv6,
                    dst_addr: reply_dst_addr,
                    next_header: IpProtocol::Icmpv6,
                    payload_len: ndp_repr.buffer_len(),
                    hop_limit: 255,
                };

                // Build Ethernet header
                let eth_repr = EthernetRepr {
                    src_addr: self.inner.state.params.gateway_mac_ipv6,
                    dst_addr: frame.src_addr,
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

                let total_len = eth_repr.buffer_len()
                    + ipv6_repr.buffer_len()
                    + ndp_repr.buffer_len();

                tracing::info!(
                    dst_addr = %reply_dst_addr,
                    packet_len = total_len,
                    "sending NDP Router Advertisement with MANAGED flag"
                );

                // Dump the packet to the log for debugging (in hex)
                let hex_packet = buffer[..total_len]
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>();
                tracing::info!(packet = %hex_packet, "sending NDP packet");

                self.client.recv(&buffer[..total_len], &ChecksumState::NONE);
            }

            NdiscRepr::NeighborAdvert { .. } => return Err(DropReason::UnsupportedNdp(NdpMessageType::NeighborAdvert)),
            NdiscRepr::Redirect { .. } => return Err(DropReason::UnsupportedNdp(NdpMessageType::Redirect)),
            NdiscRepr::RouterAdvert { .. } => return Err(DropReason::UnsupportedNdp(NdpMessageType::RouterAdvert)),
        };
        Ok(())
    }
}

/// Derive an IPv6 link-local address from a MAC address
fn ipv6_link_local_from_mac(mac: EthernetAddress) -> Ipv6Address {
    let mac_bytes = mac.0;
    Ipv6Address([
        0xfe, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        mac_bytes[0] ^ 0x02, // Flip the universal/local bit
        mac_bytes[1],
        mac_bytes[2],
        0xff,
        0xfe,
        mac_bytes[3],
        mac_bytes[4],
        mac_bytes[5],
    ])
}

/// Generate an IPv6 solicited-node multicast address from a target address
fn ipv6_solicited_node_multicast(target: Ipv6Address) -> Ipv6Address {
    let target_bytes = target.0;
    Ipv6Address([
        0xff, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0xff,
        target_bytes[13],
        target_bytes[14],
        target_bytes[15],
    ])
}
