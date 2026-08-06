// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V synthetic NIC vdev — the
//! `f8615163-df3e-46c5-913f-f2d2f965ed0e` VMBus device.
//!
//! Protocol constants and wire types are cross-checked against:
//! - Windows `nvspprotocol.h` (via bluebird
//!   `os2/publics:amd64/onecore/internal/vm/inc/nvspprotocol.h`),
//! - Linux `drivers/net/hyperv/hyperv_net.h`,
//! - openvmm `vm/devices/net/netvsp/src/protocol.rs`,
//! - puppet `kernel/shared/src/nvsc/ty.rs`.
//!
//! # Bring-up sequence
//!
//! The canonical sequence to go from an offer to a working data path
//! (matches Linux `netvsc_connect_vsp` and the Windows NVSC
//! driver):
//!
//! 1. [`Netvsp::open`] — allocate ring pages, establish the ring
//!    GPADL, open the channel with polling target VP.
//! 2. [`Netvsp::negotiate_version`] — walk [`NEGOTIATION_LADDER`]
//!    high → low. `INIT` messages always use
//!    [`NVSP_LEGACY_MESSAGE_SIZE`] regardless of version because we
//!    haven't negotiated yet.
//! 3. [`Netvsp::send_ndis_config`] — V2+ only, fire-and-forget.
//! 4. [`Netvsp::send_ndis_version`] — fire-and-forget.
//! 5. [`Netvsp::establish_recv_buffer`] — host writes RX packets
//!    into this buffer and delivers pointers via xfer-page packets.
//! 6. [`Netvsp::establish_send_buffer`] — optional send-buffer
//!    section allocator for RNDIS packets that fit in a section.
//! 7. [`Netvsp::rndis_init`] — RNDIS `Initialize` handshake.
//! 8. [`Netvsp::set_packet_filter`] — **required for RX**. NDIS's
//!    default filter is 0; the vSwitch silently drops every frame
//!    until we opt in via `OID_GEN_CURRENT_PACKET_FILTER`.
//! 9. [`Netvsp::send_ethernet`] / [`Netvsp::drain_inbound`] —
//!    normal data path.

use crate::Error;
use crate::Result;
use crate::channel::Channel;
use crate::channel::ChannelState;
use crate::protocol::Guid;
use crate::ring::PacketFlags;
use crate::ring::RawRingMem;
use crate::ring::RecvRing;
use crate::ring::SendRing;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU32;
use opentmk::context::HypercallTrait;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// `f8615163-df3e-46c5-913f-f2d2f965ed0e` — VMBus interface GUID for
/// the Hyper-V synthetic NIC.
pub const INTERFACE_GUID: Guid = Guid {
    data1: 0xf8615163,
    data2: 0xdf3e,
    data3: 0x46c5,
    data4: [0x91, 0x3f, 0xf2, 0xd2, 0xf9, 0x65, 0xed, 0x0e],
};

// ---------------------------------------------------------------------
// Protocol versions
// ---------------------------------------------------------------------

const fn make_version(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | minor as u32
}

/// NVSP protocol version. Values match `NVSP_PROTOCOL_VERSION_*` in
/// Windows `nvspprotocol.h`.
///
/// V3 is intentionally absent — never shipped.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Version {
    V1 = make_version(0, 2),
    V2 = make_version(3, 2),
    V4 = make_version(4, 0),
    V5 = make_version(5, 0),
    V6 = make_version(6, 0),
    V61 = make_version(6, 1),
}

/// Version-negotiation ladder, highest-first. Iterate top→bottom
/// and stop on the first `InitComplete` with `status = SUCCESS`.
/// Matches Linux `netvsc_connect_vsp`'s `ver_list` (iterated in
/// reverse there but same set).
pub const NEGOTIATION_LADDER: [Version; 6] = [
    Version::V61,
    Version::V6,
    Version::V5,
    Version::V4,
    Version::V2,
    Version::V1,
];

/// Sentinel written to `Netvsp::version` before negotiation succeeds.
pub const INVALID_PROTOCOL_VERSION: u32 = 0xFFFF_FFFF;

// ---------------------------------------------------------------------
// Wire-frame sizes
// ---------------------------------------------------------------------

/// Total NVSP wire-frame size for pre-V6.1 messages. Header + body
/// tail-padded to this length regardless of the actual body size.
pub const NVSP_LEGACY_MESSAGE_SIZE: usize = 0x1c; // 28

/// Total NVSP wire-frame size for V6.1+ messages.
pub const NVSP_V61_MESSAGE_SIZE: usize = 0x28; // 40

/// Frame size to use for the currently-negotiated version.
pub const fn frame_size_for(version: Version) -> usize {
    match version {
        Version::V61 => NVSP_V61_MESSAGE_SIZE,
        _ => NVSP_LEGACY_MESSAGE_SIZE,
    }
}

// ---------------------------------------------------------------------
// Message-type identifiers
// ---------------------------------------------------------------------

/// NVSP message-type discriminator. Written as the first `u32` of
/// every message on the wire.
pub mod msg_type {
    #![expect(missing_docs, reason = "documented at struct level")]

    pub const NONE: u32 = 0;

    // Init messages.
    pub const INIT: u32 = 1;
    pub const INIT_COMPLETE: u32 = 2;

    pub const VERSION_MSG_START: u32 = 100;

    // Version 1 messages.
    pub const V1_SEND_NDIS_VERSION: u32 = 100;
    pub const V1_SEND_RECV_BUF: u32 = 101;
    pub const V1_SEND_RECV_BUF_COMPLETE: u32 = 102;
    pub const V1_REVOKE_RECV_BUF: u32 = 103;
    pub const V1_SEND_SEND_BUF: u32 = 104;
    pub const V1_SEND_SEND_BUF_COMPLETE: u32 = 105;
    pub const V1_REVOKE_SEND_BUF: u32 = 106;
    pub const V1_SEND_RNDIS_PKT: u32 = 107;
    pub const V1_SEND_RNDIS_PKT_COMPLETE: u32 = 108;

    // Version 2 messages (only NDIS config is relevant pre-phase-3).
    pub const V2_SEND_NDIS_CONFIG: u32 = 125;

    // Version 4 messages.
    pub const V4_SEND_VF_ASSOCIATION: u32 = 128;
    pub const V4_SWITCH_DATA_PATH: u32 = 129;

    // Version 5 messages.
    pub const V5_SEND_INDIRECTION_TABLE: u32 = 134;
}

// ---------------------------------------------------------------------
// NVSP status codes
// ---------------------------------------------------------------------

/// NVSP status codes returned in completion messages.
pub mod status {
    #![expect(missing_docs, reason = "self-describing values from spec")]

    pub const NONE: u32 = 0;
    pub const SUCCESS: u32 = 1;
    pub const FAILURE: u32 = 2;
    pub const INVALID_RNDIS_PACKET: u32 = 5;
    pub const BUSY: u32 = 6;
    pub const PROTOCOL_VERSION_UNSUPPORTED: u32 = 7;
}

// ---------------------------------------------------------------------
// Buffer id constants
// ---------------------------------------------------------------------

/// Guest-chosen ID for the receive buffer. Value from puppet /
/// convention.
pub const NETVSC_RECEIVE_BUFFER_ID: u16 = 0xcafe;

/// Guest-chosen ID for the send buffer.
pub const NETVSC_SEND_BUFFER_ID: u16 = 0x0;

/// Sentinel meaning "not using a send-buffer section" in
/// `Nvsp1SendRndisPacket::send_buf_section_index`. External data
/// (GPA-direct) is being used instead.
pub const NETVSC_INVALID_INDEX: u32 = 0xFFFF_FFFF;

/// Minimum accepted section size in both send and receive buffer
/// completions — smaller than a legal Ethernet MTU is nonsense.
pub const NETVSC_MTU_MIN: u32 = 68;

/// Cap on the receive buffer for hosts speaking V1/V2 (15 MiB).
/// Modern hosts can accept up to ~2 GiB but there's no reason to
/// exceed 16 MiB for our smoke tests.
pub const NETVSC_RECEIVE_BUFFER_SIZE_LEGACY: usize = 15 * 1024 * 1024;

// ---------------------------------------------------------------------
// RNDIS channel-type constants (used inside Nvsp1SendRndisPacket)
// ---------------------------------------------------------------------

/// RNDIS data channel type (RMC_DATA).
pub const RMC_DATA: u32 = 0;

/// RNDIS control channel type (RMC_CONTROL) — used for init /
/// query / set.
pub const RMC_CONTROL: u32 = 1;

// ---------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------

/// Every NVSP packet starts with this 4-byte header.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MessageHeader {
    /// One of [`msg_type`].
    pub message_type: u32,
}

/// `NvspMessageTypeInit` body (VSC → VSP).
///
/// Both fields carry the same version — historical hosts used them
/// as a min/max range, but modern behaviour is to set both to the
/// requested version and try one at a time.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct NvspMsgInit {
    /// Requested version.
    pub protocol_version: u32,
    /// Same as [`Self::protocol_version`] on modern flows.
    pub protocol_version2: u32,
}

/// `NvspMessageTypeInitComplete` body (VSP → VSC).
///
/// `status` == [`status::SUCCESS`] on acceptance; anything else
/// means try the next version in the ladder.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct NvspMsgInitComplete {
    /// Deprecated — was `negotiated_protocol_ver` up through Win6.
    pub deprecated: u32,
    /// Max MDL chain length; informational.
    pub maximum_mdl_chain_length: u32,
    /// See [`status`].
    pub status: u32,
}

/// `Nvsp1MessageSendNdisVersion` body (VSC → VSP, no completion).
///
/// Sent immediately after `NvspMsgInit` succeeds and after
/// [`Nvsp2MsgSendNdisConfig`] (V2+ only). Standard values are
/// major=6, minor=0x1e (30) for V5+ or minor=1 for V4-.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Nvsp1MsgSendNdisVersion {
    /// NDIS major version.
    pub ndis_major_version: u32,
    /// NDIS minor version.
    pub ndis_minor_version: u32,
}

