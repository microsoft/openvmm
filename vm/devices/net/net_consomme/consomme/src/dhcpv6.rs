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
use std::mem::size_of;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Ref;
use zerocopy::big_endian::U16;

const DHCPV6_ALL_AGENTS_MULTICAST: Ipv6Address =
    Ipv6Address::from_octets([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 2]);

// DHCPv6 ports
pub const DHCPV6_SERVER: u16 = 547;
pub const DHCPV6_CLIENT: u16 = 546;


open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    /// DHCPv6 message types (RFC 8415)
    pub enum MessageType: u8 {
        INFORMATION_REQUEST = 11,
        REPLY = 7,
    }
}


open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    /// DHCPv6 option codes (RFC 8415)
    pub enum OptionCode: u16 {
        CLIENT_ID = 1,
        SERVER_ID = 2,
        DNS_SERVERS = 23,
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

#[derive(Debug, Error)]
enum DhcpV6Error {
    #[error("message too short: {0:#x}")]
    MessageTooShort(usize),
    #[error("malformed option at offset {0:#x}")]
    MalformedOption(usize),
    #[error("invalid DNS Server option length {0:#x}")]
    InvalidDnsServerOption(usize),
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
struct DhcpV6Header {
    msg_type: u8,
    transaction_id: [u8; 3],
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
struct DhcpV6Option {
    code: U16,
    len: U16,
}

impl Message {
    fn new(msg_type: MessageType) -> Self {
        Self {
            msg_type,
            transaction_id: [0; 3],
            options: HashMap::new(),
        }
    }

    fn decode(message_bytes: &[u8]) -> Result<Self, DhcpV6Error> {
        let (header, mut unparsed_bytes) = Ref::<_, DhcpV6Header>::from_prefix(message_bytes)
            .map_err(|_| DhcpV6Error::MessageTooShort(message_bytes.len()))?;

        let msg_type = MessageType(header.msg_type);
        let transaction_id = header.transaction_id;

        let mut options = HashMap::new();

        while unparsed_bytes.len() >= size_of::<DhcpV6Option>() {
            let option_offset = message_bytes.len() - unparsed_bytes.len();
            let (option_header, after_option_header) = Ref::<_, DhcpV6Option>::from_prefix(unparsed_bytes)
                .map_err(|_| DhcpV6Error::MalformedOption(option_offset))?;

            let option_code = option_header.code.get();
            let option_len = option_header.len.get() as usize;

            if option_len > after_option_header.len() {
                return Err(DhcpV6Error::MalformedOption(message_bytes.len() - after_option_header.len()));
            }

            let option_value = &after_option_header[..option_len];
            unparsed_bytes = &after_option_header[option_len..];

            let code = OptionCode(option_code);
            match code {
                OptionCode::CLIENT_ID => {
                    options.insert(code, DhcpOption::ClientId(option_value.to_vec()));
                }
                OptionCode::SERVER_ID => {
                    options.insert(code, DhcpOption::ServerId(option_value.to_vec()));
                }
                OptionCode::DNS_SERVERS => {
                        // DNS servers option contains a list of IPv6 addresses (16 bytes each)
                        if !option_len.is_multiple_of(16) {
                            return Err(DhcpV6Error::InvalidDnsServerOption(option_len));
                        }
                        let mut dns_servers = Vec::new();
                        for i in (0..option_len).step_by(16) {
                            let mut addr_bytes = [0u8; 16];
                            addr_bytes.copy_from_slice(&option_value[i..i + 16]);
                            dns_servers.push(std::net::Ipv6Addr::from(addr_bytes));
                        }
                    options.insert(code, DhcpOption::DnsServers(dns_servers));
                }
                _ => {
                    // Skip unknown options
                }
            }
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
        buffer.push(self.msg_type.0);
        buffer.extend_from_slice(&self.transaction_id);

        // Encode options
        for (code, option) in &self.options {
            let code_bytes = code.0.to_be_bytes();
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
            DhcpOption::ClientId(_) => OptionCode::CLIENT_ID,
            DhcpOption::ServerId(_) => OptionCode::SERVER_ID,
            DhcpOption::DnsServers(_) => OptionCode::DNS_SERVERS,
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
            tracing::info!(error = %e, "failed to decode DHCPv6 message");
            DropReason::MalformedPacket
        })?;

        match msg.msg_type {
            MessageType::INFORMATION_REQUEST => {
                // Build DHCPv6 Reply response
                let mut reply = Message::new(MessageType::REPLY);
                reply.set_transaction_id(msg.transaction_id);

                // Add Client Identifier option (echo back from the InformationRequest)
                if let Some(DhcpOption::ClientId(client_id)) = msg.get_option(OptionCode::CLIENT_ID)
                {
                    reply.insert_option(DhcpOption::ClientId(client_id.clone()));
                }

                // Add Server Identifier option
                // Use DUID-LL (type 3: Link-layer address)
                let gateway_mac = self.inner.state.params.gateway_mac_ipv6.0;
                let mut duid_bytes = vec![0x00, 0x03, 0x00, 0x01]; // Type 3 (LL), Hardware type 1 (Ethernet)
                duid_bytes.extend_from_slice(&gateway_mac);
                reply.insert_option(DhcpOption::ServerId(duid_bytes));

                // Add DNS Name Server option if we have nameservers
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
                        let octets = addr.octets();
                        !(addr.is_unspecified()
                            || addr.is_loopback()
                            || addr.is_unicast_link_local()
                            || addr.is_multicast()
                            || matches!(octets[0], 0xfc | 0xfd) // Is unique local address
                            || octets.starts_with(&[0xfe, 0xc0])) // Is synthetic DNS server
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
            _ => return Err(DropReason::UnsupportedDhcpv6(msg.msg_type)),
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

        assert_eq!(msg.msg_type, MessageType::INFORMATION_REQUEST);
        assert_eq!(msg.transaction_id, [0x1c, 0x57, 0xca]);
        let client_id = "0001000130adec9800155d300e15";
        if let Some(DhcpOption::ClientId(data)) = msg.get_option(OptionCode::CLIENT_ID) {
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
        let mut msg = Message::new(MessageType::REPLY);
        msg.set_transaction_id(TRANSACTION_ID);
        msg.insert_option(DhcpOption::ClientId(hex_to_bytes(CLIENT_ID_HEX)));
        msg.insert_option(DhcpOption::ServerId(hex_to_bytes(SERVER_ID_HEX)));

        let dns_servers = vec![hex_to_ipv6(DNS1_HEX), hex_to_ipv6(DNS2_HEX)];
        msg.insert_option(DhcpOption::DnsServers(dns_servers.clone()));

        // Encode and decode to verify round-trip
        let decoded = Message::decode(&msg.encode()).expect("Failed to decode encoded message");

        assert_eq!(decoded.msg_type, MessageType::REPLY);
        assert_eq!(decoded.transaction_id, TRANSACTION_ID);

        let DhcpOption::ClientId(data) = decoded
            .get_option(OptionCode::CLIENT_ID)
            .expect("ClientId not found")
        else {
            panic!("Wrong option type for ClientId");
        };
        assert_eq!(bytes_to_hex(data), CLIENT_ID_HEX);

        let DhcpOption::ServerId(data) = decoded
            .get_option(OptionCode::SERVER_ID)
            .expect("ServerId not found")
        else {
            panic!("Wrong option type for ServerId");
        };
        assert_eq!(bytes_to_hex(data), SERVER_ID_HEX);

        let DhcpOption::DnsServers(servers) = decoded
            .get_option(OptionCode::DNS_SERVERS)
            .expect("DnsServers not found")
        else {
            panic!("Wrong option type for DnsServers");
        };
        assert_eq!(servers, &dns_servers);
    }
}
