// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The microVM restore packet, version 4 or, with network settings, version 5,
//! and the time sample of the NVX time ABI v1, as the guest reads them through
//! the portb device.
//!
//! Every byte the guest reads is a port exit, so the records are compact and
//! readable four bytes at a time.

use mesh::MeshPayload;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use thiserror::Error;

/// The restore packet magic.
pub const RESTORE_PACKET_MAGIC: [u8; 3] = *b"OVR";
/// The version of a restore packet without network settings.
pub const RESTORE_PACKET_VERSION: u8 = 4;
/// The version of a restore packet that carries network settings: version
/// 4's fields, with the network byte in place of the reserved byte, and the
/// DNS server after the entropy. Only a restore that replaces the network
/// policy of a snapshot without a tier gives one, so that a guest that reads
/// version 4 alone fails such a restore rather than ignore its settings.
pub const RESTORE_PACKET_NETWORK_VERSION: u8 = 5;
/// The length of the restore packet header.
pub const RESTORE_PACKET_HEADER_LEN: usize = 32;
/// The length of one memory range record.
pub const RESTORE_RANGE_LEN: usize = 16;
/// The length of the restore entropy.
pub const RESTORE_ENTROPY_LEN: usize = 64;
/// The length of the DNS server record of a version 5 packet: an IPv6
/// address, or an IPv4 address and twelve zero bytes, or zeros without a DNS
/// server.
pub const RESTORE_DNS_SERVER_LEN: usize = 16;
/// The most memory ranges a packet can carry.
pub const MAX_RESTORE_RANGES: usize = u8::MAX as usize;
/// The online VP targets a packet can carry; 0 means no processor target.
pub const VALID_ONLINE_VP_COUNTS: [u8; 5] = [0, 1, 2, 4, 8];
/// The largest rate deviation magnitude: 250 ppm scaled by 2^16.
pub const MAX_RATE_DEVIATION: i32 = 250 * 65_536;

/// `D` came from the UTC delta.
pub const FLAG_DOWNTIME_UTC: u8 = 1 << 0;
/// An explicit RAM target was requested.
pub const FLAG_MEMORY_TARGET: u8 = 1 << 1;
/// Host input is gated until the guest acknowledges through port `0x605`.
pub const FLAG_ACK_REQUIRED: u8 = 1 << 2;
/// A test hook altered time ABI behavior.
pub const FLAG_TEST_HOOKS: u8 = 1 << 3;
const RESTORE_PACKET_FLAGS: u8 =
    FLAG_DOWNTIME_UTC | FLAG_MEMORY_TARGET | FLAG_ACK_REQUIRED | FLAG_TEST_HOOKS;

/// The bits of a version 5 packet's network byte that give the family of its
/// DNS server: 0 for none, [`NETWORK_DNS_IPV4`], or [`NETWORK_DNS_IPV6`].
pub const NETWORK_DNS_MASK: u8 = 0x3;
/// The network byte's family of an IPv4 DNS server.
pub const NETWORK_DNS_IPV4: u8 = 1;
/// The network byte's family of an IPv6 DNS server.
pub const NETWORK_DNS_IPV6: u8 = 2;
/// The network byte's bit that asks the guest to announce its listeners.
pub const NETWORK_ANNOUNCE_LISTENERS: u8 = 1 << 2;
const NETWORK_BITS: u8 = NETWORK_DNS_MASK | NETWORK_ANNOUNCE_LISTENERS;

/// The portb selector of the restore packet.
pub const RESTORE_PACKET_SELECT: u8 = 0xa5;
/// The portb selector of the generation ID.
pub const GENERATION_ID_SELECT: u8 = 0xa6;
/// The portb selector that latches a time sample.
pub const TIME_SAMPLE_SELECT: u8 = 0xa7;
/// The port of the time-sample window.
pub const TIME_WINDOW_PORT: u16 = 0xeb;
/// The portb status bit advertising the time-sample window.
pub const STATUS_TIME_SAMPLE_AVAILABLE: u8 = 1 << 6;
/// The time sample version.
pub const TIME_SAMPLE_VERSION: u8 = 1;
/// The length of a time sample.
pub const TIME_SAMPLE_LEN: usize = 16;