/// `Nvsp1MessageSendReceiveBuffer` body (VSC → VSP).
///
/// The `pad` field is not on the wire per Windows, but openvmm
/// explicitly reserves it as `u16` after `id` to avoid unsafe
/// `#[repr(packed)]` field references. Total body = 8 bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Nvsp1MsgSendBuffer {
    /// GPADL handle for the buffer (from
    /// `crate::gpadl::establish_gpadl`).
    pub gpadl_handle: u32,
    /// Buffer identifier — [`NETVSC_RECEIVE_BUFFER_ID`] or
    /// [`NETVSC_SEND_BUFFER_ID`].
    pub id: u16,
    /// Padding to `u32` alignment.
    pub pad: u16,
}

/// One entry in [`Nvsp1MsgSendRecvBufComplete::sections`].
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Nvsp1ReceiveBufferSection {
    /// Offset from buffer start where this section begins.
    pub offset: u32,
    /// Size of each sub-allocation.
    pub sub_alloc_size: u32,
    /// Number of sub-allocations.
    pub num_sub_allocs: u32,
    /// Offset one-past-end of the section.
    pub end_offset: u32,
}

/// `Nvsp1MessageSendReceiveBufferComplete` body (VSP → VSC).
///
/// **Note**: `sections` is a `[T; 1]` per spec — the Windows and
/// openvmm sources both note "no VSP has ever sent more than 1".
/// Callers should assert `num_sections == 1` on receipt.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Nvsp1MsgSendRecvBufComplete {
    /// See [`status`].
    pub status: u32,
    /// Number of sections; always 1 in practice.
    pub num_sections: u32,
    /// The single (in practice) section descriptor.
    pub sections: [Nvsp1ReceiveBufferSection; 1],
}

/// `Nvsp1MessageRevokeReceiveBuffer` body.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Nvsp1MsgRevokeRecvBuf {
    /// Must match the id used at [`Nvsp1MsgSendBuffer::id`].
    pub id: u16,
    /// Padding.
    pub pad: u16,
}

/// `Nvsp1MessageSendSendBufferComplete` body (VSP → VSC).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Nvsp1MsgSendSendBufComplete {
    /// See [`status`].
    pub status: u32,
    /// VSP-chosen section size for the send buffer.
    pub section_size: u32,
}

/// `Nvsp1MessageSendRndisPacket` body (bidirectional).
///
/// For phase-3 RNDIS init we send this with
/// `send_buf_section_index = NETVSC_INVALID_INDEX` and
/// `send_buf_section_size = 0`, indicating the RNDIS payload
/// travels via a GPA-direct external buffer.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Nvsp1MsgSendRndisPacket {
    /// [`RMC_CONTROL`] for init/query/set, [`RMC_DATA`] for
    /// packet frames.
    pub channel_type: u32,
    /// Send-buffer section index, or [`NETVSC_INVALID_INDEX`] to
    /// use GPA-direct.
    pub send_buf_section_index: u32,
    /// Section size in bytes, or 0 when
    /// `send_buf_section_index == NETVSC_INVALID_INDEX`.
    pub send_buf_section_size: u32,
}

/// `Nvsp1MessageSendRndisPacketComplete` body.
///
/// Only acknowledges the outgoing NVSP resource — the RNDIS
/// response itself arrives via `VM_PKT_DATA_USING_XFER_PAGES`
/// referencing the recv buffer.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Nvsp1MsgSendRndisPacketComplete {
    /// See [`status`].
    pub status: u32,
}

/// NDIS capability bits for [`Nvsp2MsgSendNdisConfig::capabilities`].
///
/// Bit positions match Windows `NVSP_2_NETVSC_CAPABILITIES`. Bit 4
/// (`correlation_id`) is intentionally never set from the guest per
/// Windows source comment "this capability has never worked
/// correctly, since day 1".
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct NdisCapabilities(pub u64);

impl NdisCapabilities {
    #![expect(missing_docs, reason = "bit accessors")]

    pub const VMQ: u64 = 1 << 0;
    pub const CHIMNEY: u64 = 1 << 1;
    pub const SRIOV: u64 = 1 << 2;
    pub const IEEE_8021Q: u64 = 1 << 3;
    // Bit 4 (CorrelationIdBroken): must always be 0 on guest.
    pub const TEAMING: u64 = 1 << 5;
    pub const VIRTUAL_SUBNET_ID: u64 = 1 << 6;
    pub const RSC_OVER_VMBUS: u64 = 1 << 7;
    pub const TIMESTAMP: u64 = 1 << 8;
    pub const RELIABLE_CORRELATION_ID: u64 = 1 << 9;
    pub const ALLOW_RSC_DISABLED_STATUS: u64 = 1 << 10;

    /// Recommended capability set as a function of negotiated
    /// version. Mirrors Linux's
    /// `negotiate_nvsp_ver` capability construction.
    pub fn recommended(version: Version) -> Self {
        let mut caps = Self::IEEE_8021Q;
        if version >= Version::V5 {
            caps |= Self::SRIOV | Self::TEAMING;
        }
        if version >= Version::V61 {
            caps |= Self::RSC_OVER_VMBUS;
        }
        Self(caps)
    }
}

impl core::ops::BitOr<u64> for NdisCapabilities {
    type Output = Self;
    fn bitor(self, rhs: u64) -> Self {
        Self(self.0 | rhs)
    }
}

impl core::ops::BitOrAssign<u64> for NdisCapabilities {
    fn bitor_assign(&mut self, rhs: u64) {
        self.0 |= rhs;
    }
}

/// `Nvsp2MessageSendNdisConfig` body (VSC → VSP, no completion).
///
/// Sent right after `NvspMsgInit` succeeds on V2+.
/// Fire-and-forget — the VSP does not reply.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Nvsp2MsgSendNdisConfig {
    /// Maximum Transmission Unit including Ethernet header.
    pub mtu: u32,
    /// Reserved, must be 0.
    pub reserved: u32,
    /// Bitfield of [`NdisCapabilities`].
    pub capabilities: u64,
}

// ---------------------------------------------------------------------
// RNDIS wire types (Phase 3)
// ---------------------------------------------------------------------

/// RNDIS message-type identifiers. See openvmm
/// `rndisprot.rs::MESSAGE_TYPE_*` and MS-RNDIS §2.2.
pub mod rndis {
    #![expect(missing_docs, reason = "self-documenting constants from MS-RNDIS")]

    // Request messages.
    pub const MESSAGE_TYPE_PACKET_MSG: u32 = 0x0000_0001;
    pub const MESSAGE_TYPE_INITIALIZE_MSG: u32 = 0x0000_0002;
    pub const MESSAGE_TYPE_HALT_MSG: u32 = 0x0000_0003;
    pub const MESSAGE_TYPE_QUERY_MSG: u32 = 0x0000_0004;
    pub const MESSAGE_TYPE_SET_MSG: u32 = 0x0000_0005;

    // Response messages.
    pub const MESSAGE_TYPE_INITIALIZE_CMPLT: u32 = 0x8000_0002;
    pub const MESSAGE_TYPE_QUERY_CMPLT: u32 = 0x8000_0004;
    pub const MESSAGE_TYPE_SET_CMPLT: u32 = 0x8000_0005;

    pub const STATUS_SUCCESS: u32 = 0x0000_0000;

    // RNDIS init version we advertise.
    pub const MAJOR_VERSION: u32 = 1;
    pub const MINOR_VERSION: u32 = 0;

    /// Max transfer size we request in `InitializeRequest`. 16 KiB
    /// matches puppet + Linux.
    pub const MAX_TRANSFER_SIZE: u32 = 0x4000;

    // ---------- NDIS OIDs ----------

    /// `OID_GEN_CURRENT_PACKET_FILTER` — the master packet-acceptance
    /// filter. Until the driver sets a non-zero value, NDIS accepts
    /// zero frames. Standard value bits below.
    pub const OID_GEN_CURRENT_PACKET_FILTER: u32 = 0x0001_010E;

    /// Accept frames whose destination MAC matches ours.
    pub const NDIS_PACKET_TYPE_DIRECTED: u32 = 0x0001;
    /// Accept specific multicasts (with a MAC list; not needed here).
    pub const NDIS_PACKET_TYPE_MULTICAST: u32 = 0x0002;
    /// Accept all multicasts.
    pub const NDIS_PACKET_TYPE_ALL_MULTICAST: u32 = 0x0004;
    /// Accept broadcast frames.
    pub const NDIS_PACKET_TYPE_BROADCAST: u32 = 0x0008;
    /// Accept every frame regardless of MAC.
    pub const NDIS_PACKET_TYPE_PROMISCUOUS: u32 = 0x0020;
}

/// Common 8-byte header on every RNDIS message.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RndisMessageHeader {
    /// One of [`rndis`] `MESSAGE_TYPE_*`.
    pub message_type: u32,
    /// Total message length in bytes including this header.
    pub message_length: u32,
}

/// `MESSAGE_TYPE_INITIALIZE_MSG` body (VSC → VSP), sent inside a
/// `V1_SEND_RNDIS_PKT(RMC_CONTROL)` on the netvsp channel.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RndisInitializeRequest {
    /// Guest-chosen id echoed back in the completion.
    pub request_id: u32,
    /// RNDIS major version — we advertise
    /// [`rndis::MAJOR_VERSION`].
    pub major_version: u32,
    /// RNDIS minor version.
    pub minor_version: u32,
    /// Max transfer size we support (bytes).
    pub max_transfer_size: u32,
}

/// `MESSAGE_TYPE_INITIALIZE_CMPLT` body (VSP → VSC), delivered via
/// `VM_PKT_DATA_USING_XFER_PAGES` referencing the recv buffer.
///
/// Field layout matches openvmm `rndisprot::InitializeComplete`.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RndisInitializeComplete {
    /// Echoes the `request_id` from the paired request.
    pub request_id: u32,
    /// [`rndis::STATUS_SUCCESS`] on success.
    pub status: u32,
    /// RNDIS major version the host is speaking.
    pub major_version: u32,
    /// RNDIS minor version.
    pub minor_version: u32,
    /// `DF_CONNECTIONLESS` etc.
    pub device_flags: u32,
    /// Medium — 0 for 802.3.
    pub medium: u32,
    /// Max RNDIS packets per netvsp message.
    pub max_packets_per_message: u32,
    /// Max transfer size the VSP accepts.
    pub max_transfer_size: u32,
    /// Log2 alignment factor for RNDIS packets.
    pub packet_alignment_factor: u32,
    /// Address-family list offset (unused for Ethernet).
    pub af_list_offset: u32,
    /// Address-family list size (unused).
    pub af_list_size: u32,
}

