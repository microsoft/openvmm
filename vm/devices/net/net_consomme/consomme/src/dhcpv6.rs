// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Access;
use super::Client;
use super::DropReason;
use crate::ChecksumState;
use crate::MIN_MTU;
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
use std::collections::HashMap;

const DHCPV6_ALL_AGENTS_MULTICAST: Ipv6Address =
    Ipv6Address([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 2]);

// DHCPv6 ports
pub const DHCPV6_SERVER: u16 = 547;
pub const DHCPV6_CLIENT: u16 = 546;

/// DHCPv6 message types (RFC 8415)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    InformationRequest = 11,
    Reply = 7,
    Unknown(u8),
}

impl MessageType {
    fn from_u8(value: u8) -> Self {
        match value {
            11 => MessageType::InformationRequest,
            7 => MessageType::Reply,
            other => MessageType::Unknown(other),
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            MessageType::InformationRequest => 11,
            MessageType::Reply => 7,
            MessageType::Unknown(v) => v,
        }
    }
}

/// DHCPv6 option codes (RFC 8415)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
enum OptionCode {
    ClientId = 1,
    ServerId = 2,
    DnsServers = 23,
}

impl OptionCode {
    fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(OptionCode::ClientId),
            2 => Some(OptionCode::ServerId),
            23 => Some(OptionCode::DnsServers),
            _ => None,
        }
    }
}

/// DHCPv6 option
#[derive(Debug, Clone)]
enum DhcpOption {
    ClientId(Vec<u8>),
    ServerId(Vec<u8>),
    DnsServers(Vec<std::net::Ipv6Addr>),
}

/// DHCPv6 message
struct Message {
    msg_type: MessageType,
    transaction_id: [u8; 3],
    options: HashMap<OptionCode, DhcpOption>,
}

impl Message {
    fn new(msg_type: MessageType) -> Self {
        Self {
            msg_type,
            transaction_id: [0; 3],
            options: HashMap::new(),
        }
    }