/// A restore packet or time sample is malformed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RestorePacketError {
    /// The record is shorter than its header.
    #[error("record is {0} bytes, shorter than its header")]
    Truncated(usize),
    /// The magic is wrong.
    #[error("bad restore packet magic")]
    Magic,
    /// The version is not supported.
    #[error("unsupported version {0}")]
    Version(u8),
    /// Reserved flag bits or bytes are set.
    #[error("reserved bits are set")]
    Reserved,
    /// The online VP target is not 0, 1, 2, 4, or 8.
    #[error("invalid online VP target {0}")]
    OnlineVpCount(u8),
    /// Memory ranges without a memory target, or too many ranges.
    #[error("invalid memory range count {0}")]
    MemoryRanges(usize),
    /// The generation counter of a restore is zero.
    #[error("restore generation is zero")]
    Generation,
    /// The rate deviation is beyond the tolerance.
    #[error("rate deviation {0} is beyond the tolerance")]
    RateDeviation(i32),
    /// The network byte of a version 5 packet sets reserved bits, or names no
    /// DNS server family.
    #[error("invalid network byte {0:#04x}")]
    Network(u8),
    /// The DNS server of a version 5 packet is unspecified, pads an IPv4
    /// address with a byte that is not zero, or is not zero although the
    /// network byte names no DNS server.
    #[error("invalid DNS server")]
    DnsServer,
    /// The record length does not match its counts.
    #[error("record is {actual} bytes, expected {expected}")]
    Length {
        /// The expected length.
        expected: usize,
        /// The actual length.
        actual: usize,
    },
}

/// A guest RAM range in the restore packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, MeshPayload)]
pub struct RestoreMemoryRange {
    /// The first guest physical address.
    pub gpa_start: u64,
    /// The length in bytes.
    pub length: u64,
}

/// The DNS server of a [`RestoreNetwork`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, MeshPayload)]
pub enum RestoreDnsServer {
    /// An IPv4 server.
    Ipv4(Ipv4Addr),
    /// An IPv6 server.
    Ipv6(Ipv6Addr),
}

impl From<IpAddr> for RestoreDnsServer {
    fn from(address: IpAddr) -> Self {
        match address {
            IpAddr::V4(address) => Self::Ipv4(address),
            IpAddr::V6(address) => Self::Ipv6(address),
        }
    }
}

impl From<RestoreDnsServer> for IpAddr {
    fn from(server: RestoreDnsServer) -> Self {
        match server {
            RestoreDnsServer::Ipv4(address) => Self::V4(address),
            RestoreDnsServer::Ipv6(address) => Self::V6(address),
        }
    }
}

/// The network settings of a restore that replaces the network policy of a
/// snapshot without a tier, which a version 5 packet carries: what a cold
/// boot under the new policy names on the guest's command line, and the
/// guest learns from no other source after such a restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, MeshPayload)]
pub struct RestoreNetwork {
    /// The guest's DNS server, one of its gateway's addresses, which a cold
    /// boot names with `virtnet_dns=`.
    pub dns_server: Option<RestoreDnsServer>,
    /// Whether the guest announces its listeners to its gateway, which a
    /// cold boot asks with `virtnet_ingress=`.
    pub announce_listeners: bool,
}

impl RestoreNetwork {
    /// Returns the network byte that encodes the settings.
    fn byte(&self) -> u8 {
        let dns = match self.dns_server {
            None => 0,
            Some(RestoreDnsServer::Ipv4(_)) => NETWORK_DNS_IPV4,
            Some(RestoreDnsServer::Ipv6(_)) => NETWORK_DNS_IPV6,
        };
        let announce = if self.announce_listeners {
            NETWORK_ANNOUNCE_LISTENERS
        } else {
            0
        };
        dns | announce
    }

