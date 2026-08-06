// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMBus protocol wire types.
//!
//! These types are ported from `vmbus_core::protocol` (see
//! `deps/openvmm/vm/devices/vmbus/vmbus_core/src/protocol.rs`) with all
//! `std` / `mesh` / `inspect` / `guid`-crate dependencies stripped so the
//! module is `#![no_std]`-compatible.
//!
//! Layouts match the authoritative Windows minkernel headers
//! (`VmbusChannelMessages.h`, `VmbusVersions.h`) — see the design doc for
//! citations.
//!
//! When adding a new field or struct, prefer copying the openvmm layout
//! verbatim so the two stay in sync.

#![expect(missing_docs)]

use bitfield_struct::bitfield;
use core::mem::size_of;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unalign;

/// Windows-format GUID, matching the wire layout used by every VMBus
/// message. Kept local to avoid depending on the workspace `guid` crate,
/// which still needs `std`.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, Eq, PartialEq, Hash, IntoBytes, FromBytes, Immutable, KnownLayout,
)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// Size of the vmbus `MessageHeader` in bytes.
pub const HEADER_SIZE: usize = size_of::<MessageHeader>();

/// Maximum message payload size — matches the Hyper-V message payload
/// (`HV_MESSAGE_PAYLOAD_SIZE`).
pub const MAX_MESSAGE_SIZE: usize = hvdef::HV_MESSAGE_PAYLOAD_SIZE;

/// Hyper-V message type used for VMBus messages.
pub const HV_MESSAGE_TYPE_CHANNEL: u32 = 1;

/// Legacy connection ID used before protocol version 5.0.
pub const VMBUS_CONNECTION_ID_LEGACY: u32 = 1;

/// Connection ID used for protocol version 5.0 and above (overridden by
/// the value returned in `VersionResponse::selected_version_or_connection_id`).
pub const VMBUS_CONNECTION_ID_MODERN: u32 = 4;

pub const STATUS_SUCCESS: i32 = 0;
pub const STATUS_UNSUCCESSFUL: i32 = 0x8000ffff_u32 as i32;
pub const STATUS_CONNECTION_REFUSED: i32 = 0xc0000236_u32 as i32;

