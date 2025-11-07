// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::str::FromStr;

use super::Access;
use super::Client;
use super::DropReason;
use crate::ChecksumState;
use crate::MIN_MTU;
use dhcproto::v6;
use dhcproto::Decodable;
use dhcproto::Encodable;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::EthernetRepr;
use smoltcp::wire::IpAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv6Address;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::Ipv6Repr;
use smoltcp::wire::UdpPacket;
use smoltcp::wire::UdpRepr;

pub const DHCPV6_CLIENT_PORT: u16 = 546;
pub const DHCPV6_SERVER_PORT: u16 = 547;

impl<T: Client> Access<'_, T> {
    pub(crate) fn handle_dhcpv6(&mut self, payload: &[u8], client_ip: Option<Ipv6Address>) -> Result<(), DropReason> {
        // Parse the DHCPv6 message
        let msg = v6::Message::decode(&mut dhcproto::Decoder::new(payload)).map_err(|x| {
            tracing::info!(error = &x as &dyn std::error::Error, "failed to decode DHCPv6 message");
            DropReason::Packet(smoltcp::Error::Malformed)
        })?;

        match msg.msg_type() {
            v6::MessageType::InformationRequest => {
                // Handle InformationRequest message (stateless DHCPv6)
                tracing::info!("Received DHCPv6 InformationRequest message");

                // Build DHCPv6 Reply response
                let mut reply = v6::Message::new(v6::MessageType::Reply);
                reply.set_xid(msg.xid());

                // Add Client Identifier option (echo back from the InformationRequest)
                if let Some(v6::DhcpOption::ClientId(client_id)) = msg.opts().get(v6::OptionCode::ClientId) {
                    reply.opts_mut().insert(v6::DhcpOption::ClientId(client_id.clone()));
                }

                // Add Server Identifier option
                // Use DUID-LL (type 3: Link-layer address)
                let gateway_mac = self.inner.state.params.gateway_mac_ipv6.0;
                let mut duid_bytes = vec![0x00, 0x03, 0x00, 0x01]; // Type 3 (LL), Hardware type 1 (Ethernet)
                duid_bytes.extend_from_slice(&gateway_mac);
                reply.opts_mut().insert(v6::DhcpOption::ServerId(duid_bytes));

                // Add DNS Recursive Name Server option if we have nameservers
                let dns_servers: Vec<std::net::Ipv6Addr> = self
                    .inner
                    .state
                    .params
                    .nameservers
                    .iter()
                    .filter_map(|ip| match ip {
                        IpAddress::Ipv6(addr) => Some(*addr),
                        _ => None,
                    })
                    .filter(|addr| !(addr.is_unspecified() || addr.is_loopback() || addr.is_link_local() || addr.is_multicast() || addr.0.starts_with(&[0xfc, 0x00]) || addr.0.starts_with(&[0xfe, 0xc0])))
                    .map(|addr| addr.into())
                    .collect();

                let mut dns_servers_len = 0;
                if !dns_servers.is_empty() {
                    dns_servers_len = dns_servers.len();
                    reply.opts_mut().insert(v6::DhcpOption::DomainNameServers(dns_servers));
                }

                let mut dhcpv6_buffer = Vec::new();
                let mut encoder = dhcproto::Encoder::new(&mut dhcpv6_buffer);
                reply.encode(&mut encoder).map_err(|x| {
                    tracing::error!(error = &x as &dyn std::error::Error, "failed to encode DHCPv6 message");
                    DropReason::Packet(smoltcp::Error::Malformed)
                })?;

                let resp_udp = UdpRepr {
                    src_port: DHCPV6_SERVER_PORT,
                    dst_port: DHCPV6_CLIENT_PORT,
                };

                let client_link_local = client_ip.unwrap_or_else(|| Ipv6Address::from_str("ff02::1:2").unwrap());
                let resp_ipv6 = Ipv6Repr {
                    src_addr: self.inner.state.params.gateway_ipv6,
                    dst_addr: client_link_local,
                    next_header: IpProtocol::Udp,
                    payload_len: resp_udp.header_len() + dhcpv6_buffer.len(),
                    hop_limit: 64,
                };
                let resp_eth = EthernetRepr {
                    src_addr: self.inner.state.params.gateway_mac_ipv6,
                    dst_addr: self.inner.state.params.client_mac,
                    ethertype: EthernetProtocol::Ipv6,
                };

                // Construct the complete packet
                let mut buffer = [0; MIN_MTU];
                let mut eth_frame = EthernetFrame::new_unchecked(&mut buffer);
                resp_eth.emit(&mut eth_frame);

                let mut ipv6_packet = Ipv6Packet::new_unchecked(eth_frame.payload_mut());
                resp_ipv6.emit(&mut ipv6_packet);

                let mut udp_packet = UdpPacket::new_unchecked(ipv6_packet.payload_mut());
                resp_udp.emit(
                    &mut udp_packet,
                    &IpAddress::Ipv6(resp_ipv6.src_addr),
                    &IpAddress::Ipv6(resp_ipv6.dst_addr),
                    dhcpv6_buffer.len(),
                    |udp_payload| {
                        udp_payload[..dhcpv6_buffer.len()].copy_from_slice(&dhcpv6_buffer);
                    },
                    &ChecksumCapabilities::default(),
                );

                let total_len = resp_eth.buffer_len()
                    + resp_ipv6.buffer_len()
                    + resp_udp.header_len()
                    + dhcpv6_buffer.len();

                tracing::info!(
                    dst_addr = %client_link_local,
                    packet_len = total_len,
                    dns_servers = dns_servers_len,
                    "sending DHCPv6 Reply to InformationRequest"
                );

                // Dump the packet to the log for debugging (in hex)
                let hex_packet = buffer[..total_len]
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>();
                tracing::info!(packet = %hex_packet, "sending DHCPv6 packet");

                self.client.recv(&buffer[..total_len], &ChecksumState::NONE);
            },
            _ => return Err(DropReason::UnsupportedDhcpv6(msg.msg_type())),
        }

        Ok(())
    }
}