    /// Returns the DNS server record that encodes the settings.
    fn dns_server_record(&self) -> [u8; RESTORE_DNS_SERVER_LEN] {
        let mut record = [0; RESTORE_DNS_SERVER_LEN];
        match self.dns_server {
            None => {}
            Some(RestoreDnsServer::Ipv4(address)) => record[..4].copy_from_slice(&address.octets()),
            Some(RestoreDnsServer::Ipv6(address)) => record = address.octets(),
        }
        record
    }

    /// Decodes the settings from the network byte `byte` and the DNS server
    /// record `record`.
    fn decode(byte: u8, record: &[u8; RESTORE_DNS_SERVER_LEN]) -> Result<Self, RestorePacketError> {
        if byte & !NETWORK_BITS != 0 {
            return Err(RestorePacketError::Network(byte));
        }
        let [a, b, c, d, padding @ ..] = *record;
        let dns_server = match byte & NETWORK_DNS_MASK {
            0 if *record == [0; RESTORE_DNS_SERVER_LEN] => None,
            NETWORK_DNS_IPV4 if padding == [0; RESTORE_DNS_SERVER_LEN - 4] => {
                Some(RestoreDnsServer::Ipv4(Ipv4Addr::new(a, b, c, d)))
            }
            NETWORK_DNS_IPV6 => Some(RestoreDnsServer::Ipv6(Ipv6Addr::from(*record))),
            NETWORK_DNS_MASK => return Err(RestorePacketError::Network(byte)),
            _ => return Err(RestorePacketError::DnsServer),
        };
        let network = Self {
            dns_server,
            announce_listeners: byte & NETWORK_ANNOUNCE_LISTENERS != 0,
        };
        network.validate()?;
        Ok(network)
    }

    /// Checks that the DNS server, if any, is not unspecified.
    fn validate(&self) -> Result<(), RestorePacketError> {
        if self
            .dns_server
            .is_some_and(|server| IpAddr::from(server).is_unspecified())
        {
            return Err(RestorePacketError::DnsServer);
        }
        Ok(())
    }
}

/// The restore packet fields the controller knows before the worker starts.
#[derive(Debug, Clone, PartialEq, Eq, MeshPayload)]
pub struct RestorePacketBase {
    /// The online VP target: 0 for none, else 1, 2, 4, or 8.
    pub online_vp_count: u8,
    /// Whether an explicit RAM target was requested.
    pub memory_target: bool,
    /// Whether the guest must acknowledge through port `0x605`.
    pub ack_required: bool,
    /// The generation counter `g` of the restored process, at least 1.
    pub generation: u32,
    /// The RAM ranges to online, only with a memory target.
    pub ranges: Vec<RestoreMemoryRange>,
    /// The restore entropy; its first 16 bytes are the generation ID.
    pub entropy: [u8; RESTORE_ENTROPY_LEN],
    /// The network settings of a restore that replaces the network policy of
    /// a snapshot without a tier, which make the packet version 5.
    pub network: Option<RestoreNetwork>,
}

impl RestorePacketBase {
    /// Checks the fields the packet encoding constrains.
    pub fn validate(&self) -> Result<(), RestorePacketError> {
        if !VALID_ONLINE_VP_COUNTS.contains(&self.online_vp_count) {
            return Err(RestorePacketError::OnlineVpCount(self.online_vp_count));
        }
        if self.ranges.len() > MAX_RESTORE_RANGES
            || (!self.memory_target && !self.ranges.is_empty())
        {
            return Err(RestorePacketError::MemoryRanges(self.ranges.len()));
        }
        if self.generation == 0 {
            return Err(RestorePacketError::Generation);
        }
        if let Some(network) = &self.network {
            network.validate()?;
        }
        Ok(())
    }