// ---------------------------------------------------------------------------
// Message type + header
// ---------------------------------------------------------------------------

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum MessageType: u32 {
        INVALID = 0,
        OFFER_CHANNEL = 1,
        RESCIND_CHANNEL_OFFER = 2,
        REQUEST_OFFERS = 3,
        ALL_OFFERS_DELIVERED = 4,
        OPEN_CHANNEL = 5,
        OPEN_CHANNEL_RESULT = 6,
        CLOSE_CHANNEL = 7,
        GPADL_HEADER = 8,
        GPADL_BODY = 9,
        GPADL_CREATED = 10,
        GPADL_TEARDOWN = 11,
        GPADL_TORNDOWN = 12,
        REL_ID_RELEASED = 13,
        INITIATE_CONTACT = 14,
        VERSION_RESPONSE = 15,
        UNLOAD = 16,
        UNLOAD_COMPLETE = 17,
        OPEN_RESERVED_CHANNEL = 18,
        CLOSE_RESERVED_CHANNEL = 19,
        CLOSE_RESERVED_RESPONSE = 20,
        TL_CONNECT_REQUEST = 21,
        MODIFY_CHANNEL = 22,
        TL_CONNECT_RESULT = 23,
        MODIFY_CHANNEL_RESPONSE = 24,
        MODIFY_CONNECTION = 25,
        MODIFY_CONNECTION_RESPONSE = 26,
        PAUSE = 27,
        PAUSE_RESPONSE = 28,
        RESUME = 29,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MessageHeader {
    pub message_type: MessageType,
    pub padding: u32,
}

impl MessageHeader {
    pub fn new(message_type: MessageType) -> Self {
        Self {
            message_type,
            padding: 0,
        }
    }

    pub fn message_type(&self) -> MessageType {
        self.message_type
    }
}

/// Marker trait implemented by every wire message. Used by the encode
/// helpers in [`crate::message`].
pub trait VmbusMessage: Sized + IntoBytes + Immutable {
    const MESSAGE_TYPE: MessageType;
    const MESSAGE_SIZE: usize = HEADER_SIZE + size_of::<Self>();
}

// ---------------------------------------------------------------------------
// Feature flags + protocol version
// ---------------------------------------------------------------------------

#[bitfield(u32)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
pub struct FeatureFlags {
    /// 0x1 — guest may specify event flag & connection ID in `OpenChannel2`.
    pub guest_specified_signal_parameters: bool,
    /// 0x2 — `REDIRECT_INTERRUPT` flag supported in `OpenChannel2`.
    pub channel_interrupt_redirection: bool,
    /// 0x4 — `MODIFY_CONNECTION` / `MODIFY_CONNECTION_RESPONSE` supported.
    pub modify_connection: bool,
    /// 0x8 — guest may identify itself with a GUID in `InitiateContact2`.
    pub client_id: bool,
    /// 0x10 — encrypted ring buffers (confidential VMBus).
    pub confidential_channels: bool,
    /// 0x20 — pause/resume supported (openvmm-only today).
    pub pause_resume: bool,
    /// 0x40 — server-provided monitor pages.
    pub server_specified_monitor_pages: bool,
    #[bits(25)]
    _reserved: u32,
}

impl FeatureFlags {
    pub fn contains(&self, other: FeatureFlags) -> bool {
        self.into_bits() & other.into_bits() == other.into_bits()
    }

    /// Flags this guest advertises by default (Copper + CLIENT_ID).
    pub fn supported() -> Self {
        Self::new()
            .with_guest_specified_signal_parameters(true)
            .with_channel_interrupt_redirection(true)
            .with_modify_connection(true)
            .with_client_id(true)
    }
}

pub const fn make_version(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | (minor as u32)
}

/// The set of vmbus protocol versions we recognise. The variants are
/// ordered oldest to newest so `Ord` gives a useful comparison.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Version {
    V1 = make_version(0, 13),
    Win7 = make_version(1, 1),
    Win8 = make_version(2, 4),
    Win8_1 = make_version(3, 0),
    Win10 = make_version(4, 0),
    Win10Rs3_0 = make_version(4, 1),
    Win10Rs3_1 = make_version(5, 0),
    Win10Rs4 = make_version(5, 1),
    Win10Rs5 = make_version(5, 2),
    Iron = make_version(5, 3),
    Copper = make_version(6, 0),
}

impl Version {
    /// Version-negotiation ladder in the order we attempt it.
    ///
    /// See §3 of `tasks/vmbus-port-design.md`.
    pub const NEGOTIATION_LADDER: &'static [Version] = &[
        Version::Copper,
        Version::Iron,
        Version::Win10Rs5,
        Version::Win10Rs4,
        Version::Win10Rs3_1,
        Version::Win10,
        Version::Win8_1,
        Version::Win8,
    ];

    pub fn raw(self) -> u32 {
        self as u32
    }
}

// ---------------------------------------------------------------------------
// ID newtypes
// ---------------------------------------------------------------------------

#[repr(transparent)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    IntoBytes,
    FromBytes,
    Immutable,
    KnownLayout,
)]
pub struct ChannelId(pub u32);

#[repr(transparent)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    IntoBytes,
    FromBytes,
    Immutable,
    KnownLayout,
)]
pub struct GpadlId(pub u32);

/// Composite connection id used when signalling the host on Copper+.
pub struct ConnectionId(pub u32);

impl ConnectionId {
    pub fn new(channel_id: u32, vtl: hvdef::Vtl, sint: u8) -> Self {
        Self(channel_id | (sint as u32) << 12 | (vtl as u8 as u32) << 16)
    }
}

// ---------------------------------------------------------------------------
// InitiateContact / VersionResponse
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct InitiateContact {
    pub version_requested: u32,
    pub target_message_vp: u32,
    pub interrupt_page_or_target_info: u64,
    pub parent_to_child_monitor_page_gpa: u64,
    pub child_to_parent_monitor_page_gpa: u64,
}