    fn decode(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() < 4 {
            return Err("DHCPv6 message too short");
        }

        let msg_type = MessageType::from_u8(data[0]);
        let transaction_id = [data[1], data[2], data[3]];

        let mut options = HashMap::new();
        let mut offset = 4;

        // Parse options
        while offset + 4 <= data.len() {
            let option_code = u16::from_be_bytes([data[offset], data[offset + 1]]);
            let option_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            offset += 4;

            if offset + option_len > data.len() {
                return Err("Invalid option length");
            }

            let option_data = &data[offset..offset + option_len];
            offset += option_len;

            if let Some(code) = OptionCode::from_u16(option_code) {
                match code {
                    OptionCode::ClientId => {
                        options.insert(code, DhcpOption::ClientId(option_data.to_vec()));
                    }
                    OptionCode::ServerId => {
                        options.insert(code, DhcpOption::ServerId(option_data.to_vec()));
                    }
                    OptionCode::DnsServers => {
                        // DNS servers option contains a list of IPv6 addresses (16 bytes each)
                        if option_len % 16 != 0 {
                            return Err("Invalid DNS servers option length");
                        }
                        let mut dns_servers = Vec::new();
                        for i in (0..option_len).step_by(16) {
                            let mut addr_bytes = [0u8; 16];
                            addr_bytes.copy_from_slice(&option_data[i..i + 16]);
                            dns_servers.push(std::net::Ipv6Addr::from(addr_bytes));
                        }
                        options.insert(code, DhcpOption::DnsServers(dns_servers));
                    }
                }
            }
            // Skip unknown options
        }

        Ok(Self {
            msg_type,
            transaction_id,
            options,
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut buffer = Vec::new();

        // Message type (1 byte) + transaction ID (3 bytes)
        buffer.push(self.msg_type.to_u8());
        buffer.extend_from_slice(&self.transaction_id);

        // Encode options
        for (code, option) in &self.options {
            let code_bytes = (*code as u16).to_be_bytes();
            buffer.extend_from_slice(&code_bytes);

            match option {
                DhcpOption::ClientId(data) | DhcpOption::ServerId(data) => {
                    let len_bytes = (data.len() as u16).to_be_bytes();
                    buffer.extend_from_slice(&len_bytes);
                    buffer.extend_from_slice(data);
                }
                DhcpOption::DnsServers(servers) => {
                    let len = (servers.len() * 16) as u16;
                    let len_bytes = len.to_be_bytes();
                    buffer.extend_from_slice(&len_bytes);
                    for server in servers {
                        buffer.extend_from_slice(&server.octets());
                    }
                }
            }
        }

        buffer
    }

    fn set_transaction_id(&mut self, xid: [u8; 3]) {
        self.transaction_id = xid;
    }

    fn insert_option(&mut self, option: DhcpOption) {
        let code = match &option {
            DhcpOption::ClientId(_) => OptionCode::ClientId,
            DhcpOption::ServerId(_) => OptionCode::ServerId,
            DhcpOption::DnsServers(_) => OptionCode::DnsServers,
        };
        self.options.insert(code, option);
    }

    fn get_option(&self, code: OptionCode) -> Option<&DhcpOption> {
        self.options.get(&code)
    }
}

impl<T: Client> Access<'_, T> {
    pub(crate) fn handle_dhcpv6(
        &mut self,
        payload: &[u8],
        client_ip: Option<Ipv6Address>,
    ) -> Result<(), DropReason> {
        // Parse the DHCPv6 message
        let msg = Message::decode(payload).map_err(|e| {
            tracing::info!(error = e, "failed to decode DHCPv6 message");
            DropReason::Packet(smoltcp::wire::Error)
        })?;

        match msg.msg_type {
            MessageType::InformationRequest => {
                // Build DHCPv6 Reply response
                let mut reply = Message::new(MessageType::Reply);
                reply.set_transaction_id(msg.transaction_id);

                // Add Client Identifier option (echo back from the InformationRequest)
                if let Some(DhcpOption::ClientId(client_id)) = msg.get_option(OptionCode::ClientId)
                {
                    reply.insert_option(DhcpOption::ClientId(client_id.clone()));
                }

                // Add Server Identifier option
                // Use DUID-LL (type 3: Link-layer address)
                let gateway_mac = self.inner.state.params.gateway_mac_ipv6.0;
                let mut duid_bytes = vec![0x00, 0x03, 0x00, 0x01]; // Type 3 (LL), Hardware type 1 (Ethernet)
                duid_bytes.extend_from_slice(&gateway_mac);
                reply.insert_option(DhcpOption::ServerId(duid_bytes));

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
                    .filter(|addr| {
                        !(addr.is_unspecified()
                            || addr.is_loopback()
                            || addr.is_link_local()
                            || addr.is_multicast()
                            || addr.0.starts_with(&[0xfc, 0x00])
                            || addr.0.starts_with(&[0xfe, 0xc0]))
                    })
                    .map(|addr| addr.into())
                    .collect();

                if !dns_servers.is_empty() {
                    reply.insert_option(DhcpOption::DnsServers(dns_servers));
                }

                let dhcpv6_buffer = reply.encode();

                let resp_udp = UdpRepr {
                    src_port: DHCPV6_SERVER,
                    dst_port: DHCPV6_CLIENT,
                };

                let client_link_local = client_ip.unwrap_or(DHCPV6_ALL_AGENTS_MULTICAST);
                let resp_ipv6 = Ipv6Repr {
                    src_addr: self.inner.state.params.gateway_link_local_ipv6,
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

                self.client.recv(&buffer[..total_len], &ChecksumState::NONE);
            }
            _ => return Err(DropReason::UnsupportedDhcpv6(msg.msg_type.into())),
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper function to convert a hex string to bytes
    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Helper function to convert bytes to hex string
    fn bytes_to_hex(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join("")
    }

    /// Helper function to create IPv6 address from hex string
    fn hex_to_ipv6(hex: &str) -> std::net::Ipv6Addr {
        std::net::Ipv6Addr::from(<[u8; 16]>::try_from(hex_to_bytes(hex).as_slice()).unwrap())
    }

    #[test]
    fn test_message_decode() {
        // This is a sample DHCPv6 InformationRequest message that was captured from a VM.
        let input_hex = "0b1c57ca0008000200000001000e0001000130adec9800155d300e150010000e0000013700084d53465420352e30000600080011001700180020";
        let input_bytes = hex_to_bytes(input_hex);
        let msg = Message::decode(&input_bytes).expect("Failed to decode message");

        assert_eq!(msg.msg_type, MessageType::InformationRequest);
        assert_eq!(msg.transaction_id, [0x1c, 0x57, 0xca]);
        let client_id = "0001000130adec9800155d300e15";
        if let Some(DhcpOption::ClientId(data)) = msg.get_option(OptionCode::ClientId) {
            assert_eq!(bytes_to_hex(data), client_id);
        } else {
            panic!("ClientId option not found");
        }
    }

    #[test]
    fn test_message_encode() {
        const CLIENT_ID_HEX: &str = "0001000130adec9800155d300e15";
        const SERVER_ID_HEX: &str = "0003000152550a000102";
        const DNS1_HEX: &str = "20014898000000000000000010501050";
        const DNS2_HEX: &str = "20014898000000000000000010505050";
        const TRANSACTION_ID: [u8; 3] = [0x1c, 0x57, 0xca];

        // Create a message with all option types
        let mut msg = Message::new(MessageType::Reply);
        msg.set_transaction_id(TRANSACTION_ID);
        msg.insert_option(DhcpOption::ClientId(hex_to_bytes(CLIENT_ID_HEX)));
        msg.insert_option(DhcpOption::ServerId(hex_to_bytes(SERVER_ID_HEX)));

        let dns_servers = vec![hex_to_ipv6(DNS1_HEX), hex_to_ipv6(DNS2_HEX)];
        msg.insert_option(DhcpOption::DnsServers(dns_servers.clone()));

        // Encode and decode to verify round-trip
        let decoded = Message::decode(&msg.encode()).expect("Failed to decode encoded message");

        assert_eq!(decoded.msg_type, MessageType::Reply);
        assert_eq!(decoded.transaction_id, TRANSACTION_ID);

        let DhcpOption::ClientId(data) = decoded
            .get_option(OptionCode::ClientId)
            .expect("ClientId not found")
        else {
            panic!("Wrong option type for ClientId");
        };
        assert_eq!(bytes_to_hex(data), CLIENT_ID_HEX);

        let DhcpOption::ServerId(data) = decoded
            .get_option(OptionCode::ServerId)
            .expect("ServerId not found")
        else {
            panic!("Wrong option type for ServerId");
        };
        assert_eq!(bytes_to_hex(data), SERVER_ID_HEX);

        let DhcpOption::DnsServers(servers) = decoded
            .get_option(OptionCode::DnsServers)
            .expect("DnsServers not found")
        else {
            panic!("Wrong option type for DnsServers");
        };
        assert_eq!(servers, &dns_servers);
    }
}