    /// Returns the version of the packet: [`RESTORE_PACKET_NETWORK_VERSION`]
    /// with network settings, and [`RESTORE_PACKET_VERSION`] without.
    pub fn version(&self) -> u8 {
        if self.network.is_some() {
            RESTORE_PACKET_NETWORK_VERSION
        } else {
            RESTORE_PACKET_VERSION
        }
    }

    /// Returns the encoded length of the packet.
    pub fn encoded_len(&self) -> usize {
        let dns_server = if self.network.is_some() {
            RESTORE_DNS_SERVER_LEN
        } else {
            0
        };
        RESTORE_PACKET_HEADER_LEN
            + RESTORE_RANGE_LEN * self.ranges.len()
            + RESTORE_ENTROPY_LEN
            + dns_server
    }
}

/// The time fields of the restore packet, sealed by the worker before the
/// first restored VP runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, MeshPayload)]
pub struct RestoreTimeRecord {
    /// The downtime `D`, in nanoseconds.
    pub downtime_ns: u64,
    /// Whether `D` came from the UTC delta.
    pub downtime_utc: bool,
    /// `(F_d - F_s) / F_s` in ppm scaled by 2^16.
    pub rate_deviation: i32,
    /// Whether a test hook is active.
    pub test_hooks: bool,
}

/// A complete restore packet: version 4, or version 5 when its base carries
/// network settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePacket {
    /// The controller's fields.
    pub base: RestorePacketBase,
    /// The worker's sealed time fields.
    pub time: RestoreTimeRecord,
    /// Host UTC when the guest first selected the packet, in nanoseconds
    /// since the Unix epoch.
    pub utc_ns: u64,
}

impl RestorePacket {
    /// Encodes the packet.
    pub fn encode(&self) -> Result<Vec<u8>, RestorePacketError> {
        self.base.validate()?;
        if self.time.rate_deviation.unsigned_abs() > MAX_RATE_DEVIATION.unsigned_abs() {
            return Err(RestorePacketError::RateDeviation(self.time.rate_deviation));
        }
        let mut flags = 0;
        for (set, flag) in [
            (self.time.downtime_utc, FLAG_DOWNTIME_UTC),
            (self.base.memory_target, FLAG_MEMORY_TARGET),
            (self.base.ack_required, FLAG_ACK_REQUIRED),
            (self.time.test_hooks, FLAG_TEST_HOOKS),
        ] {
            if set {
                flags |= flag;
            }
        }
        let mut bytes = Vec::with_capacity(self.base.encoded_len());
        bytes.extend_from_slice(&RESTORE_PACKET_MAGIC);
        bytes.extend_from_slice(&[
            self.base.version(),
            flags,
            self.base.online_vp_count,
            self.base.ranges.len() as u8,
            self.base.network.map_or(0, |network| network.byte()),
        ]);
        bytes.extend_from_slice(&self.base.generation.to_le_bytes());
        bytes.extend_from_slice(&self.time.rate_deviation.to_le_bytes());
        bytes.extend_from_slice(&self.time.downtime_ns.to_le_bytes());
        bytes.extend_from_slice(&self.utc_ns.to_le_bytes());
        for range in &self.base.ranges {
            bytes.extend_from_slice(&range.gpa_start.to_le_bytes());
            bytes.extend_from_slice(&range.length.to_le_bytes());
        }
        bytes.extend_from_slice(&self.base.entropy);
        if let Some(network) = &self.base.network {
            bytes.extend_from_slice(&network.dns_server_record());
        }
        debug_assert_eq!(bytes.len(), self.base.encoded_len());
        Ok(bytes)
    }

