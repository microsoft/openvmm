// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Access;
use super::Client;
use super::DropReason;
use crate::ChecksumState;
use crate::MIN_MTU;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::DHCP_MAX_DNS_SERVER_COUNT;
use smoltcp::wire::DhcpMessageType;
use smoltcp::wire::DhcpPacket;
use smoltcp::wire::DhcpRepr;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::EthernetRepr;
use smoltcp::wire::IpAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::UdpPacket;
use smoltcp::wire::UdpRepr;

pub const DHCP_SERVER: u16 = 67;
pub const DHCP_CLIENT: u16 = 68;

impl<T: Client> Access<'_, T> {
    pub(crate) fn handle_dhcp(&mut self, payload: &[u8]) -> Result<(), DropReason> {
        let dhcp_packet = DhcpPacket::new_checked(payload)?;
        let dhcp_req = DhcpRepr::parse(&dhcp_packet)?;
        let your_ip;
        let message_type;
        match dhcp_req.message_type {
            DhcpMessageType::Discover => {
                your_ip = Some(self.inner.state.params.client_ip);
                message_type = DhcpMessageType::Offer;
            }
            DhcpMessageType::Request => {
                your_ip = match dhcp_req.requested_ip {
                    Some(addr) if addr == self.inner.state.params.client_ip => Some(addr),
                    None => Some(self.inner.state.params.client_ip),
                    Some(_) => None,
                };
                message_type = DhcpMessageType::Ack;
            }
            ty => return Err(DropReason::UnsupportedDhcp(ty)),
        }

        let dns_servers = if self.inner.state.params.nameservers.is_empty() {
            None
        } else {
            let mut dns_servers = heapless::Vec::new();
            for &s in self
                .inner
                .state
                .params
                .nameservers
                .iter()
                .take(DHCP_MAX_DNS_SERVER_COUNT)
            {
                let _ = dns_servers.push(s);
            }
            Some(dns_servers)
        };

        let resp_dhcp = if let Some(your_ip) = your_ip {
            DhcpRepr {
                message_type,
                transaction_id: dhcp_req.transaction_id,
                secs: 0,
                client_hardware_address: dhcp_req.client_hardware_address,
                client_ip: Ipv4Address::UNSPECIFIED,
                your_ip,
                server_ip: self.inner.state.params.gateway_ip,
                router: Some(self.inner.state.params.gateway_ip),
                subnet_mask: Some(self.inner.state.params.net_mask),
                relay_agent_ip: Ipv4Address::UNSPECIFIED,
                broadcast: false,
                requested_ip: None,
                client_identifier: None,
                server_identifier: Some(self.inner.state.params.gateway_ip),
                parameter_request_list: None,
                dns_servers,
                max_size: None,
                lease_duration: Some(86400),
                renew_duration: None,
                rebind_duration: None,
                additional_options: &[],
            }
        } else {
            DhcpRepr {
                message_type: DhcpMessageType::Nak,
                transaction_id: dhcp_req.transaction_id,
                secs: 0,
                client_hardware_address: dhcp_req.client_hardware_address,
                client_ip: Ipv4Address::UNSPECIFIED,
                your_ip: Ipv4Address::BROADCAST,
                server_ip: self.inner.state.params.gateway_ip,
                router: None,
                subnet_mask: None,
                relay_agent_ip: Ipv4Address::UNSPECIFIED,
                broadcast: false,
                requested_ip: None,
                client_identifier: None,
                server_identifier: None,
                parameter_request_list: None,
                dns_servers: None,
                max_size: None,
                lease_duration: None,
                renew_duration: None,
                rebind_duration: None,
                additional_options: &[],
            }
        };

        let resp_udp = UdpRepr {
            src_port: DHCP_SERVER,
            dst_port: DHCP_CLIENT,
        };
        let resp_ipv4 = Ipv4Repr {
            src_addr: self.inner.state.params.gateway_ip,
            dst_addr: Ipv4Address::BROADCAST,
            next_header: IpProtocol::Udp,
            payload_len: resp_udp.header_len() + resp_dhcp.buffer_len(),
            hop_limit: 64,
        };
        let resp_eth = EthernetRepr {
            src_addr: self.inner.state.params.gateway_mac,
            dst_addr: dhcp_req.client_hardware_address,
            ethertype: EthernetProtocol::Ipv4,
        };

        let mut resp_buffer = [0; MIN_MTU];
        let mut resp_eth_packet = EthernetFrame::new_unchecked(&mut resp_buffer);
        resp_eth.emit(&mut resp_eth_packet);
        let mut resp_ipv4_packet = Ipv4Packet::new_unchecked(resp_eth_packet.payload_mut());
        resp_ipv4.emit(&mut resp_ipv4_packet, &ChecksumCapabilities::default());
        let mut resp_udp_packet = UdpPacket::new_unchecked(resp_ipv4_packet.payload_mut());
        resp_udp.emit(
            &mut resp_udp_packet,
            &IpAddress::Ipv4(resp_ipv4.src_addr),
            &IpAddress::Ipv4(resp_ipv4.dst_addr),
            resp_dhcp.buffer_len(),
            |udp_payload| {
                let mut resp_dhcp_packet = DhcpPacket::new_unchecked(udp_payload);
                resp_dhcp.emit(&mut resp_dhcp_packet).unwrap();
            },
            &ChecksumCapabilities::default(),
        );

        self.client.recv(
            &resp_buffer[..resp_eth.buffer_len()
                + resp_ipv4.buffer_len()
                + resp_udp.header_len()
                + resp_dhcp.buffer_len()],
            &ChecksumState::IPV4_ONLY,
        );
        Ok(())
    }
}