/// `MESSAGE_TYPE_PACKET_MSG` body — the RNDIS wrapper around a
/// single Ethernet frame. Sent VSC → VSP for TX and VSP → VSC for RX.
///
/// Wire layout (36 bytes) matches openvmm `rndisprot::Packet`. The
/// data buffer follows this struct at the offset specified by
/// `data_offset` (measured from the start of this `Packet` struct,
/// **not** the RNDIS `MessageHeader`).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RndisPacket {
    /// Offset of the Ethernet frame relative to the start of this
    /// `Packet` struct.
    pub data_offset: u32,
    /// Length of the Ethernet frame in bytes.
    pub data_length: u32,
    /// Offset of the OOB data buffer (unused: 0).
    pub oob_data_offset: u32,
    /// Length of the OOB data (unused: 0).
    pub oob_data_length: u32,
    /// Number of OOB elements (unused: 0).
    pub num_oob_data_elements: u32,
    /// Offset of the per-packet-info buffer (unused: 0).
    pub per_packet_info_offset: u32,
    /// Length of the per-packet-info buffer (unused: 0).
    pub per_packet_info_length: u32,
    /// VC handle (0 for connectionless).
    pub vc_handle: u32,
    /// Reserved (0).
    pub reserved: u32,
}

/// `MESSAGE_TYPE_SET_MSG` body — set a single NDIS OID on the
/// remote device.
///
/// Matches openvmm `rndisprot::SetRequest`. The information buffer
/// follows this struct at `information_buffer_offset` (measured
/// from start of `RndisSetRequest`, per RNDIS spec).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RndisSetRequest {
    /// Guest-chosen id echoed back in the completion.
    pub request_id: u32,
    /// OID identifier — e.g. [`rndis::OID_GEN_CURRENT_PACKET_FILTER`].
    pub oid: u32,
    /// Length of the information buffer in bytes.
    pub information_buffer_length: u32,
    /// Offset from the start of THIS struct to the information
    /// buffer. Standard value = `size_of::<RndisSetRequest>()` for
    /// an appended buffer.
    pub information_buffer_offset: u32,
    /// Device VC handle (unused, 0).
    pub device_vc_handle: u32,
}

/// `MESSAGE_TYPE_SET_CMPLT` body.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RndisSetComplete {
    /// Echoes the `request_id` from the paired request.
    pub request_id: u32,
    /// [`rndis::STATUS_SUCCESS`] on success.
    pub status: u32,
}

/// Encode a NVSP message: header + body copied into a
/// zero-padded fixed-size frame ([`frame_size_for`]).
///
/// Returns `Err(())` if the header + body exceed the frame size.
/// Frame size 40 (V6.1) accepts any body ≤ 36 bytes; frame size 28
/// (legacy) accepts any body ≤ 24 bytes.
pub fn encode_message<T: IntoBytes + Immutable>(
    message_type: u32,
    body: &T,
    version: Version,
    out: &mut [u8],
) -> core::result::Result<usize, ()> {
    let frame_size = frame_size_for(version);
    if out.len() < frame_size {
        return Err(());
    }
    let hdr = MessageHeader { message_type };
    let hdr_bytes = hdr.as_bytes();
    let body_bytes = body.as_bytes();
    if hdr_bytes.len() + body_bytes.len() > frame_size {
        return Err(());
    }
    out[..hdr_bytes.len()].copy_from_slice(hdr_bytes);
    out[hdr_bytes.len()..hdr_bytes.len() + body_bytes.len()].copy_from_slice(body_bytes);
    // Zero-fill the tail — hosts expect a stable frame size.
    for b in &mut out[hdr_bytes.len() + body_bytes.len()..frame_size] {
        *b = 0;
    }
    Ok(frame_size)
}

/// Parse an inbound NVSP frame's header and return
/// `(message_type, body_slice)`.
///
/// The body slice may include trailing padding bytes that the
/// sender zero-filled to reach [`frame_size_for`]; callers should
/// use `zerocopy::FromBytes::read_from_prefix` on the body and
/// ignore the tail.
pub fn parse_header(frame: &[u8]) -> core::result::Result<(u32, &[u8]), ()> {
    let (hdr, rest) = MessageHeader::read_from_prefix(frame).map_err(|_| ())?;
    Ok((hdr.message_type, rest))
}

// ---------------------------------------------------------------------
// Netvsp handle (Phase 1: open + negotiate + NDIS config/version)
// ---------------------------------------------------------------------

/// Owned page-aligned buffer with its GPADL registration.
///
/// Kept alive for the lifetime of the netvsp connection — the host
/// retains references to the underlying pages through the GPADL, so
/// dropping this while the host still owns it would be a use-after-free.
pub struct OwnedBuf {
    /// 4 KiB-aligned base pointer. Identity-mapped, so VA == GPA on
    /// our UEFI target.
    pub ptr: *mut u8,
    /// Size in bytes (multiple of 4096).
    pub len: usize,
    /// Registered GPADL id for this buffer.
    pub gpadl: crate::gpadl::GpadlHandle,
}

// SAFETY: `OwnedBuf` is only ever accessed by the single-threaded
// UEFI runtime; the pointer is a stable identity-mapped allocation.
#[expect(unsafe_code, reason = "single-threaded UEFI runtime; identity-mapped GPADL pages")]
unsafe impl Send for OwnedBuf {}
#[expect(unsafe_code, reason = "single-threaded UEFI runtime; identity-mapped GPADL pages")]
unsafe impl Sync for OwnedBuf {}

/// Guest-side handle to an open Hyper-V synthetic NIC channel.
///
/// Not thread-safe on its own; the UEFI runtime is effectively
/// single-threaded. State transitions happen only from the calling
/// thread.
pub struct Netvsp {
    channel: Channel,
    send: SendRing<RawRingMem>,
    recv: RecvRing<RawRingMem>,

    /// Negotiated NVSP version, or [`INVALID_PROTOCOL_VERSION`] until
    /// [`Self::negotiate_version`] succeeds.
    version: u32,

    /// Fresh id counter for outgoing completion-requested sends.
    /// Starts at 1; 0 is reserved for "no completion".
    next_transaction_id: u64,

    /// Guest-owned receive buffer + GPADL registered with the VSP.
    /// Populated by [`Self::establish_recv_buffer`].
    recv_buf: Option<OwnedBuf>,
    /// `sub_alloc_size` reported by the host in the recv-buf
    /// completion. Non-zero means the recv buffer is live.
    recv_section_size: u32,
    /// Number of receive sub-allocations.
    recv_section_count: u32,

    /// Guest-owned send buffer + GPADL. Populated by
    /// [`Self::establish_send_buffer`].
    send_buf: Option<OwnedBuf>,
    /// `section_size` reported by the host in the send-buf completion.
    send_section_size: u32,
    /// Send-section count = send_buf.len / send_section_size.
    send_section_count: u32,
}

/// Reasonable default retry budget for ring-buffer completion polling.
/// Roughly a few seconds of spinning on modern hardware.
const DEFAULT_MAX_POLLS: usize = 100_000_000;

/// Ring size: 8 data pages = 32 KiB per direction, power-of-two as
/// required by `RawRingMem::new`. Plus 1 control page → 9 pages per
/// direction, 18 pages (72 KiB) total per channel.
const RING_DATA_PAGES: usize = 8;
const RING_DATA_BYTES: usize = RING_DATA_PAGES * 4096;