    /// Decodes and validates a packet.
    pub fn decode(bytes: &[u8]) -> Result<Self, RestorePacketError> {
        let header: &[u8; RESTORE_PACKET_HEADER_LEN] = bytes
            .get(..RESTORE_PACKET_HEADER_LEN)
            .and_then(|header| header.try_into().ok())
            .ok_or(RestorePacketError::Truncated(bytes.len()))?;
        if header[..3] != RESTORE_PACKET_MAGIC {
            return Err(RestorePacketError::Magic);
        }
        // Version 5 puts its network byte in version 4's reserved byte.
        let network_byte = match header[3] {
            RESTORE_PACKET_VERSION => None,
            RESTORE_PACKET_NETWORK_VERSION => Some(header[7]),
            version => return Err(RestorePacketError::Version(version)),
        };
        let flags = header[4];
        if flags & !RESTORE_PACKET_FLAGS != 0 || (network_byte.is_none() && header[7] != 0) {
            return Err(RestorePacketError::Reserved);
        }
        let range_count = usize::from(header[6]);
        let entropy_offset = RESTORE_PACKET_HEADER_LEN + RESTORE_RANGE_LEN * range_count;
        let dns_server_offset = entropy_offset + RESTORE_ENTROPY_LEN;
        let expected = if network_byte.is_some() {
            dns_server_offset + RESTORE_DNS_SERVER_LEN
        } else {
            dns_server_offset
        };
        if bytes.len() != expected {
            return Err(RestorePacketError::Length {
                expected,
                actual: bytes.len(),
            });
        }
        let u32_at =
            |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let u64_at =
            |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let ranges = (0..range_count)
            .map(|i| {
                let offset = RESTORE_PACKET_HEADER_LEN + RESTORE_RANGE_LEN * i;
                RestoreMemoryRange {
                    gpa_start: u64_at(offset),
                    length: u64_at(offset + 8),
                }
            })
            .collect();
        let network = network_byte
            .map(|byte| {
                RestoreNetwork::decode(byte, bytes[dns_server_offset..].try_into().unwrap())
            })
            .transpose()?;
        let packet = Self {
            base: RestorePacketBase {
                online_vp_count: header[5],
                memory_target: flags & FLAG_MEMORY_TARGET != 0,
                ack_required: flags & FLAG_ACK_REQUIRED != 0,
                generation: u32_at(8),
                ranges,
                entropy: bytes[entropy_offset..dns_server_offset].try_into().unwrap(),
                network,
            },
            time: RestoreTimeRecord {
                downtime_ns: u64_at(16),
                downtime_utc: flags & FLAG_DOWNTIME_UTC != 0,
                rate_deviation: u32_at(12) as i32,
                test_hooks: flags & FLAG_TEST_HOOKS != 0,
            },
            utc_ns: u64_at(24),
        };
        packet.base.validate()?;
        if packet.time.rate_deviation.unsigned_abs() > MAX_RATE_DEVIATION.unsigned_abs() {
            return Err(RestorePacketError::RateDeviation(
                packet.time.rate_deviation,
            ));
        }
        Ok(packet)
    }
}

/// A time sample, latched by selector [`TIME_SAMPLE_SELECT`] and read from
/// port [`TIME_WINDOW_PORT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSample {
    /// Whether a test hook is active.
    pub test_hooks: bool,
    /// The generation counter `g` of this VM process.
    pub generation: u32,
    /// Host UTC at the latch, in nanoseconds since the Unix epoch.
    pub utc_ns: u64,
}

impl TimeSample {
    /// Encodes the sample.
    pub fn encode(&self) -> [u8; TIME_SAMPLE_LEN] {
        let mut bytes = [0; TIME_SAMPLE_LEN];
        bytes[0] = TIME_SAMPLE_VERSION;
        bytes[1] = if self.test_hooks { FLAG_TEST_HOOKS } else { 0 };
        bytes[4..8].copy_from_slice(&self.generation.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.utc_ns.to_le_bytes());
        bytes
    }

