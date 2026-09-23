// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate contains just the plain-data descriptors (RX/TX buffers and
//! metadata, VLAN info, offload/RSS capability flags, endpoint-action
//! signals, and small helper functions on `TxSegment` slices).

#![no_std]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

use bitfield_struct::bitfield;

pub const ETHERNET_HEADER_LEN: u32 = 14;
pub const ETHERNET_VLAN_HEADER_LEN: u32 = 18;

pub const IPV4_MIN_HEADER_LEN: u16 = 20;
pub const IPV6_MIN_HEADER_LEN: u16 = 40;

#[bitfield(u16)]
pub struct VlanMetadata {
    /// Priority for 802.1Q.
    #[bits(3)]
    pub priority: u8,
    /// In pretty much every circumstance this is false. When
    /// it is used, setting DEI will inform switches/routing infra
    /// that this can be dropped before higher priority traffic.
    pub drop_eligible_indicator: bool,
    /// The 802.1Q ID for this transmission.
    #[bits(12)]
    pub vlan_id: u16,
}

/// A receive buffer ID.
#[derive(Debug, Copy, Clone)]
#[repr(transparent)]
pub struct RxId(pub u32);

/// An individual segment in guest memory of a receive buffer.
#[derive(Debug, Copy, Clone)]
pub struct RxBufferSegment {
    /// Guest physical address.
    pub gpa: u64,
    /// The number of bytes in this range.
    pub len: u32,
}

/// Receive packet metadata.
#[derive(Debug, Copy, Clone)]
pub struct RxMetadata {
    /// The offset of the packet data from the beginning of the receive buffer.
    pub offset: usize,
    /// The length of the packet in bytes.
    pub len: usize,
    /// The IP checksum validation state.
    pub ip_checksum: RxChecksumState,
    /// The L4 checksum validation state.
    pub l4_checksum: RxChecksumState,
    /// The L4 protocol.
    pub l4_protocol: L4Protocol,
    /// Information about 802.1Q VLAN tagging. When a vlan is in use, this structure
    /// is populated. Only applies when traffic is being received over an L2 connection,
    /// so L3-only or above traffic will not use this option.
    pub vlan: Option<VlanMetadata>,
}

impl Default for RxMetadata {
    fn default() -> Self {
        Self {
            offset: 0,
            len: 0,
            ip_checksum: RxChecksumState::Unknown,
            l4_checksum: RxChecksumState::Unknown,
            l4_protocol: L4Protocol::Unknown,
            vlan: None,
        }
    }
}

/// The "L4" protocol: the TCP/UDP layer.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum L4Protocol {
    Unknown,
    Tcp,
    Udp,
}

/// The receive checksum state for a packet.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RxChecksumState {
    /// The checksum was not evaluated.
    Unknown,
    /// The checksum value is correct.
    Good,
    /// The checksum value is incorrect.
    Bad,
    /// The checksum has been validated, but the value in the header is wrong.
    ///
    /// This occurs when LRO/RSC offload has been performed--multiple packet
    /// payloads are glommed together without updating the checksum in the first
    /// packet's header.
    ValidatedButWrong,
}

impl RxChecksumState {
    /// Returns true if the checksum has been validated.
    pub fn is_valid(self) -> bool {
        self == Self::Good || self == Self::ValidatedButWrong
    }
}

/// A transmit ID. This may be used by multiple segments at the same time.
#[derive(Debug, Copy, Clone)]
#[repr(transparent)]
pub struct TxId(pub u32);

#[derive(Debug, Clone)]
/// The segment type.
pub enum TxSegmentType {
    /// The start of a packet.
    Head(TxMetadata),
    /// A packet continuation.
    Tail,
}

#[derive(Debug, Clone)]
/// Transmit packet metadata.
pub struct TxMetadata {
    /// The transmit ID.
    pub id: TxId,
    /// The number of segments, including this one.
    pub segment_count: u8,
    /// Flags.
    pub flags: TxFlags,
    /// The total length of the packet in bytes.
    pub len: u32,
    /// The length of the Ethernet frame header. Only guaranteed to be set if
    /// various offload flags are set.
    pub l2_len: u8,
    /// The length of the IP header. Only guaranteed to be set if various
    /// offload flags are set.
    pub l3_len: u16,
    /// The length of the TCP header. Only guaranteed to be set if various
    /// offload flags are set.
    pub l4_len: u8,
    /// The offset into the buffer where the L4 header begins (TCP or UDP). Only
    /// expected to be set if offload (checksum and/or segmentation) flags are set.
    pub transport_header_offset: u16,
    /// The maximum segment size, used for segmentation offload (TSO or USO).
    /// Only guaranteed to be set if [`TxFlags::offload_tcp_segmentation`] or
    /// [`TxFlags::offload_udp_segmentation`] is set.
    pub max_segment_size: u16,
    /// Information about 802.1Q VLAN tagging. When a vlan is in use, this structure
    /// is populated. Only applies when traffic is being sent over an L2 connection,
    /// so L3-only or above traffic will not use this option.
    pub vlan: Option<VlanMetadata>,
}

