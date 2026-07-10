// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Access;
use super::Client;
use super::DropReason;
use crate::ChecksumState;
use crate::MIN_MTU;
use heapless::Vec as HeaplessVec;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::DHCP_MAX_DNS_SERVER_COUNT;
use smoltcp::wire::DhcpMessageType;
use smoltcp::wire::DhcpPacket;
use smoltcp::wire::DhcpRepr;
use smoltcp::wire::EthernetAddress;
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
// RFC 1542 section 2.1 requires every BOOTP message to be at least 300 octets.
const BOOTP_MIN_MESSAGE_LEN: usize = 300;

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

        let mut dns_servers: HeaplessVec<Ipv4Address, DHCP_MAX_DNS_SERVER_COUNT> =
            HeaplessVec::new();
        dns_servers.extend(
            self.inner
                .state
                .params
                .nameservers
                .iter()
                .filter_map(|ip| match ip {
                    IpAddress::Ipv4(addr) => Some(*addr),
                    _ => None,
                })
                .take(DHCP_MAX_DNS_SERVER_COUNT),
        );

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
                broadcast: dhcp_req.broadcast,
                requested_ip: None,
                client_identifier: None,
                server_identifier: Some(self.inner.state.params.gateway_ip),
                parameter_request_list: None,
                dns_servers: Some(dns_servers),
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
                your_ip: Ipv4Address::UNSPECIFIED,
                server_ip: Ipv4Address::UNSPECIFIED,
                router: None,
                subnet_mask: None,
                relay_agent_ip: Ipv4Address::UNSPECIFIED,
                broadcast: dhcp_req.broadcast,
                requested_ip: None,
                client_identifier: None,
                server_identifier: Some(self.inner.state.params.gateway_ip),
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
        let resp_dhcp_len = resp_dhcp.buffer_len().max(BOOTP_MIN_MESSAGE_LEN);
        // RFC 2131 section 4.1 requires consistent link- and network-layer
        // destinations. A DHCPNAK is always broadcast when no relay is involved.
        let (dst_hardware_address, dst_ip) = match (dhcp_req.broadcast, your_ip) {
            (true, _) | (_, None) => (EthernetAddress::BROADCAST, Ipv4Address::BROADCAST),
            (false, Some(ip)) => (dhcp_req.client_hardware_address, ip),
        };
        let resp_ipv4 = Ipv4Repr {
            src_addr: self.inner.state.params.gateway_ip,
            dst_addr: dst_ip,
            next_header: IpProtocol::Udp,
            payload_len: resp_udp.header_len() + resp_dhcp_len,
            hop_limit: 64,
        };
        let resp_eth = EthernetRepr {
            src_addr: self.inner.state.params.gateway_mac,
            dst_addr: dst_hardware_address,
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
            resp_dhcp_len,
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
                + resp_dhcp_len],
            &ChecksumState::UDP4,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Consomme;
    use crate::ConsommeParams;
    use pal_async::DefaultDriver;
    use pal_async::driver::Driver;

    const CLIENT_MAC: EthernetAddress = EthernetAddress([0x00, 0x15, 0x5d, 0x12, 0x34, 0x56]);
    const CLIENT_IP: Ipv4Address = Ipv4Address::new(10, 0, 0, 2);
    const GATEWAY_IP: Ipv4Address = Ipv4Address::new(10, 0, 0, 1);

    struct CaptureClient {
        driver: DefaultDriver,
        frames: Vec<(Vec<u8>, ChecksumState)>,
    }

    impl Client for CaptureClient {
        fn driver(&self) -> &dyn Driver {
            &self.driver
        }

        fn recv(&mut self, data: &[u8], checksum: &ChecksumState) {
            self.frames.push((data.to_vec(), *checksum));
        }

        fn rx_mtu(&mut self) -> usize {
            1514
        }
    }

    fn request(
        message_type: DhcpMessageType,
        broadcast: bool,
        requested_ip: Option<Ipv4Address>,
    ) -> Vec<u8> {
        let request = DhcpRepr {
            message_type,
            transaction_id: 0x1234_5678,
            secs: 0,
            client_hardware_address: CLIENT_MAC,
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip: Ipv4Address::UNSPECIFIED,
            server_ip: Ipv4Address::UNSPECIFIED,
            router: None,
            subnet_mask: None,
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast,
            requested_ip,
            client_identifier: None,
            server_identifier: None,
            parameter_request_list: None,
            dns_servers: None,
            max_size: None,
            lease_duration: None,
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let mut payload = vec![0; request.buffer_len()];
        request
            .emit(&mut DhcpPacket::new_unchecked(&mut payload))
            .unwrap();
        payload
    }

    struct Reply {
        message_type: DhcpMessageType,
        ethernet_dst: EthernetAddress,
        ip_dst: Ipv4Address,
        your_ip: Ipv4Address,
        server_ip: Ipv4Address,
        server_identifier: Option<Ipv4Address>,
        broadcast: bool,
        dhcp_len: usize,
        checksum_state: ChecksumState,
    }

    fn reply(
        driver: DefaultDriver,
        message_type: DhcpMessageType,
        broadcast: bool,
        requested_ip: Option<Ipv4Address>,
    ) -> Reply {
        let mut params = ConsommeParams::new().unwrap();
        params.client_mac = CLIENT_MAC;
        let mut consomme = Consomme::new(params);
        let mut client = CaptureClient {
            driver,
            frames: Vec::new(),
        };
        consomme
            .access(&mut client)
            .handle_dhcp(&request(message_type, broadcast, requested_ip))
            .unwrap();

        let (frame, checksum_state) = client.frames.last().expect("DHCP reply");
        let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
        let ethernet_repr = EthernetRepr::parse(&ethernet).unwrap();
        let ipv4 = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        let ipv4_repr = Ipv4Repr::parse(&ipv4, &ChecksumCapabilities::default()).unwrap();
        let udp = UdpPacket::new_checked(ipv4.payload()).unwrap();
        let dhcp = DhcpPacket::new_checked(udp.payload()).unwrap();
        let dhcp_repr = DhcpRepr::parse(&dhcp).unwrap();

        Reply {
            message_type: dhcp_repr.message_type,
            ethernet_dst: ethernet_repr.dst_addr,
            ip_dst: ipv4_repr.dst_addr,
            your_ip: dhcp_repr.your_ip,
            server_ip: dhcp_repr.server_ip,
            server_identifier: dhcp_repr.server_identifier,
            broadcast: dhcp_repr.broadcast,
            dhcp_len: udp.payload().len(),
            checksum_state: *checksum_state,
        }
    }

    fn assert_udp4(checksum_state: ChecksumState) {
        assert!(checksum_state.ipv4);
        assert!(checksum_state.udp);
        assert!(!checksum_state.tcp);
        assert_eq!(checksum_state.tso, None);
        assert_eq!(checksum_state.gso, None);
    }

    #[pal_async::async_test]
    async fn broadcasts_offer_when_requested(driver: DefaultDriver) {
        let reply = reply(driver, DhcpMessageType::Discover, true, None);

        assert_eq!(reply.message_type, DhcpMessageType::Offer);
        assert_eq!(reply.ethernet_dst, EthernetAddress::BROADCAST);
        assert_eq!(reply.ip_dst, Ipv4Address::BROADCAST);
        assert!(reply.broadcast);
        assert_udp4(reply.checksum_state);
    }

    #[pal_async::async_test]
    async fn pads_bootp_reply_to_minimum_length(driver: DefaultDriver) {
        let reply = reply(driver, DhcpMessageType::Discover, false, None);

        assert_eq!(reply.dhcp_len, BOOTP_MIN_MESSAGE_LEN);
    }

    #[pal_async::async_test]
    async fn unicasts_offer_when_supported(driver: DefaultDriver) {
        let reply = reply(driver, DhcpMessageType::Discover, false, None);

        assert_eq!(reply.message_type, DhcpMessageType::Offer);
        assert_eq!(reply.ethernet_dst, CLIENT_MAC);
        assert_eq!(reply.ip_dst, CLIENT_IP);
        assert!(!reply.broadcast);
        assert_udp4(reply.checksum_state);
    }

    #[pal_async::async_test]
    async fn broadcasts_nak_with_required_fields(driver: DefaultDriver) {
        let reply = reply(
            driver,
            DhcpMessageType::Request,
            false,
            Some(Ipv4Address::new(10, 0, 0, 99)),
        );

        assert_eq!(reply.message_type, DhcpMessageType::Nak);
        assert_eq!(reply.ethernet_dst, EthernetAddress::BROADCAST);
        assert_eq!(reply.ip_dst, Ipv4Address::BROADCAST);
        assert_eq!(reply.your_ip, Ipv4Address::UNSPECIFIED);
        assert_eq!(reply.server_ip, Ipv4Address::UNSPECIFIED);
        assert_eq!(reply.server_identifier, Some(GATEWAY_IP));
        assert!(!reply.broadcast);
        assert_udp4(reply.checksum_state);
    }
}