impl VmbusMessage for InitiateContact {
    const MESSAGE_TYPE: MessageType = MessageType::INITIATE_CONTACT;
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct InitiateContact2 {
    pub initiate_contact: InitiateContact,
    pub client_id: Guid,
}

impl VmbusMessage for InitiateContact2 {
    const MESSAGE_TYPE: MessageType = MessageType::INITIATE_CONTACT;
}

impl From<InitiateContact> for InitiateContact2 {
    fn from(value: InitiateContact) -> Self {
        Self {
            initiate_contact: value,
            ..FromZeros::new_zeroed()
        }
    }
}

#[bitfield(u64)]
pub struct TargetInfo {
    pub sint: u8,
    pub vtl: u8,
    pub _padding: u16,
    pub feature_flags: u32,
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum ConnectionState: u8 {
        SUCCESSFUL = 0,
        FAILED_LOW_RESOURCES = 1,
        FAILED_UNKNOWN_FAILURE = 2,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VersionResponse {
    pub version_supported: u8,
    pub connection_state: ConnectionState,
    pub padding: u16,
    pub selected_version_or_connection_id: u32,
}

impl VmbusMessage for VersionResponse {
    const MESSAGE_TYPE: MessageType = MessageType::VERSION_RESPONSE;
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VersionResponse2 {
    pub version_response: VersionResponse,
    pub supported_features: u32,
}

impl VmbusMessage for VersionResponse2 {
    const MESSAGE_TYPE: MessageType = MessageType::VERSION_RESPONSE;
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VersionResponse3 {
    pub version_response2: VersionResponse2,
    pub _padding: u32,
    pub parent_to_child_monitor_page_gpa: u64,
    pub child_to_parent_monitor_page_gpa: u64,
}

impl VmbusMessage for VersionResponse3 {
    const MESSAGE_TYPE: MessageType = MessageType::VERSION_RESPONSE;
}

// ---------------------------------------------------------------------------
// Offer / user-defined
// ---------------------------------------------------------------------------

#[repr(C, align(4))]
#[derive(Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct UserDefinedData(pub [u8; 120]);

impl Default for UserDefinedData {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

impl UserDefinedData {
    pub fn as_hvsock_params(&self) -> &HvsockUserDefinedParameters {
        HvsockUserDefinedParameters::ref_from_bytes(
            &self.0[0..size_of::<HvsockUserDefinedParameters>()],
        )
        .expect("HvsockUserDefinedParameters fits in UserDefinedData")
    }
}

impl core::fmt::Debug for UserDefinedData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.iter().all(|&b| b == 0) {
            write!(f, "UserDefinedData([<all-zeroes>])")
        } else {
            write!(f, "UserDefinedData([")?;
            for byte in &self.0 {
                write!(f, "{:02X}", byte)?;
            }
            write!(f, "])")
        }
    }
}

#[bitfield(u16)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
pub struct OfferFlags {
    pub enumerate_device_interface: bool,
    pub confidential_ring_buffer: bool,
    pub confidential_external_memory: bool,
    #[bits(1)]
    _reserved1: u16,
    pub named_pipe_mode: bool,
    #[bits(8)]
    _reserved2: u16,
    pub tlnpi_provider: bool,
    #[bits(2)]
    _reserved3: u16,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct OfferChannel {
    pub interface_id: Guid,
    pub instance_id: Guid,
    pub rsvd: [u32; 4],
    pub flags: OfferFlags,
    pub mmio_megabytes: u16,
    pub user_defined: UserDefinedData,
    pub subchannel_index: u16,
    pub mmio_megabytes_optional: u16,
    pub channel_id: ChannelId,
    pub monitor_id: u8,
    pub monitor_allocated: u8,
    pub is_dedicated: u16,
    pub connection_id: u32,
}

impl VmbusMessage for OfferChannel {
    const MESSAGE_TYPE: MessageType = MessageType::OFFER_CHANNEL;
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RescindChannelOffer {
    pub channel_id: ChannelId,
}

impl VmbusMessage for RescindChannelOffer {
    const MESSAGE_TYPE: MessageType = MessageType::RESCIND_CHANNEL_OFFER;
}

// ---------------------------------------------------------------------------
// GPADL
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlHeader {
    pub channel_id: ChannelId,
    pub gpadl_id: GpadlId,
    pub len: u16,
    pub count: u16,
}

impl VmbusMessage for GpadlHeader {
    const MESSAGE_TYPE: MessageType = MessageType::GPADL_HEADER;
}

impl GpadlHeader {
    /// Number of 64-bit PFN values that fit after this header inside a
    /// single vmbus message.
    pub const MAX_DATA_VALUES: usize = (MAX_MESSAGE_SIZE - Self::MESSAGE_SIZE) / size_of::<u64>();
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlBody {
    pub rsvd: u32,
    pub gpadl_id: GpadlId,
}

impl VmbusMessage for GpadlBody {
    const MESSAGE_TYPE: MessageType = MessageType::GPADL_BODY;
}

impl GpadlBody {
    pub const MAX_DATA_VALUES: usize = (MAX_MESSAGE_SIZE - Self::MESSAGE_SIZE) / size_of::<u64>();
}

#[repr(C)]
#[derive(Copy, Clone, Eq, PartialEq, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlCreated {
    pub channel_id: ChannelId,
    pub gpadl_id: GpadlId,
    pub status: i32,
}

impl VmbusMessage for GpadlCreated {
    const MESSAGE_TYPE: MessageType = MessageType::GPADL_CREATED;
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlTeardown {
    pub channel_id: ChannelId,
    pub gpadl_id: GpadlId,
}

impl VmbusMessage for GpadlTeardown {
    const MESSAGE_TYPE: MessageType = MessageType::GPADL_TEARDOWN;
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlTorndown {
    pub gpadl_id: GpadlId,
}

impl VmbusMessage for GpadlTorndown {
    const MESSAGE_TYPE: MessageType = MessageType::GPADL_TORNDOWN;
}

// ---------------------------------------------------------------------------
// OpenChannel / CloseChannel / RelIdReleased
// ---------------------------------------------------------------------------

/// Target-VP sentinel that disables per-channel interrupts.
pub const VP_INDEX_DISABLE_INTERRUPT: u32 = u32::MAX;

#[repr(C)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct OpenChannel {
    pub channel_id: ChannelId,
    pub open_id: u32,
    pub ring_buffer_gpadl_id: GpadlId,
    pub target_vp: u32,
    pub downstream_ring_buffer_page_offset: u32,
    pub user_data: UserDefinedData,
}

impl VmbusMessage for OpenChannel {
    const MESSAGE_TYPE: MessageType = MessageType::OPEN_CHANNEL;
}

#[bitfield(u16)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
pub struct OpenChannelFlags {
    pub redirect_interrupt: bool,
    #[bits(15)]
    pub unused: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct OpenChannel2 {
    pub open_channel: OpenChannel,
    pub connection_id: u32,
    pub event_flag: u16,
    pub flags: OpenChannelFlags,
}

impl VmbusMessage for OpenChannel2 {
    const MESSAGE_TYPE: MessageType = MessageType::OPEN_CHANNEL;
}

impl From<OpenChannel> for OpenChannel2 {
    fn from(value: OpenChannel) -> Self {
        Self {
            open_channel: value,
            ..FromZeros::new_zeroed()
        }
    }
}

#[repr(C)]
#[derive(PartialEq, Eq, Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct OpenResult {
    pub channel_id: ChannelId,
    pub open_id: u32,
    pub status: u32,
}

impl VmbusMessage for OpenResult {
    const MESSAGE_TYPE: MessageType = MessageType::OPEN_CHANNEL_RESULT;
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct CloseChannel {
    pub channel_id: ChannelId,
}

impl VmbusMessage for CloseChannel {
    const MESSAGE_TYPE: MessageType = MessageType::CLOSE_CHANNEL;
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RelIdReleased {
    pub channel_id: ChannelId,
}

impl VmbusMessage for RelIdReleased {
    const MESSAGE_TYPE: MessageType = MessageType::REL_ID_RELEASED;
}

// ---------------------------------------------------------------------------
// Reserved channel / modify (struct-only in scaffold)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct OpenReservedChannel {
    pub channel_id: ChannelId,
    pub target_vp: u32,
    pub target_sint: u32,
    pub ring_buffer_gpadl: GpadlId,
    pub downstream_page_offset: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct CloseReservedChannel {
    pub channel_id: ChannelId,
    pub target_vp: u32,
    pub target_sint: u32,
}

#[repr(C)]
#[derive(PartialEq, Eq, Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct CloseReservedChannelResponse {
    pub channel_id: ChannelId,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ModifyChannel {
    pub channel_id: ChannelId,
    pub target_vp: u32,
}

#[repr(C)]
#[derive(PartialEq, Eq, Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ModifyChannelResponse {
    pub channel_id: ChannelId,
    pub status: i32,
}

impl VmbusMessage for ModifyChannelResponse {
    const MESSAGE_TYPE: MessageType = MessageType::MODIFY_CHANNEL_RESPONSE;
}

#[repr(C)]
#[derive(PartialEq, Eq, Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ModifyConnection {
    pub parent_to_child_monitor_page_gpa: u64,
    pub child_to_parent_monitor_page_gpa: u64,
}

#[repr(C)]
#[derive(PartialEq, Eq, Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ModifyConnectionResponse {
    pub connection_state: ConnectionState,
}

// ---------------------------------------------------------------------------
// hv-socket (structs-only pass, see §7 of the design doc)
// ---------------------------------------------------------------------------

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum PipeType: u32 {
        BYTE = 0,
        MESSAGE = 4,
    }
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct PipeUserDefinedParameters {
    pub pipe_type: PipeType,
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum HvsockParametersVersion: u32 {
        PRE_RS5 = 0,
        RS5 = 1,
    }
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct HvsockUserDefinedParameters {
    pub pipe_params: PipeUserDefinedParameters,
    pub is_for_guest_accept: u8,
    pub is_for_guest_container: u8,
    /// Stored unaligned to match Windows wire layout.
    pub version: Unalign<HvsockParametersVersion>,
    /// Stored unaligned to match Windows wire layout.
    pub silo_id: Unalign<Guid>,
    pub _padding: [u8; 2],
}

impl HvsockUserDefinedParameters {
    pub fn new(is_for_guest_accept: bool, is_for_guest_container: bool, silo_id: Guid) -> Self {
        Self {
            pipe_params: PipeUserDefinedParameters {
                pipe_type: PipeType::BYTE,
            },
            is_for_guest_accept: is_for_guest_accept.into(),
            is_for_guest_container: is_for_guest_container.into(),
            version: Unalign::new(HvsockParametersVersion::RS5),
            silo_id: Unalign::new(silo_id),
            _padding: [0; 2],
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TlConnectRequest {
    pub endpoint_id: Guid,
    pub service_id: Guid,
}

impl VmbusMessage for TlConnectRequest {
    const MESSAGE_TYPE: MessageType = MessageType::TL_CONNECT_REQUEST;
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TlConnectRequest2 {
    pub base: TlConnectRequest,
    pub silo_id: Guid,
}

impl VmbusMessage for TlConnectRequest2 {
    const MESSAGE_TYPE: MessageType = MessageType::TL_CONNECT_REQUEST;
}

impl From<TlConnectRequest> for TlConnectRequest2 {
    fn from(value: TlConnectRequest) -> Self {
        Self {
            base: value,
            ..FromZeros::new_zeroed()
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TlConnectResult {
    pub endpoint_id: Guid,
    pub service_id: Guid,
    pub status: i32,
}

impl VmbusMessage for TlConnectResult {
    const MESSAGE_TYPE: MessageType = MessageType::TL_CONNECT_RESULT;
}

// ---------------------------------------------------------------------------
// Empty control messages
// ---------------------------------------------------------------------------

macro_rules! empty_message {
    ($name:ident, $ty:expr) => {
        #[repr(C)]
        #[derive(
            Copy, Clone, Debug, Default, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout,
        )]
        pub struct $name;

        impl VmbusMessage for $name {
            const MESSAGE_TYPE: MessageType = $ty;
        }
    };
}

empty_message!(RequestOffers, MessageType::REQUEST_OFFERS);
empty_message!(AllOffersDelivered, MessageType::ALL_OFFERS_DELIVERED);
empty_message!(Unload, MessageType::UNLOAD);
empty_message!(UnloadComplete, MessageType::UNLOAD_COMPLETE);
empty_message!(Pause, MessageType::PAUSE);
empty_message!(PauseResponse, MessageType::PAUSE_RESPONSE);
empty_message!(Resume, MessageType::RESUME);

// ---------------------------------------------------------------------------
// Packet-layer types (ring buffer)
// ---------------------------------------------------------------------------

open_enum! {
    /// Packet types stored in the ring buffer descriptor header.
    ///
    /// Unknown values must round-trip losslessly per the OpenVMM guidance.
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum PacketType: u16 {
        INVALID = 0x0,
        VM_PKT_ESTABLISH_GPADL = 0x4,
        VM_PKT_TEARDOWN_GPADL = 0x5,
        VM_PKT_DATA_INBAND = 0x6,
        VM_PKT_DATA_USING_XFER_PAGES = 0x7,
        VM_PKT_DATA_USING_GPADL = 0x8,
        VM_PKT_DATA_USING_GPA_DIRECT = 0x9,
        VM_PKT_COMP = 0xB,
    }
}

#[bitfield(u16)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct PacketFlags {
    /// Set when this packet expects a completion.
    pub request_completion: bool,
    #[bits(15)]
    _reserved: u16,
}

/// Descriptor at the head of each ring-buffer packet.
///
/// See `VmbusPacketDescriptor` in Windows minkernel headers.
///
/// **Field order matters** — must match `vmbus_ring::PacketDescriptor`
/// exactly: `packet_type, data_offset8, length8, flags, transaction_id`.
/// Getting `flags` in the wrong position renders every packet
/// unreadable to the host with silent drop.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct PacketDescriptor {
    pub packet_type: PacketType,
    /// Offset from the start of the descriptor to the payload, in units of
    /// 8 bytes.
    pub data_offset8: u16,
    /// Total length of the packet including the descriptor, in units of
    /// 8 bytes.
    pub length8: u16,
    pub flags: PacketFlags,
    /// Correlator returned in the corresponding completion packet.
    pub transaction_id: u64,
}

/// GPA range as it appears on the wire for `VM_PKT_DATA_USING_GPA_DIRECT`
/// and inside `GpadlHeader` bodies.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpaRange {
    pub byte_count: u32,
    pub byte_offset: u32,
    // Followed by a variable number of PFNs (`[u64; N]`) on the wire.
}

/// Extended header sitting between the [`PacketDescriptor`] and the
/// payload for `VM_PKT_DATA_USING_GPA_DIRECT` packets. Followed by
/// `range_count` [`GpaRange`]s each followed by their PFN list.
///
/// Matches `vmbus_ring::GpaDirectHeader` in openvmm.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpaDirectHeader {
    /// Reserved on the wire — may carry garbage on receive per
    /// openvmm's comment; must be zero on send.
    pub reserved: u32,
    /// Number of `GpaRange` records that follow.
    pub range_count: u32,
}

/// Extended header on `VM_PKT_DATA_USING_XFER_PAGES` packets, sitting
/// between the [`PacketDescriptor`] and the payload. Followed by
/// `range_count` [`TransferPageRange`] records.
///
/// Matches `vmbus_ring::TransferPageHeader` in openvmm.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TransferPageHeader {
    /// Identifies the transfer-page set (recv buffer). NVSP hosts
    /// echo the guest-chosen `NETVSC_RECEIVE_BUFFER_ID`.
    pub transfer_page_set_id: u16,
    /// Reserved — may carry garbage.
    pub reserved: u16,
    /// Number of `TransferPageRange` records that follow.
    pub range_count: u32,
}

/// One entry in a `VM_PKT_DATA_USING_XFER_PAGES` packet describing
/// where in the recv buffer the host wrote a single sub-message.
///
/// Matches `vmbus_ring::TransferPageRange` in openvmm.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TransferPageRange {
    /// Length of the sub-message in bytes.
    pub byte_count: u32,
    /// Offset from the start of the recv buffer where the sub-message
    /// lives.
    pub byte_offset: u32,
}