/// Flags affecting transmit behavior.
#[bitfield(u8)]
pub struct TxFlags {
    /// Offload IPv4 header checksum calculation.
    ///
    /// `l3_protocol`, `l2_len`, and `l3_len` must be set.
    pub offload_ip_header_checksum: bool,
    /// Offload the TCP checksum calculation.
    ///
    /// `l3_protocol`, `l2_len`, and `l3_len` must be set.
    pub offload_tcp_checksum: bool,
    /// Offload the UDP checksum calculation.
    ///
    /// `l3_protocol`, `l2_len`, and `l3_len` must be set.
    pub offload_udp_checksum: bool,
    /// Offload the TCP segmentation, allowing packets to be larger than the
    /// MTU.
    ///
    /// `l3_protocol`, `l2_len`, `l3_len`, `l4_len`, and `tcp_segment_size` must
    /// be set.
    pub offload_tcp_segmentation: bool,
    /// If true, the packet is IPv4.
    pub is_ipv4: bool,
    /// If true, the packet is IPv6. Mutually exclusive with `is_ipv4`.
    pub is_ipv6: bool,
    /// Offload UDP segmentation (USO), allowing UDP packets larger than the
    /// MTU. `l2_len`, `l3_len`, and `max_segment_size` must be set.
    pub offload_udp_segmentation: bool,
    #[bits(1)]
    _reserved: u8,
}

impl Default for TxMetadata {
    fn default() -> Self {
        Self {
            id: TxId(0),
            segment_count: 0,
            len: 0,
            flags: TxFlags::new(),
            l2_len: 0,
            l3_len: 0,
            l4_len: 0,
            transport_header_offset: 0,
            max_segment_size: 0,
            vlan: None,
        }
    }
}

#[derive(Debug, Clone)]
/// A transmit packet segment.
pub struct TxSegment {
    /// The segment type (head or tail).
    pub ty: TxSegmentType,
    /// The guest address of this segment.
    pub gpa: u64,
    /// The length of this segment.
    pub len: u32,
}

/// Computes the number of packets in `segments`.
pub fn packet_count(mut segments: &[TxSegment]) -> usize {
    let mut packet_count = 0;
    while let Some(head) = segments.first() {
        let TxSegmentType::Head(metadata) = &head.ty else {
            unreachable!()
        };
        segments = &segments[metadata.segment_count as usize..];
        packet_count += 1;
    }
    packet_count
}

/// Gets the next packet from a list of segments, returning the packet metadata,
/// the segments in the packet, and the remaining segments.
pub fn next_packet(segments: &[TxSegment]) -> (&TxMetadata, &[TxSegment], &[TxSegment]) {
    let metadata = if let TxSegmentType::Head(metadata) = &segments[0].ty {
        metadata
    } else {
        unreachable!();
    };
    let (this, rest) = segments.split_at(metadata.segment_count.into());
    (metadata, this, rest)
}

/// Multi-queue related support.
#[derive(Debug, Copy, Clone)]
pub struct MultiQueueSupport {
    /// The number of supported queues.
    pub max_queues: u16,
    /// The size of the RSS indirection table.
    pub indirection_table_size: u16,
}

/// The set of supported transmit offloads.
#[derive(Debug, Copy, Clone, Default)]
pub struct TxOffloadSupport {
    /// IPv4 header checksum offload.
    pub ipv4_header: bool,
    /// TCP checksum offload.
    pub tcp: bool,
    /// UDP checksum offload.
    pub udp: bool,
    /// TCP segmentation offload.
    pub tso: bool,
    /// UDP segmentation offload (USO).
    pub uso: bool,
}

#[derive(Debug, Clone)]
pub struct RssConfig<'a> {
    pub key: &'a [u8],
    pub indirection_table: &'a [u16],
    pub flags: u32, // TODO
}

/// A signal returned from `Endpoint::wait_for_endpoint_action` indicating why
/// the caller was woken.
#[derive(PartialEq, Debug)]
pub enum EndpointAction {
    RestartRequired,
    LinkStatusNotify(bool),
}