    /// Decodes and validates a sample.
    pub fn decode(bytes: &[u8]) -> Result<Self, RestorePacketError> {
        let bytes: &[u8; TIME_SAMPLE_LEN] =
            bytes.try_into().map_err(|_| RestorePacketError::Length {
                expected: TIME_SAMPLE_LEN,
                actual: bytes.len(),
            })?;
        if bytes[0] != TIME_SAMPLE_VERSION {
            return Err(RestorePacketError::Version(bytes[0]));
        }
        if bytes[1] & !FLAG_TEST_HOOKS != 0 || bytes[2..4] != [0, 0] {
            return Err(RestorePacketError::Reserved);
        }
        Ok(Self {
            test_hooks: bytes[1] & FLAG_TEST_HOOKS != 0,
            generation: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            utc_ns: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entropy() -> [u8; RESTORE_ENTROPY_LEN] {
        std::array::from_fn(|i| i as u8)
    }

    fn packet_without_ranges() -> RestorePacket {
        RestorePacket {
            base: RestorePacketBase {
                online_vp_count: 2,
                memory_target: false,
                ack_required: true,
                generation: 7,
                ranges: Vec::new(),
                entropy: entropy(),
                network: None,
            },
            time: RestoreTimeRecord {
                downtime_ns: 0x0102_0304_0506_0708,
                downtime_utc: true,
                rate_deviation: -2,
                test_hooks: false,
            },
            utc_ns: 0x1122_3344_5566_7788,
        }
    }

    fn packet_with_ranges() -> RestorePacket {
        RestorePacket {
            base: RestorePacketBase {
                online_vp_count: 0,
                memory_target: true,
                ack_required: false,
                generation: 1,
                ranges: vec![
                    RestoreMemoryRange {
                        gpa_start: 0x1_0000_0000,
                        length: 0x800_0000,
                    },
                    RestoreMemoryRange {
                        gpa_start: 0x2_0000_0000,
                        length: 0x1000_0000,
                    },
                ],
                entropy: entropy(),
                network: None,
            },
            time: RestoreTimeRecord {
                downtime_ns: 30_000_000_000,
                downtime_utc: false,
                rate_deviation: MAX_RATE_DEVIATION,
                test_hooks: true,
            },
            utc_ns: 1_700_000_000_000_000_000,
        }
    }

    #[test]
    fn golden_packet_without_ranges() {
        let mut expected = vec![
            b'O', b'V', b'R', 4, 0x05, 2, 0, 0, // header
            7, 0, 0, 0, // generation
            0xfe, 0xff, 0xff, 0xff, // rate deviation
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // downtime
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // UTC
        ];
        expected.extend(entropy());
        let packet = packet_without_ranges();
        let bytes = packet.encode().unwrap();
        assert_eq!(bytes.len(), 96);
        assert_eq!(bytes, expected);
        assert_eq!(RestorePacket::decode(&bytes).unwrap(), packet);
    }

    #[test]
    fn golden_packet_with_ranges() {
        let mut expected = vec![
            b'O', b'V', b'R', 4, 0x0a, 0, 2, 0, // header
            1, 0, 0, 0, // generation
            0x00, 0x00, 0xfa, 0x00, // rate deviation 250 * 2^16
            0x00, 0xac, 0x23, 0xfc, 0x06, 0x00, 0x00, 0x00, // downtime 30 s
            0x00, 0x00, 0x2a, 0x36, 0xfe, 0x9c, 0x97, 0x17, // UTC
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // range 0 start
            0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, // range 0 length
            0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // range 1 start
            0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, // range 1 length
        ];
        expected.extend(entropy());
        let packet = packet_with_ranges();
        let bytes = packet.encode().unwrap();
        assert_eq!(bytes.len(), 128);
        assert_eq!(bytes, expected);
        assert_eq!(RestorePacket::decode(&bytes).unwrap(), packet);
    }

    #[test]
    fn decode_rejects_malformed_packets() {
        let good = packet_with_ranges().encode().unwrap();
        let mutate = |offset: usize, value: u8| {
            let mut bytes = good.clone();
            bytes[offset] = value;
            RestorePacket::decode(&bytes).unwrap_err()
        };
        assert_eq!(mutate(0, b'X'), RestorePacketError::Magic);
        assert_eq!(mutate(3, 3), RestorePacketError::Version(3));
        assert_eq!(mutate(4, 0x1a), RestorePacketError::Reserved);
        assert_eq!(mutate(7, 1), RestorePacketError::Reserved);
        assert_eq!(mutate(5, 3), RestorePacketError::OnlineVpCount(3));
        assert_eq!(mutate(4, 0x08), RestorePacketError::MemoryRanges(2));
        assert!(matches!(mutate(6, 3), RestorePacketError::Length { .. }));
        assert_eq!(mutate(8, 0), RestorePacketError::Generation);
        assert!(matches!(
            mutate(14, 0xfb),
            RestorePacketError::RateDeviation(_)
        ));
        assert_eq!(
            RestorePacket::decode(&good[..31]).unwrap_err(),
            RestorePacketError::Truncated(31)
        );
        assert!(matches!(
            RestorePacket::decode(&good[..good.len() - 1]).unwrap_err(),
            RestorePacketError::Length { .. }
        ));
    }

    #[test]
    fn encode_rejects_invalid_fields() {
        let mut packet = packet_without_ranges();
        packet.base.online_vp_count = 3;
        assert_eq!(
            packet.encode().unwrap_err(),
            RestorePacketError::OnlineVpCount(3)
        );

        let mut packet = packet_with_ranges();
        packet.base.memory_target = false;
        assert_eq!(
            packet.encode().unwrap_err(),
            RestorePacketError::MemoryRanges(2)
        );

        let mut packet = packet_without_ranges();
        packet.base.generation = 0;
        assert_eq!(packet.encode().unwrap_err(), RestorePacketError::Generation);

        let mut packet = packet_without_ranges();
        packet.time.rate_deviation = -MAX_RATE_DEVIATION - 1;
        assert!(matches!(
            packet.encode().unwrap_err(),
            RestorePacketError::RateDeviation(_)
        ));

        for unspecified in [
            RestoreDnsServer::Ipv4(Ipv4Addr::UNSPECIFIED),
            RestoreDnsServer::Ipv6(Ipv6Addr::UNSPECIFIED),
        ] {
            let packet = network_packet(RestoreNetwork {
                dns_server: Some(unspecified),
                announce_listeners: false,
            });
            assert_eq!(packet.encode().unwrap_err(), RestorePacketError::DnsServer);
        }
    }

    fn network_packet(network: RestoreNetwork) -> RestorePacket {
        let mut packet = packet_without_ranges();
        packet.base.network = Some(network);
        packet
    }

    #[test]
    fn golden_packets_with_network_settings() {
        let ipv6: Ipv6Addr = "fd00::a00:1".parse().unwrap();
        for (network, network_byte, dns_server) in [
            // An IPv4 DNS server, with the guest's listeners announced.
            (
                RestoreNetwork {
                    dns_server: Some(RestoreDnsServer::Ipv4(Ipv4Addr::new(10, 0, 0, 1))),
                    announce_listeners: true,
                },
                0x05,
                [10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            // An IPv6 DNS server, without announcements.
            (
                RestoreNetwork {
                    dns_server: Some(RestoreDnsServer::Ipv6(ipv6)),
                    announce_listeners: false,
                },
                0x02,
                ipv6.octets(),
            ),
            // A policy that names no DNS server and publishes no listener.
            (
                RestoreNetwork {
                    dns_server: None,
                    announce_listeners: false,
                },
                0x00,
                [0; RESTORE_DNS_SERVER_LEN],
            ),
        ] {
            let mut expected = vec![
                b'O',
                b'V',
                b'R',
                5,
                0x05,
                2,
                0,
                network_byte, // header
                7,
                0,
                0,
                0, // generation
                0xfe,
                0xff,
                0xff,
                0xff, // rate deviation
                0x08,
                0x07,
                0x06,
                0x05,
                0x04,
                0x03,
                0x02,
                0x01, // downtime
                0x88,
                0x77,
                0x66,
                0x55,
                0x44,
                0x33,
                0x22,
                0x11, // UTC
            ];
            expected.extend(entropy());
            expected.extend(dns_server);
            let packet = network_packet(network);
            let bytes = packet.encode().unwrap();
            assert_eq!(bytes.len(), 112, "{network:?}");
            assert_eq!(bytes, expected, "{network:?}");
            assert_eq!(
                RestorePacket::decode(&bytes).unwrap(),
                packet,
                "{network:?}"
            );
        }
    }

    #[test]
    fn decode_rejects_malformed_network_settings() {
        let good = network_packet(RestoreNetwork {
            dns_server: Some(RestoreDnsServer::Ipv4(Ipv4Addr::new(10, 0, 0, 1))),
            announce_listeners: true,
        })
        .encode()
        .unwrap();
        let dns_server = good.len() - RESTORE_DNS_SERVER_LEN;
        let mutate = |offset: usize, value: u8| {
            let mut bytes = good.clone();
            bytes[offset] = value;
            RestorePacket::decode(&bytes).unwrap_err()
        };
        // Reserved bits, and the DNS server family that no version defines.
        assert_eq!(mutate(7, 0x0d), RestorePacketError::Network(0x0d));
        assert_eq!(mutate(7, 0x07), RestorePacketError::Network(0x07));
        // A nonzero byte after an IPv4 server, and a server where the network
        // byte names none.
        assert_eq!(mutate(dns_server + 4, 1), RestorePacketError::DnsServer);
        assert_eq!(mutate(7, 0x04), RestorePacketError::DnsServer);
        // Unspecified servers.
        let mut unspecified = good.clone();
        unspecified[dns_server..dns_server + 4].fill(0);
        assert_eq!(
            RestorePacket::decode(&unspecified).unwrap_err(),
            RestorePacketError::DnsServer
        );
        unspecified[7] = 0x06;
        assert_eq!(
            RestorePacket::decode(&unspecified).unwrap_err(),
            RestorePacketError::DnsServer
        );
        // Version 4 keeps its reserved byte zero, and its packet ends with the
        // entropy.
        assert_eq!(mutate(3, 4), RestorePacketError::Reserved);
        let mut version_4 = good.clone();
        version_4[3] = 4;
        version_4[7] = 0;
        assert!(matches!(
            RestorePacket::decode(&version_4).unwrap_err(),
            RestorePacketError::Length { .. }
        ));
        assert!(matches!(
            RestorePacket::decode(&good[..dns_server]).unwrap_err(),
            RestorePacketError::Length { .. }
        ));
    }

    #[test]
    fn golden_time_sample() {
        let sample = TimeSample {
            test_hooks: true,
            generation: 3,
            utc_ns: 0x0102_0304_0506_0708,
        };
        let bytes = sample.encode();
        assert_eq!(
            bytes,
            [
                1, 0x08, 0, 0, 3, 0, 0, 0, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01
            ]
        );
        assert_eq!(TimeSample::decode(&bytes).unwrap(), sample);

        let mut bad = bytes;
        bad[0] = 2;
        assert_eq!(
            TimeSample::decode(&bad).unwrap_err(),
            RestorePacketError::Version(2)
        );
        let mut bad = bytes;
        bad[1] = 0x01;
        assert_eq!(
            TimeSample::decode(&bad).unwrap_err(),
            RestorePacketError::Reserved
        );
        let mut bad = bytes;
        bad[3] = 1;
        assert_eq!(
            TimeSample::decode(&bad).unwrap_err(),
            RestorePacketError::Reserved
        );
        assert!(matches!(
            TimeSample::decode(&bytes[..15]).unwrap_err(),
            RestorePacketError::Length { .. }
        ));
    }
}