impl Netvsp {
    /// Open the synthetic NIC channel described by `offer`.
    ///
    /// * Allocates a contiguous 18-page ring region.
    /// * Establishes a GPADL over it.
    /// * Opens the channel with `target_vp = u32::MAX` (polling).
    ///
    /// After this call succeeds, [`Self::negotiate_version`] must be
    /// called before any other message is sent.
    pub fn open<C: HypercallTrait>(
        ctx: &mut C,
        offer: &crate::protocol::OfferChannel,
    ) -> Result<Self> {
        // Layout of the 18-page ring region, in order:
        //   pages 0     : send control page
        //   pages 1..=8 : send data (8 pages, power-of-two)
        //   pages 9     : recv control page
        //   pages 10..=17: recv data
        const TOTAL_PAGES: usize = 2 * (1 + RING_DATA_PAGES);
        const REGION_BYTES: usize = TOTAL_PAGES * 4096;
        let layout = core::alloc::Layout::from_size_align(REGION_BYTES, 4096)
            .map_err(|_| Error::Parse {
                ty: None,
                reason: "netvsp ring layout invalid",
            })?;
        // SAFETY: alignment and size are validated above. Never freed.
        #[expect(unsafe_code, reason = "raw page-aligned allocation for GPADL")]
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if base.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "netvsp ring allocation failed",
            });
        }
        let base_gpa = base as u64;
        log::info!(
            "netvsp: ring region at GPA {:#x} ({} bytes)",
            base_gpa,
            REGION_BYTES,
        );

        let mut pfns: Vec<u64> = Vec::with_capacity(TOTAL_PAGES);
        for i in 0..TOTAL_PAGES {
            pfns.push((base_gpa + (i * 4096) as u64) >> 12);
        }
        let gpadl = crate::gpadl::establish_gpadl(
            ctx,
            offer.channel_id,
            REGION_BYTES as u32,
            &pfns,
        )?;
        log::info!(
            "netvsp: ring GPADL established id={:?} for channel {:?}",
            gpadl.id(),
            offer.channel_id,
        );

        // send data starts at page 1, recv at page 1+RING_DATA_PAGES+1
        // (control + 8 data + recv control page).
        let send_ctrl_off = 0usize;
        let send_data_off = 4096usize;
        let recv_ctrl_off = send_data_off + RING_DATA_BYTES;
        let recv_data_off = recv_ctrl_off + 4096;

        // SAFETY: `alloc_zeroed(REGION_BYTES)` returned a valid
        // contiguous allocation we own for the process lifetime.
        // Ring memory objects will hold only atomic pointers into it.
        #[expect(unsafe_code, reason = "raw ring memory over identity-mapped region")]
        let send_mem = unsafe {
            RawRingMem::new(
                base.add(send_ctrl_off) as *const AtomicU32,
                base.add(send_data_off) as *const AtomicU8,
                RING_DATA_BYTES,
            )
        };
        #[expect(unsafe_code, reason = "raw ring memory over identity-mapped region")]
        let recv_mem = unsafe {
            RawRingMem::new(
                base.add(recv_ctrl_off) as *const AtomicU32,
                base.add(recv_data_off) as *const AtomicU8,
                RING_DATA_BYTES,
            )
        };

        // Open the channel. `send_data_pages` matches our layout so
        // the host knows where the send ring ends and recv begins.
        let channel = crate::channel::open_channel(
            ctx,
            offer,
            gpadl,
            RING_DATA_PAGES as u32,
            offer.connection_id,
            offer.channel_id.0 as u16,
        )?;
        log::info!("netvsp: channel opened");

        Ok(Self {
            channel,
            send: SendRing::new(send_mem),
            recv: RecvRing::new(recv_mem),
            version: INVALID_PROTOCOL_VERSION,
            next_transaction_id: 1,
            recv_buf: None,
            recv_section_size: 0,
            recv_section_count: 0,
            send_buf: None,
            send_section_size: 0,
            send_section_count: 0,
        })
    }

    /// Negotiate the NVSP protocol version by walking [`NEGOTIATION_LADDER`]
    /// high→low. Returns the accepted version.
    ///
    /// A single ladder attempt = post `INIT` with completion flag,
    /// wait for `INIT_COMPLETE`. On `status = SUCCESS`, this becomes
    /// the negotiated version; otherwise fall through to the next
    /// ladder entry. Matches Linux's `netvsc_connect_vsp` and
    /// puppet's `negotiate_versions`.
    pub fn negotiate_version<C: HypercallTrait>(&mut self, ctx: &mut C) -> Result<Version> {
        for &v in &NEGOTIATION_LADDER {
            match self.try_init(ctx, v) {
                Ok(true) => {
                    self.version = v as u32;
                    log::info!("netvsp: negotiated version {:?}", v);
                    return Ok(v);
                }
                Ok(false) => {
                    log::debug!("netvsp: host rejected {:?}, trying next", v);
                    continue;
                }
                Err(Error::Timeout) => {
                    log::debug!("netvsp: {:?} timed out, trying next", v);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::VersionMismatch)
    }

    /// Send `NvspMsgInit` for `version` and await `InitComplete`.
    /// Returns `Ok(true)` if the host accepted, `Ok(false)` if the
    /// host returned a non-success status.
    fn try_init<C: HypercallTrait>(&mut self, ctx: &mut C, version: Version) -> Result<bool> {
        // NvspMsgInit is 8 bytes. `INIT` messages **always** go out
        // as `NVSP_LEGACY_MESSAGE_SIZE (28)` regardless of the
        // requested version — Windows: "Init message has always size
        // of NVSP_LEGACY_MESSAGE_SIZE in order to be able to
        // negotiate with older hosts" (`NetVsc.c` around
        // `NvscSendInitializationMessage`).
        let mut frame = [0u8; NVSP_LEGACY_MESSAGE_SIZE];
        // Use V1 to force the legacy frame length. We're not yet
        // negotiated so `frame_size_for(self.version)` would panic.
        encode_message(
            msg_type::INIT,
            &NvspMsgInit {
                protocol_version: version as u32,
                protocol_version2: version as u32,
            },
            Version::V1,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode INIT",
        })?;

        let response =
            self.send_and_await(ctx, &frame, DEFAULT_MAX_POLLS)?;
        let (ty, body) = parse_header(&response).map_err(|_| Error::Parse {
            ty: None,
            reason: "parse INIT_COMPLETE header",
        })?;
        if ty != msg_type::INIT_COMPLETE {
            return Err(Error::Parse {
                ty: None,
                reason: "expected INIT_COMPLETE",
            });
        }
        let (parsed, _) = NvspMsgInitComplete::read_from_prefix(body).map_err(|_| {
            Error::Parse {
                ty: None,
                reason: "parse INIT_COMPLETE body",
            }
        })?;
        Ok(parsed.status == status::SUCCESS)
    }

    /// Send `Nvsp2SendNdisConfig` (V2+ only). Fire-and-forget, no
    /// completion expected.
    ///
    /// Ignored if the current negotiated version is V1 (which does not
    /// use NDIS config).
    pub fn send_ndis_config<C: HypercallTrait>(&mut self, ctx: &mut C, mtu: u32) -> Result<()> {
        let version = self.version_typed()?;
        if version == Version::V1 {
            return Ok(());
        }
        let caps = NdisCapabilities::recommended(version);
        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V2_SEND_NDIS_CONFIG,
            &Nvsp2MsgSendNdisConfig {
                mtu,
                reserved: 0,
                capabilities: caps.0,
            },
            version,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode NDIS_CONFIG",
        })?;
        self.send_no_completion(ctx, &frame[..n])
    }

    /// Send `Nvsp1SendNdisVersion`. Fire-and-forget, no completion.
    ///
    /// `major = 6`, `minor = 30` for V5+ or `minor = 1` otherwise —
    /// matches Linux `negotiate_nvsp_ver` and puppet.
    pub fn send_ndis_version<C: HypercallTrait>(&mut self, ctx: &mut C) -> Result<()> {
        let version = self.version_typed()?;
        let ndis_minor = if version <= Version::V4 { 1 } else { 0x1e };
        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_NDIS_VERSION,
            &Nvsp1MsgSendNdisVersion {
                ndis_major_version: 6,
                ndis_minor_version: ndis_minor,
            },
            version,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode NDIS_VERSION",
        })?;
        self.send_no_completion(ctx, &frame[..n])
    }

    /// Establish the receive buffer (host → guest data path).
    ///
    /// Allocates `size` bytes (must be page-multiple), registers a
    /// GPADL for the whole region, sends `V1_SEND_RECV_BUF`, and
    /// waits for `V1_SEND_RECV_BUF_COMPLETE`. Validates:
    /// * `status == SUCCESS`
    /// * `num_sections == 1` (spec quirk: no VSP has ever sent more)
    /// * `sections[0].offset == 0`
    /// * `sub_alloc_size >= NETVSC_MTU_MIN`
    /// * `u64(sub_alloc_size) * u64(num_sub_allocs) <= size`
    ///
    /// A 16 MiB buffer produces ~147 `GpadlBody` messages posted
    /// back-to-back — this is the first real exercise of the
    /// `hypercalls::post_message` retry loop added in a prior commit.
    pub fn establish_recv_buffer<C: HypercallTrait>(
        &mut self,
        ctx: &mut C,
        size: usize,
    ) -> Result<()> {
        if self.recv_buf.is_some() {
            return Err(Error::Parse {
                ty: None,
                reason: "recv buffer already established",
            });
        }
        let buf = allocate_gpadl_buffer(ctx, self.channel.channel_id(), size)?;
        log::info!(
            "netvsp: recv-buf allocated {} bytes, GPADL id={:?}",
            size,
            buf.gpadl.id(),
        );

        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_RECV_BUF,
            &Nvsp1MsgSendBuffer {
                gpadl_handle: buf.gpadl.id().0,
                id: NETVSC_RECEIVE_BUFFER_ID,
                pad: 0,
            },
            self.version_typed()?,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_RECV_BUF",
        })?;

        let response = self.send_and_await(ctx, &frame[..n], DEFAULT_MAX_POLLS)?;
        let (ty, body) = parse_header(&response).map_err(|_| Error::Parse {
            ty: None,
            reason: "parse SEND_RECV_BUF_COMPLETE header",
        })?;
        if ty != msg_type::V1_SEND_RECV_BUF_COMPLETE {
            return Err(Error::Parse {
                ty: None,
                reason: "expected SEND_RECV_BUF_COMPLETE",
            });
        }
        let (parsed, _) = Nvsp1MsgSendRecvBufComplete::read_from_prefix(body).map_err(|_| {
            Error::Parse {
                ty: None,
                reason: "parse SEND_RECV_BUF_COMPLETE body",
            }
        })?;

        if parsed.status != status::SUCCESS {
            log::warn!(
                "netvsp: recv-buf complete status = {} (not SUCCESS)",
                parsed.status
            );
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf complete non-success status",
            });
        }
        if parsed.num_sections != 1 {
            log::warn!(
                "netvsp: recv-buf num_sections = {} (expected 1)",
                parsed.num_sections
            );
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf num_sections != 1",
            });
        }
        let sec = &parsed.sections[0];
        if sec.offset != 0 {
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf section offset != 0",
            });
        }
        if sec.sub_alloc_size < NETVSC_MTU_MIN {
            log::warn!(
                "netvsp: recv-buf sub_alloc_size = {} (< MTU_MIN={})",
                sec.sub_alloc_size,
                NETVSC_MTU_MIN
            );
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf sub_alloc_size < MTU_MIN",
            });
        }
        let used = (sec.sub_alloc_size as u64) * (sec.num_sub_allocs as u64);
        if used > size as u64 {
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf sub_alloc_size * count > allocation",
            });
        }
        log::info!(
            "netvsp: recv-buf established: sub_alloc_size={}, num_sub_allocs={}, used={}/{}",
            sec.sub_alloc_size,
            sec.num_sub_allocs,
            used,
            size,
        );

        self.recv_section_size = sec.sub_alloc_size;
        self.recv_section_count = sec.num_sub_allocs;
        self.recv_buf = Some(buf);
        Ok(())
    }

    /// Establish the send buffer (guest → host bulk data path).
    ///
    /// Same shape as [`Self::establish_recv_buffer`] but for the
    /// `SEND_SEND_BUF` variant. Response validation:
    /// * `status == SUCCESS`
    /// * `section_size >= NETVSC_MTU_MIN`
    /// * `send_section_count = size / section_size > 0`
    pub fn establish_send_buffer<C: HypercallTrait>(
        &mut self,
        ctx: &mut C,
        size: usize,
    ) -> Result<()> {
        if self.send_buf.is_some() {
            return Err(Error::Parse {
                ty: None,
                reason: "send buffer already established",
            });
        }
        let buf = allocate_gpadl_buffer(ctx, self.channel.channel_id(), size)?;
        log::info!(
            "netvsp: send-buf allocated {} bytes, GPADL id={:?}",
            size,
            buf.gpadl.id(),
        );

        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_SEND_BUF,
            &Nvsp1MsgSendBuffer {
                gpadl_handle: buf.gpadl.id().0,
                id: NETVSC_SEND_BUFFER_ID,
                pad: 0,
            },
            self.version_typed()?,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_SEND_BUF",
        })?;

        let response = self.send_and_await(ctx, &frame[..n], DEFAULT_MAX_POLLS)?;
        let (ty, body) = parse_header(&response).map_err(|_| Error::Parse {
            ty: None,
            reason: "parse SEND_SEND_BUF_COMPLETE header",
        })?;
        if ty != msg_type::V1_SEND_SEND_BUF_COMPLETE {
            return Err(Error::Parse {
                ty: None,
                reason: "expected SEND_SEND_BUF_COMPLETE",
            });
        }
        let (parsed, _) = Nvsp1MsgSendSendBufComplete::read_from_prefix(body).map_err(|_| {
            Error::Parse {
                ty: None,
                reason: "parse SEND_SEND_BUF_COMPLETE body",
            }
        })?;
        if parsed.status != status::SUCCESS {
            return Err(Error::Parse {
                ty: None,
                reason: "send-buf complete non-success status",
            });
        }
        if parsed.section_size < NETVSC_MTU_MIN {
            return Err(Error::Parse {
                ty: None,
                reason: "send-buf section_size < MTU_MIN",
            });
        }
        let count = (size as u32) / parsed.section_size;
        if count == 0 {
            return Err(Error::Parse {
                ty: None,
                reason: "send-buf section_size larger than buffer",
            });
        }
        log::info!(
            "netvsp: send-buf established: section_size={}, count={}",
            parsed.section_size,
            count,
        );

        self.send_section_size = parsed.section_size;
        self.send_section_count = count;
        self.send_buf = Some(buf);
        Ok(())
    }

    /// The negotiated version, converted back to the typed enum.
    /// Errors if negotiation hasn't happened yet.
    pub fn version_typed(&self) -> Result<Version> {
        match self.version {
            v if v == Version::V1 as u32 => Ok(Version::V1),
            v if v == Version::V2 as u32 => Ok(Version::V2),
            v if v == Version::V4 as u32 => Ok(Version::V4),
            v if v == Version::V5 as u32 => Ok(Version::V5),
            v if v == Version::V6 as u32 => Ok(Version::V6),
            v if v == Version::V61 as u32 => Ok(Version::V61),
            _ => Err(Error::VersionMismatch),
        }
    }

    /// Section size reported by the host for the receive buffer.
    /// Zero until [`Self::establish_recv_buffer`] succeeds.
    pub fn recv_section_size(&self) -> u32 {
        self.recv_section_size
    }

    /// Section size reported by the host for the send buffer.
    /// Zero until [`Self::establish_send_buffer`] succeeds.
    pub fn send_section_size(&self) -> u32 {
        self.send_section_size
    }

    /// Send RNDIS `Initialize` and wait for the paired
    /// `RNDIS_INITIALIZE_COMPLETE`.
    ///
    /// Sequence:
    /// 1. Allocate a page-aligned buffer, write
    ///    `RndisMessageHeader + RndisInitializeRequest`.
    /// 2. Send `V1_SEND_RNDIS_PKT(RMC_CONTROL)` via
    ///    `SendRing::write_gpa_direct` with the RNDIS bytes carried
    ///    as external GPA-direct data. Completion-requested flag set.
    /// 3. Wait for `V1_SEND_RNDIS_PKT_COMPLETE` — acks the NVSP
    ///    resource, NOT the RNDIS init itself.
    /// 4. Wait for the RNDIS response arriving as a
    ///    `VM_PKT_DATA_USING_XFER_PAGES` referencing our recv buffer.
    /// 5. Parse the transfer-page header + range, read
    ///    `RndisInitializeComplete` from recv_buf + range.byte_offset,
    ///    verify `status == STATUS_SUCCESS`.
    /// 6. Send `VM_PKT_COMP` back so the host can free its transfer
    ///    pages.
    ///
    /// Requires [`Self::establish_recv_buffer`] to have succeeded
    /// (we need the recv buffer to receive the completion into).
    pub fn rndis_init<C: HypercallTrait>(&mut self, ctx: &mut C) -> Result<()> {
        let recv_buf = self.recv_buf.as_ref().ok_or(Error::Parse {
            ty: None,
            reason: "rndis_init requires establish_recv_buffer first",
        })?;
        let recv_base = recv_buf.ptr;
        let recv_len = recv_buf.len;

        // Allocate a page-aligned RNDIS message buffer. Total is
        // small (8 + 16 = 24 bytes) but we take a whole page for
        // clean GPA-direct addressing.
        let rndis_layout = core::alloc::Layout::from_size_align(4096, 4096).map_err(|_| {
            Error::Parse {
                ty: None,
                reason: "rndis buffer layout",
            }
        })?;
        // SAFETY: layout is a validated single-page allocation.
        // Kept alive at least until we've received the completion.
        #[expect(unsafe_code, reason = "page-aligned RNDIS message allocation")]
        let rndis_ptr = unsafe { alloc::alloc::alloc_zeroed(rndis_layout) };
        if rndis_ptr.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "rndis buffer alloc failed",
            });
        }

        // Build RNDIS message: header + InitializeRequest.
        let request_id: u32 = 1;
        let hdr = RndisMessageHeader {
            message_type: rndis::MESSAGE_TYPE_INITIALIZE_MSG,
            message_length: (core::mem::size_of::<RndisMessageHeader>()
                + core::mem::size_of::<RndisInitializeRequest>())
                as u32,
        };
        let req = RndisInitializeRequest {
            request_id,
            major_version: rndis::MAJOR_VERSION,
            minor_version: rndis::MINOR_VERSION,
            max_transfer_size: rndis::MAX_TRANSFER_SIZE,
        };
        let hdr_bytes = hdr.as_bytes();
        let req_bytes = req.as_bytes();
        // SAFETY: `rndis_ptr` is a valid 4 KiB allocation and the
        // combined write is 24 bytes.
        #[expect(unsafe_code, reason = "copy RNDIS bytes into own buffer")]
        unsafe {
            core::ptr::copy_nonoverlapping(hdr_bytes.as_ptr(), rndis_ptr, hdr_bytes.len());
            core::ptr::copy_nonoverlapping(
                req_bytes.as_ptr(),
                rndis_ptr.add(hdr_bytes.len()),
                req_bytes.len(),
            );
        }
        let rndis_total_bytes = (hdr_bytes.len() + req_bytes.len()) as u32;
        log::info!(
            "netvsp: rndis buffer at GPA {:#x}, {} bytes",
            rndis_ptr as u64,
            rndis_total_bytes
        );

        // Build the NVSP wrapper.
        let mut nvsp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_RNDIS_PKT,
            &Nvsp1MsgSendRndisPacket {
                channel_type: RMC_CONTROL,
                send_buf_section_index: NETVSC_INVALID_INDEX,
                send_buf_section_size: 0,
            },
            self.version_typed()?,
            &mut nvsp_frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_RNDIS_PKT",
        })?;

        // Send GPA-direct with completion.
        let tid = self.alloc_transaction_id();
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        let rndis_gpa = rndis_ptr as u64;
        let pfns = [rndis_gpa >> 12];
        let offset = (rndis_gpa & 0xFFF) as u32;
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        let need_signal = self.send.write_gpa_direct(
            &pfns,
            offset,
            rndis_total_bytes,
            &nvsp_frame[..n],
            flags,
            tid,
        )?;
        let _ = need_signal;
        self.channel.signal(ctx)?;
        log::info!("netvsp: RNDIS INITIALIZE sent (tid={:#x})", tid);

        // Wait for the two responses on the recv ring:
        // (a) VM_PKT_COMP with our tid → acks the NVSP send.
        // (b) VM_PKT_DATA_USING_XFER_PAGES → carries
        //     RNDIS_INITIALIZE_COMPLETE inside the recv buffer.
        // They can arrive in either order in principle; puppet
        // observed nvsp completion first, then xfer-page. We accept
        // both orders.
        let mut got_nvsp_comp = false;
        let mut got_rndis_response = false;
        let mut buf = [0u8; 512];
        for _ in 0..DEFAULT_MAX_POLLS {
            match self.recv.read(&mut buf) {
                Ok(pkt) => {
                    match pkt.descriptor.packet_type {
                        crate::protocol::PacketType::VM_PKT_COMP
                            if pkt.descriptor.transaction_id == tid =>
                        {
                            log::info!(
                                "netvsp: got V1_SEND_RNDIS_PKT_COMPLETE (tid={:#x})",
                                tid
                            );
                            got_nvsp_comp = true;
                        }
                        crate::protocol::PacketType::VM_PKT_DATA_USING_XFER_PAGES => {
                            // Parse: pkt.buf starts at ext_header,
                            // then payload. buf[..ext_header_len] is
                            // the transfer-page header + ranges.
                            let ext_len = pkt.ext_header_len;
                            let host_tid = pkt.descriptor.transaction_id;
                            let (xhdr, _) =
                                crate::protocol::TransferPageHeader::read_from_prefix(&buf)
                                    .map_err(|_| Error::Parse {
                                        ty: None,
                                        reason: "parse TransferPageHeader",
                                    })?;
                            log::info!(
                                "netvsp: xfer-page packet: set_id={:#x} range_count={} host_tid={:#x}",
                                xhdr.transfer_page_set_id,
                                xhdr.range_count,
                                host_tid
                            );
                            if xhdr.transfer_page_set_id != NETVSC_RECEIVE_BUFFER_ID {
                                log::warn!(
                                    "netvsp: xfer-page set_id={:#x} != NETVSC_RECEIVE_BUFFER_ID",
                                    xhdr.transfer_page_set_id
                                );
                                continue;
                            }
                            if xhdr.range_count == 0 {
                                continue;
                            }
                            // First range describes the RNDIS message.
                            let (range0, _) = crate::protocol::TransferPageRange::read_from_prefix(
                                &buf[8..],
                            )
                            .map_err(|_| Error::Parse {
                                ty: None,
                                reason: "parse TransferPageRange",
                            })?;
                            log::info!(
                                "netvsp: xfer-page range0: offset={:#x} count={}",
                                range0.byte_offset,
                                range0.byte_count,
                            );
                            if (range0.byte_offset as usize + range0.byte_count as usize)
                                > recv_len
                            {
                                return Err(Error::Parse {
                                    ty: None,
                                    reason: "xfer-page range out of recv buf",
                                });
                            }
                            // SAFETY: recv_base + byte_offset..
                            // +byte_count is within the recv buffer
                            // we own; single-threaded UEFI.
                            #[expect(unsafe_code, reason = "read RNDIS response from recv buffer")]
                            let rndis_msg = unsafe {
                                core::slice::from_raw_parts(
                                    recv_base.add(range0.byte_offset as usize),
                                    range0.byte_count as usize,
                                )
                            };
                            let (rhdr, rest) = RndisMessageHeader::read_from_prefix(rndis_msg)
                                .map_err(|_| Error::Parse {
                                    ty: None,
                                    reason: "parse RndisMessageHeader",
                                })?;
                            log::info!(
                                "netvsp: RNDIS response type={:#x} len={}",
                                rhdr.message_type,
                                rhdr.message_length,
                            );
                            if rhdr.message_type != rndis::MESSAGE_TYPE_INITIALIZE_CMPLT {
                                return Err(Error::Parse {
                                    ty: None,
                                    reason: "expected RNDIS_INITIALIZE_CMPLT",
                                });
                            }
                            let (comp, _) = RndisInitializeComplete::read_from_prefix(rest)
                                .map_err(|_| Error::Parse {
                                    ty: None,
                                    reason: "parse RndisInitializeComplete",
                                })?;
                            log::info!(
                                "netvsp: RNDIS init complete status={:#x} request_id={:#x} \
                                 major={} minor={} device_flags={:#x} medium={} \
                                 max_packets={} max_transfer={}",
                                comp.status,
                                comp.request_id,
                                comp.major_version,
                                comp.minor_version,
                                comp.device_flags,
                                comp.medium,
                                comp.max_packets_per_message,
                                comp.max_transfer_size,
                            );
                            if comp.status != rndis::STATUS_SUCCESS {
                                return Err(Error::Parse {
                                    ty: None,
                                    reason: "RNDIS init status != SUCCESS",
                                });
                            }
                            if comp.request_id != request_id {
                                return Err(Error::Parse {
                                    ty: None,
                                    reason: "RNDIS response request_id mismatch",
                                });
                            }

                            // Send VM_PKT_COMP back so the host can
                            // free its transfer pages. Payload =
                            // Nvsp1MsgSendRndisPacketComplete { SUCCESS }
                            // per puppet's convention.
                            let mut comp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
                            let m = encode_message(
                                msg_type::V1_SEND_RNDIS_PKT_COMPLETE,
                                &Nvsp1MsgSendRndisPacketComplete {
                                    status: status::SUCCESS,
                                },
                                self.version_typed()?,
                                &mut comp_frame,
                            )
                            .map_err(|_| Error::Parse {
                                ty: None,
                                reason: "encode V1_SEND_RNDIS_PKT_COMPLETE",
                            })?;
                            self.send.write_completion(&comp_frame[..m], host_tid)?;
                            self.channel.signal(ctx)?;
                            log::info!(
                                "netvsp: xfer-page COMP sent back (host_tid={:#x})",
                                host_tid
                            );
                            got_rndis_response = true;
                            let _ = ext_len;
                        }
                        _ => {
                            log::debug!(
                                "netvsp: unexpected packet during rndis_init: type={:#x} tid={:#x}",
                                pkt.descriptor.packet_type.0,
                                pkt.descriptor.transaction_id,
                            );
                        }
                    }
                    if got_nvsp_comp && got_rndis_response {
                        return Ok(());
                    }
                }
                Err(Error::RingEmpty) => {
                    core::hint::spin_loop();
                }
                Err(e) => return Err(e),
            }
        }
        log::warn!(
            "netvsp: rndis_init timed out (got_nvsp_comp={} got_rndis_response={})",
            got_nvsp_comp,
            got_rndis_response
        );
        Err(Error::Timeout)
    }

    /// Set the NDIS packet filter to accept common frame types
    /// (broadcast, all multicast, directed unicast to our MAC).
    ///
    /// **Must be called before the host will deliver any Ethernet
    /// frames.** Until this succeeds, NDIS's filter is 0 → every
    /// frame is silently dropped by the vSwitch/netvsp pipeline
    /// **on the receive side** (send still works). Matches Linux's
    /// `rndis_filter_open`.
    ///
    /// Sequence:
    /// 1. Allocate a page-aligned buffer, write `RndisMessageHeader
    ///    + RndisSetRequest + [filter u32]`.
    /// 2. Send via GPA-direct wrapped in `V1_SEND_RNDIS_PKT(RMC_CONTROL)`.
    /// 3. Wait for `V1_SEND_RNDIS_PKT_COMPLETE` (NVSP-level ack).
    /// 4. Wait for `RNDIS_SET_CMPLT` on the xfer-page path.
    /// 5. Send `VM_PKT_COMP` back to release the transfer pages.
    ///
    /// Requires [`Self::rndis_init`] to have already succeeded.
    pub fn set_packet_filter<C: HypercallTrait>(
        &mut self,
        ctx: &mut C,
        filter: u32,
    ) -> Result<()> {
        let recv_buf = self.recv_buf.as_ref().ok_or(Error::Parse {
            ty: None,
            reason: "set_packet_filter requires recv buffer",
        })?;
        let recv_base = recv_buf.ptr;
        let recv_len = recv_buf.len;

        let rndis_layout = core::alloc::Layout::from_size_align(4096, 4096).map_err(|_| {
            Error::Parse {
                ty: None,
                reason: "set_pkt_filter buffer layout",
            }
        })?;
        // SAFETY: page-sized page-aligned request.
        #[expect(unsafe_code, reason = "page-aligned RNDIS SET buffer")]
        let rndis_ptr = unsafe { alloc::alloc::alloc_zeroed(rndis_layout) };
        if rndis_ptr.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "set_pkt_filter buffer alloc",
            });
        }

        let hdr_size = core::mem::size_of::<RndisMessageHeader>();
        let req_size = core::mem::size_of::<RndisSetRequest>();
        let info_size = core::mem::size_of::<u32>();
        let total_len = (hdr_size + req_size + info_size) as u32;

        let request_id: u32 = 2;
        let rndis_hdr = RndisMessageHeader {
            message_type: rndis::MESSAGE_TYPE_SET_MSG,
            message_length: total_len,
        };
        let set_req = RndisSetRequest {
            request_id,
            oid: rndis::OID_GEN_CURRENT_PACKET_FILTER,
            information_buffer_length: info_size as u32,
            information_buffer_offset: req_size as u32,
            device_vc_handle: 0,
        };

        // SAFETY: rndis_ptr is a valid 4 KiB allocation; total_len
        // = 8 + 20 + 4 = 32 << 4096.
        #[expect(unsafe_code, reason = "copy RNDIS bytes into own buffer")]
        unsafe {
            core::ptr::copy_nonoverlapping(
                rndis_hdr.as_bytes().as_ptr(),
                rndis_ptr,
                hdr_size,
            );
            core::ptr::copy_nonoverlapping(
                set_req.as_bytes().as_ptr(),
                rndis_ptr.add(hdr_size),
                req_size,
            );
            core::ptr::copy_nonoverlapping(
                filter.to_le_bytes().as_ptr(),
                rndis_ptr.add(hdr_size + req_size),
                info_size,
            );
        }

        // NVSP wrapper.
        let mut nvsp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_RNDIS_PKT,
            &Nvsp1MsgSendRndisPacket {
                channel_type: RMC_CONTROL,
                send_buf_section_index: NETVSC_INVALID_INDEX,
                send_buf_section_size: 0,
            },
            self.version_typed()?,
            &mut nvsp_frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_RNDIS_PKT (set)",
        })?;

        // GPA-direct send with completion.
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        let tid = self.alloc_transaction_id();
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        let rndis_gpa = rndis_ptr as u64;
        let pfns = [rndis_gpa >> 12];
        let offset = (rndis_gpa & 0xFFF) as u32;
        let _need_signal = self.send.write_gpa_direct(
            &pfns,
            offset,
            total_len,
            &nvsp_frame[..n],
            flags,
            tid,
        )?;
        self.channel.signal(ctx)?;
        log::info!(
            "netvsp: RNDIS SET packet_filter={:#x} sent (tid={:#x})",
            filter,
            tid
        );

        // Wait for both the NVSP send-completion AND the RNDIS SET
        // response. Same pattern as rndis_init.
        let mut got_nvsp_comp = false;
        let mut got_rndis_response = false;
        let mut buf = [0u8; 512];
        for _ in 0..DEFAULT_MAX_POLLS {
            match self.recv.read(&mut buf) {
                Ok(pkt) => {
                    match pkt.descriptor.packet_type {
                        crate::protocol::PacketType::VM_PKT_COMP
                            if pkt.descriptor.transaction_id == tid =>
                        {
                            log::info!("netvsp: RNDIS SET nvsp-comp received");
                            got_nvsp_comp = true;
                        }
                        crate::protocol::PacketType::VM_PKT_DATA_USING_XFER_PAGES => {
                            let host_tid = pkt.descriptor.transaction_id;
                            let (xhdr, _) =
                                crate::protocol::TransferPageHeader::read_from_prefix(&buf)
                                    .map_err(|_| Error::Parse {
                                        ty: None,
                                        reason: "parse TransferPageHeader",
                                    })?;
                            if xhdr.range_count == 0 {
                                continue;
                            }
                            let (r0, _) = crate::protocol::TransferPageRange::read_from_prefix(
                                &buf[8..],
                            )
                            .map_err(|_| Error::Parse {
                                ty: None,
                                reason: "parse TransferPageRange",
                            })?;
                            if (r0.byte_offset as usize + r0.byte_count as usize) > recv_len {
                                continue;
                            }
                            // SAFETY: bounds-checked; single-threaded.
                            #[expect(unsafe_code, reason = "read RNDIS SET response from recv")]
                            let rndis_msg = unsafe {
                                core::slice::from_raw_parts(
                                    recv_base.add(r0.byte_offset as usize),
                                    r0.byte_count as usize,
                                )
                            };
                            let (rhdr, rest) =
                                RndisMessageHeader::read_from_prefix(rndis_msg).map_err(|_| {
                                    Error::Parse {
                                        ty: None,
                                        reason: "parse RndisMessageHeader",
                                    }
                                })?;
                            if rhdr.message_type == rndis::MESSAGE_TYPE_SET_CMPLT {
                                let (sc, _) = RndisSetComplete::read_from_prefix(rest).map_err(
                                    |_| Error::Parse {
                                        ty: None,
                                        reason: "parse RndisSetComplete",
                                    },
                                )?;
                                log::info!(
                                    "netvsp: RNDIS SET_CMPLT request_id={:#x} status={:#x}",
                                    sc.request_id,
                                    sc.status,
                                );
                                if sc.status != rndis::STATUS_SUCCESS {
                                    return Err(Error::Parse {
                                        ty: None,
                                        reason: "RNDIS SET status != SUCCESS",
                                    });
                                }
                                if sc.request_id != request_id {
                                    return Err(Error::Parse {
                                        ty: None,
                                        reason: "RNDIS SET request_id mismatch",
                                    });
                                }
                                got_rndis_response = true;
                            }
                            // Regardless of whether it was our SET
                            // response or unrelated traffic, ack the
                            // xfer-page so the host can reap it.
                            let mut cf = [0u8; NVSP_V61_MESSAGE_SIZE];
                            let m = encode_message(
                                msg_type::V1_SEND_RNDIS_PKT_COMPLETE,
                                &Nvsp1MsgSendRndisPacketComplete {
                                    status: status::SUCCESS,
                                },
                                self.version_typed()?,
                                &mut cf,
                            )
                            .map_err(|_| Error::Parse {
                                ty: None,
                                reason: "encode SET-ack",
                            })?;
                            self.send.write_completion(&cf[..m], host_tid)?;
                            self.channel.signal(ctx)?;
                        }
                        _ => {
                            log::debug!(
                                "netvsp: unexpected packet during SET: type={:#x} tid={:#x}",
                                pkt.descriptor.packet_type.0,
                                pkt.descriptor.transaction_id,
                            );
                        }
                    }
                    if got_nvsp_comp && got_rndis_response {
                        return Ok(());
                    }
                }
                Err(Error::RingEmpty) => core::hint::spin_loop(),
                Err(e) => return Err(e),
            }
        }
        log::warn!(
            "netvsp: set_packet_filter timed out (nvsp={} rndis={})",
            got_nvsp_comp,
            got_rndis_response
        );
        Err(Error::Timeout)
    }

    /// Send a raw Ethernet frame via RNDIS `PACKET_MSG` wrapped in
    /// `Nvsp1MsgSendRndisPacket(RMC_DATA)`, using a GPA-direct
    /// external buffer for the RNDIS message.
    ///
    /// This is the TX equivalent of what puppet's `send_eth_packet`
    /// does. `wait_for_completion` controls whether we spin waiting
    /// for the paired `V1_SEND_RNDIS_PKT_COMPLETE` (round-trip
    /// send) or fire-and-forget (bulk stress). Fire-and-forget still
    /// signals the host and returns; the host will pipeline
    /// completions on the recv ring for us to drain later.
    ///
    /// Requires [`Self::rndis_init`] to have succeeded (the host
    /// won't accept data packets before RNDIS init).
    ///
    /// # Wire layout
    ///
    /// The RNDIS message written to the buffer:
    /// ```text
    /// RndisMessageHeader { PACKET_MSG, message_length }
    /// RndisPacket { data_offset = size_of::<Packet>, data_length = frame.len() }
    /// [frame bytes]
    /// ```
    /// `data_offset` is measured from the start of the `RndisPacket`
    /// struct (openvmm convention). Note that per_packet_info /
    /// oob_data all zero for a plain unadorned frame.
    ///
    /// The NVSP wrapper:
    /// ```text
    /// Nvsp1MsgSendRndisPacket {
    ///   channel_type = RMC_DATA (0),
    ///   send_buf_section_index = NETVSC_INVALID_INDEX,
    ///   send_buf_section_size = 0,
    /// }
    /// ```
    pub fn send_ethernet<C: HypercallTrait>(
        &mut self,
        ctx: &mut C,
        frame: &[u8],
        wait_for_completion: bool,
    ) -> Result<()> {
        if frame.is_empty() || frame.len() > 4096 - 64 {
            return Err(Error::Parse {
                ty: None,
                reason: "ethernet frame size out of range",
            });
        }
        if self.recv_buf.is_none() {
            return Err(Error::Parse {
                ty: None,
                reason: "send_ethernet requires establish_recv_buffer first",
            });
        }

        // Allocate a page-aligned buffer. Layout inside:
        //   [0..8)   RndisMessageHeader
        //   [8..44)  RndisPacket (36 bytes)
        //   [44..)   Ethernet frame
        let rndis_layout = core::alloc::Layout::from_size_align(4096, 4096).map_err(|_| {
            Error::Parse {
                ty: None,
                reason: "rndis buffer layout",
            }
        })?;
        // SAFETY: validated single-page layout, single-threaded UEFI.
        #[expect(unsafe_code, reason = "page-aligned RNDIS message allocation")]
        let rndis_ptr = unsafe { alloc::alloc::alloc_zeroed(rndis_layout) };
        if rndis_ptr.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "rndis buffer alloc failed",
            });
        }
        let hdr_size = core::mem::size_of::<RndisMessageHeader>();
        let pkt_size = core::mem::size_of::<RndisPacket>();
        let total_len = (hdr_size + pkt_size + frame.len()) as u32;

        let rndis_hdr = RndisMessageHeader {
            message_type: rndis::MESSAGE_TYPE_PACKET_MSG,
            message_length: total_len,
        };
        // data_offset is measured from the START of RndisPacket,
        // not from the start of the whole RNDIS message. So it's
        // just pkt_size (the frame sits immediately after the Packet
        // struct).
        let rndis_pkt = RndisPacket {
            data_offset: pkt_size as u32,
            data_length: frame.len() as u32,
            oob_data_offset: 0,
            oob_data_length: 0,
            num_oob_data_elements: 0,
            per_packet_info_offset: 0,
            per_packet_info_length: 0,
            vc_handle: 0,
            reserved: 0,
        };

        // SAFETY: rndis_ptr is a valid 4 KiB allocation, and
        // hdr_size + pkt_size + frame.len() <= 4096 by the input
        // check above.
        #[expect(unsafe_code, reason = "copy RNDIS message parts into own buffer")]
        unsafe {
            core::ptr::copy_nonoverlapping(
                rndis_hdr.as_bytes().as_ptr(),
                rndis_ptr,
                hdr_size,
            );
            core::ptr::copy_nonoverlapping(
                rndis_pkt.as_bytes().as_ptr(),
                rndis_ptr.add(hdr_size),
                pkt_size,
            );
            core::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                rndis_ptr.add(hdr_size + pkt_size),
                frame.len(),
            );
        }

        // Build the NVSP wrapper.
        let mut nvsp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_RNDIS_PKT,
            &Nvsp1MsgSendRndisPacket {
                channel_type: RMC_DATA,
                send_buf_section_index: NETVSC_INVALID_INDEX,
                send_buf_section_size: 0,
            },
            self.version_typed()?,
            &mut nvsp_frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_RNDIS_PKT (data)",
        })?;

        // Post via GPA-direct.
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        let tid = self.alloc_transaction_id();
        let mut flags = PacketFlags::new();
        if wait_for_completion {
            flags.set_request_completion(true);
        }
        let rndis_gpa = rndis_ptr as u64;
        let pfns = [rndis_gpa >> 12];
        let offset = (rndis_gpa & 0xFFF) as u32;
        let _need_signal = self.send.write_gpa_direct(
            &pfns,
            offset,
            total_len,
            &nvsp_frame[..n],
            flags,
            tid,
        )?;
        self.channel.signal(ctx)?;

        if !wait_for_completion {
            // Note: we leak the RNDIS buffer here. For the smoke
            // + stress cases this is fine — opentmk's static heap
            // is 512 MiB. A real driver would free after the paired
            // completion arrives.
            return Ok(());
        }

        // Wait for V1_SEND_RNDIS_PKT_COMPLETE (matching tid).
        let mut buf = [0u8; 512];
        for _ in 0..DEFAULT_MAX_POLLS {
            match self.recv.read(&mut buf) {
                Ok(pkt) => {
                    if pkt.descriptor.packet_type
                        == crate::protocol::PacketType::VM_PKT_COMP
                        && pkt.descriptor.transaction_id == tid
                    {
                        // Peek the completion status — it's the first
                        // 4 bytes of the NVSP body.
                        let (_hdr, body) = parse_header(pkt.payload).map_err(|_| {
                            Error::Parse {
                                ty: None,
                                reason: "parse SEND_RNDIS_PKT_COMPLETE hdr",
                            }
                        })?;
                        let (comp, _) =
                            Nvsp1MsgSendRndisPacketComplete::read_from_prefix(body).map_err(
                                |_| Error::Parse {
                                    ty: None,
                                    reason: "parse SEND_RNDIS_PKT_COMPLETE body",
                                },
                            )?;
                        if comp.status != status::SUCCESS {
                            return Err(Error::Parse {
                                ty: None,
                                reason: "SEND_RNDIS_PKT_COMPLETE non-success status",
                            });
                        }
                        return Ok(());
                    }
                    // Any other packet type during a data send is
                    // most likely an inbound Ethernet frame the host
                    // is delivering to us. Log at debug and keep
                    // looking for our completion; the caller can
                    // drain those separately via receive_ethernet.
                    log::debug!(
                        "netvsp: skipping packet during send: type={:#x} tid={:#x}",
                        pkt.descriptor.packet_type.0,
                        pkt.descriptor.transaction_id,
                    );
                }
                Err(Error::RingEmpty) => core::hint::spin_loop(),
                Err(e) => return Err(e),
            }
        }
        Err(Error::Timeout)
    }

    /// Drain any pending inbound Ethernet frames (host → guest) and
    /// invoke `on_frame` with each one's bytes.
    ///
    /// The host delivers frames as `VM_PKT_DATA_USING_XFER_PAGES`
    /// packets referencing recv-buffer sections. We also send back
    /// `VM_PKT_COMP` for each so the host can free its transfer
    /// pages.
    ///
    /// Returns the number of frames drained. `max_polls` bounds the
    /// spin — 0 means "one non-blocking pass; return whatever's
    /// currently on the ring".
    pub fn drain_inbound<C: HypercallTrait>(
        &mut self,
        ctx: &mut C,
        max_polls: usize,
        mut on_frame: impl FnMut(&[u8]),
    ) -> Result<usize> {
        let recv_buf = self.recv_buf.as_ref().ok_or(Error::Parse {
            ty: None,
            reason: "drain_inbound requires establish_recv_buffer first",
        })?;
        let recv_base = recv_buf.ptr;
        let recv_len = recv_buf.len;
        let mut count = 0usize;
        let mut buf = [0u8; 4096];
        let iters = if max_polls == 0 { 1 } else { max_polls };
        for _ in 0..iters {
            match self.recv.read(&mut buf) {
                Ok(pkt) => match pkt.descriptor.packet_type {
                    crate::protocol::PacketType::VM_PKT_DATA_USING_XFER_PAGES => {
                        let host_tid = pkt.descriptor.transaction_id;
                        let (xhdr, _) =
                            crate::protocol::TransferPageHeader::read_from_prefix(&buf)
                                .map_err(|_| Error::Parse {
                                    ty: None,
                                    reason: "parse TransferPageHeader",
                                })?;
                        for i in 0..xhdr.range_count as usize {
                            let range_off = 8 + i * 8;
                            let (r, _) =
                                crate::protocol::TransferPageRange::read_from_prefix(
                                    &buf[range_off..],
                                )
                                .map_err(|_| Error::Parse {
                                    ty: None,
                                    reason: "parse TransferPageRange",
                                })?;
                            if (r.byte_offset as usize + r.byte_count as usize) > recv_len {
                                continue;
                            }
                            // SAFETY: bounds-checked above; single-threaded.
                            #[expect(unsafe_code, reason = "read from recv buffer")]
                            let msg = unsafe {
                                core::slice::from_raw_parts(
                                    recv_base.add(r.byte_offset as usize),
                                    r.byte_count as usize,
                                )
                            };
                            let (rhdr, rest) =
                                RndisMessageHeader::read_from_prefix(msg).map_err(|_| {
                                    Error::Parse {
                                        ty: None,
                                        reason: "parse RndisMessageHeader",
                                    }
                                })?;
                            if rhdr.message_type == rndis::MESSAGE_TYPE_PACKET_MSG {
                                let (rp, _) = RndisPacket::read_from_prefix(rest).map_err(
                                    |_| Error::Parse {
                                        ty: None,
                                        reason: "parse RndisPacket",
                                    },
                                )?;
                                let frame_off = rp.data_offset as usize;
                                let frame_len = rp.data_length as usize;
                                if frame_off + frame_len <= rest.len() {
                                    on_frame(&rest[frame_off..frame_off + frame_len]);
                                    count += 1;
                                }
                            } else {
                                log::debug!(
                                    "netvsp: drain saw non-packet RNDIS type {:#x}",
                                    rhdr.message_type
                                );
                            }
                        }
                        // Send VM_PKT_COMP so the host can reap the
                        // transfer pages.
                        let mut comp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
                        let m = encode_message(
                            msg_type::V1_SEND_RNDIS_PKT_COMPLETE,
                            &Nvsp1MsgSendRndisPacketComplete {
                                status: status::SUCCESS,
                            },
                            self.version_typed()?,
                            &mut comp_frame,
                        )
                        .map_err(|_| Error::Parse {
                            ty: None,
                            reason: "encode V1_SEND_RNDIS_PKT_COMPLETE",
                        })?;
                        self.send.write_completion(&comp_frame[..m], host_tid)?;
                        self.channel.signal(ctx)?;
                    }
                    crate::protocol::PacketType::VM_PKT_COMP => {
                        log::debug!(
                            "netvsp: drain saw stale VM_PKT_COMP tid={:#x}",
                            pkt.descriptor.transaction_id
                        );
                    }
                    _ => {
                        log::debug!(
                            "netvsp: drain saw unexpected packet type {:#x}",
                            pkt.descriptor.packet_type.0
                        );
                    }
                },
                Err(Error::RingEmpty) => {
                    if max_polls == 0 {
                        break;
                    }
                    core::hint::spin_loop();
                }
                Err(e) => return Err(e),
            }
        }
        Ok(count)
    }

    /// Recover the underlying channel for closing.
    pub fn into_channel(self) -> Channel {
        self.channel
    }

    // ---- internals ----

    /// Post an NVSP frame with the completion-requested flag and
    /// spin until we receive a matching `VM_PKT_COMP`. Returns the
    /// completion frame's payload bytes (owned copy — the ring's
    /// bytes are consumed by then).
    fn send_and_await<C: HypercallTrait>(
        &mut self,
        ctx: &mut C,
        frame: &[u8],
        max_polls: usize,
    ) -> Result<Vec<u8>> {
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        let tid = self.alloc_transaction_id();
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        let need_signal = self.send.write_inband(frame, flags, tid)?;
        // Openvmm-style host reader wakes on empty→non-empty
        // signal or on interrupt-mask 0 kicks. We always signal for
        // safety on the small-batch handshake path.
        let _ = need_signal;
        self.channel.signal(ctx)?;

        // Spin the recv ring waiting for a VM_PKT_COMP with matching tid.
        let mut recv_buf = [0u8; 512];
        for _ in 0..max_polls {
            match self.recv.read(&mut recv_buf) {
                Ok(pkt) => {
                    if pkt.descriptor.packet_type
                        == crate::protocol::PacketType::VM_PKT_COMP
                        && pkt.descriptor.transaction_id == tid
                    {
                        // Copy payload out and return.
                        return Ok(pkt.payload.to_vec());
                    }
                    // Non-matching packet — log and keep looking. In
                    // Phase 1 we don't expect any, but Phase 3 will
                    // see unsolicited xfer-page arrivals.
                    log::debug!(
                        "netvsp: unexpected packet type={:#x} tid={:#x} while awaiting {:#x}",
                        pkt.descriptor.packet_type.0,
                        pkt.descriptor.transaction_id,
                        tid,
                    );
                }
                Err(Error::RingEmpty) => {
                    core::hint::spin_loop();
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::Timeout)
    }

    /// Post an NVSP frame **without** the completion flag. Used for
    /// `SEND_NDIS_CONFIG` and `SEND_NDIS_VERSION` which have no
    /// reply per protocol.
    fn send_no_completion<C: HypercallTrait>(&mut self, ctx: &mut C, frame: &[u8]) -> Result<()> {
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        let need_signal =
            self.send.write_inband(frame, PacketFlags::new(), 0)?;
        let _ = need_signal;
        self.channel.signal(ctx)
    }

    fn alloc_transaction_id(&mut self) -> u64 {
        let tid = self.next_transaction_id;
        self.next_transaction_id = self.next_transaction_id.wrapping_add(1);
        if self.next_transaction_id == 0 {
            self.next_transaction_id = 1; // skip 0 sentinel
        }
        tid
    }
}

/// Allocate a page-aligned buffer of `size` bytes, register it as a
/// GPADL on `channel_id`, and return an [`OwnedBuf`] carrying the
/// pointer + gpadl handle.
///
/// `size` must be a multiple of 4096. Uses opentmk's static heap via
/// `alloc::alloc::alloc_zeroed`.
fn allocate_gpadl_buffer<C: HypercallTrait>(
    ctx: &mut C,
    channel_id: crate::protocol::ChannelId,
    size: usize,
) -> Result<OwnedBuf> {
    if size % 4096 != 0 || size == 0 {
        return Err(Error::Parse {
            ty: None,
            reason: "GPADL buffer size must be a positive multiple of 4096",
        });
    }
    let layout = core::alloc::Layout::from_size_align(size, 4096).map_err(|_| Error::Parse {
        ty: None,
        reason: "GPADL buffer layout invalid",
    })?;
    // SAFETY: layout is a validated non-zero page-aligned request.
    // The pointer is never freed — the host holds it for the
    // lifetime of the channel.
    #[expect(unsafe_code, reason = "page-aligned allocation for GPADL registration")]
    let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        return Err(Error::Parse {
            ty: None,
            reason: "GPADL buffer allocation failed",
        });
    }
    let base_gpa = ptr as u64;

    let pfn_count = size / 4096;
    let mut pfns: Vec<u64> = Vec::with_capacity(pfn_count);
    for i in 0..pfn_count {
        pfns.push((base_gpa + (i * 4096) as u64) >> 12);
    }
    let gpadl = crate::gpadl::establish_gpadl(ctx, channel_id, size as u32, &pfns)?;
    Ok(OwnedBuf {
        ptr,
        len: size,
        gpadl,
    })
}
