// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VersionInfo;
use bitfield_struct::bitfield;
use hvdef::Vtl;
use inspect::Inspect;
use mesh::payload::Protobuf;
use open_enum::open_enum;
use std::mem::size_of;
use std::ops::BitAnd;
use std::ops::BitAndAssign;
use std::ops::BitOr;
use std::ops::Deref;
use std::ops::DerefMut;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unalign;

#[macro_use]
mod macros;

type Guid = guid::Guid;

pub const VMBUS_MESSAGE_REDIRECT_CONNECTION_ID: u32 = 0x800074;

pub const STATUS_SUCCESS: i32 = 0;
pub const STATUS_UNSUCCESSFUL: i32 = 0x8000ffff_u32 as i32;
pub const STATUS_CONNECTION_REFUSED: i32 = 0xc0000236_u32 as i32;

pub const HEADER_SIZE: usize = size_of::<MessageHeader>();
pub const MAX_MESSAGE_SIZE: usize = hvdef::HV_MESSAGE_PAYLOAD_SIZE;

// This macro is used to define a MessageType open enum, a Message enum, a parse method for the
// Message enum, and VmbusMessage trait implementations for each protocol message struct.
//
// The syntax here is as follows:
// number name { struct min_version [options],* },*
//
// If a message has different variants depending on the version or feature flags, you can express
// this by having multiple comma-separated items inside the curly braces for that message. List the
// variants in the order you want them to be matched (so, newer first).
//
// A message that can be received when disconnected should have the min_version set to 0.
//
// The following additional options can be set:
// - features: specifies one or more feature flags, at least one of which must be supported for the
//             message to be allowed.
// - check_size: set to true to only match the message if its size is at least the size of the
//               struct; if it's not, allow another message to match. Without this option, the size
//               is still checked but a message that is too small is considered a parsing failure,
//               and won't allow another match. Use this for a message whose variants can only be
//               distinguished by size.
vmbus_messages! {
    pub enum Message, MessageType {
        1 OFFER_CHANNEL { OfferChannel V1 },
        2 RESCIND_CHANNEL_OFFER { RescindChannelOffer V1 },
        3 REQUEST_OFFERS { RequestOffers V1 },
        4 ALL_OFFERS_DELIVERED { AllOffersDelivered V1 },
        5 OPEN_CHANNEL {
            OpenChannel2 Copper features:(guest_specified_signal_parameters | channel_interrupt_redirection),
            OpenChannel V1
        },
        6 OPEN_CHANNEL_RESULT { OpenResult V1 },
        7 CLOSE_CHANNEL { CloseChannel V1 },
        8 GPADL_HEADER { GpadlHeader V1 },
        9 GPADL_BODY { GpadlBody V1 },
        10 GPADL_CREATED { GpadlCreated V1 },
        11 GPADL_TEARDOWN { GpadlTeardown V1 },
        12 GPADL_TORNDOWN { GpadlTorndown V1 },
        13 REL_ID_RELEASED { RelIdReleased V1 },
        14 INITIATE_CONTACT {
            // Although the InitiateContact2 message is only used in Copper and above, it
            // must be set as minimum version 0 because the version is not known when the message
            // is received. For this same reason, we can't check the feature flags here.
            InitiateContact2 0 check_size:true,
            InitiateContact 0
        },
        15 VERSION_RESPONSE {
            VersionResponse3 0 check_size:true,
            VersionResponse2 0 check_size:true,
            VersionResponse 0
        },
        16 UNLOAD { Unload V1 },
        17 UNLOAD_COMPLETE { UnloadComplete Win7 },
        18 OPEN_RESERVED_CHANNEL { OpenReservedChannel Win10 },
        19 CLOSE_RESERVED_CHANNEL { CloseReservedChannel 0 },
        20 CLOSE_RESERVED_RESPONSE { CloseReservedChannelResponse Win10 },
        21 TL_CONNECT_REQUEST {
            // Some clients send the old message even for newer protocols, so check the size to allow
            // the old version to match if it's smaller.
            TlConnectRequest2 Win10Rs5 check_size:true,
            TlConnectRequest Win10
        },
        22 MODIFY_CHANNEL { ModifyChannel Win10Rs3_0 },
        23 TL_CONNECT_REQUEST_RESULT { TlConnectResult Win10Rs3_0 },
        24 MODIFY_CHANNEL_RESPONSE { ModifyChannelResponse Iron },
        25 MODIFY_CONNECTION { ModifyConnection Copper features:modify_connection },
        26 MODIFY_CONNECTION_RESPONSE { ModifyConnectionResponse Copper features:modify_connection },
        27 PAUSE { Pause Copper features:pause_resume },
        28 PAUSE_RESPONSE { PauseResponse Copper features:pause_resume },
        29 RESUME { Resume Copper features:pause_resume },
    }
}

/// An error that occurred while parsing a vmbus protocol message.
#[derive(Debug, Error)]
pub enum ParseError {
    /// The message was smaller than required for the message type.
    #[error("message too small: {0:?}")]
    MessageTooSmall(Option<MessageType>),
    /// The message type is not a valid vmbus protocol message, or a message that is not supported
    /// with the current protocol version.
    #[error("unexpected or unsupported message type: {0:?}")]
    InvalidMessageType(MessageType),
}

/// Trait implemented on all protocol message structs by the vmbus_message! macro.
pub trait VmbusMessage: Sized {
    /// The corresponding message type for the struct.
    const MESSAGE_TYPE: MessageType;

    /// The size of the message, including the vmbus message header.
    const MESSAGE_SIZE: usize = HEADER_SIZE + size_of::<Self>();
}

/// The header of a vmbus message.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MessageHeader {
    message_type: MessageType,
    padding: u32,
}

impl MessageHeader {
    /// Creates a new `MessageHeader` for the specified message type.
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

#[derive(Inspect)]
#[bitfield(u32)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
pub struct FeatureFlags {
    /// Feature which allows the guest to specify an event flag and connection ID when opening
    /// a channel. If not used, the event flag defaults to the channel ID and the connection ID
    /// is specified by the host in the offer channel message.
    pub guest_specified_signal_parameters: bool, // 0x1

    /// Indicates the `REDIRECT_INTERRUPT` flag is supported in the OpenChannel flags.
    pub channel_interrupt_redirection: bool, // 0x2

    /// Indicates the `MODIFY_CONNECTION` and `MODIFY_CONNECTION_RESPONSE` messages are supported.
    pub modify_connection: bool, // 0x4

    /// Feature which allows a client (Windows, Linux, MiniVMBus, etc)
    /// to specify a well-known GUID to identify itself when initiating contact.
    /// If not used, the client ID is zero.
    pub client_id: bool, // 0x8

    /// Indicates the `confidential_ring_buffer` and `confidential_external_memory` offer flags are
    /// supported.
    pub confidential_channels: bool, // 0x10

    /// The server supports messages to pause and resume additional control messages.
    pub pause_resume: bool, // 0x20

    /// The guest supports having the server (host or paravisor) provide monitor page GPAs.
    ///
    /// If this flag is present in the `InitiateContact` message, the guest may still provide its
    /// own monitor pages, which the server may ignore if it supports the flag. The server will
    /// only set this flag in the `VersionResponse` message if it is actually providing monitor
    /// pages, which the guest must then use instead of its own.
    ///
    /// If the server sets the flag in the `VersionResponse` message, it must provide a non-zero
    /// value for the [`VersionResponse3::child_to_parent_monitor_page_gpa`]; the
    /// [`VersionResponse3::parent_to_child_monitor_page_gpa`] is optional and may be zero, in which
    /// case the guest cannot cancel MNF interrupts from the host.
    pub server_specified_monitor_pages: bool, // 0x40

    #[bits(25)]
    _reserved: u32,
}

impl FeatureFlags {
    /// Returns true if `other` contains only flags that are also set in `self`.
    pub fn contains(&self, other: FeatureFlags) -> bool {
        self.into_bits() & other.into_bits() == other.into_bits()
    }
}

impl BitAnd for FeatureFlags {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        (self.into_bits() & rhs.into_bits()).into()
    }
}

impl BitAndAssign for FeatureFlags {
    fn bitand_assign(&mut self, rhs: Self) {
        *self = (self.into_bits() & rhs.into_bits()).into()
    }
}

impl BitOr for FeatureFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        (self.into_bits() | rhs.into_bits()).into()
    }
}

#[repr(transparent)]
#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    IntoBytes,
    FromBytes,
    Immutable,
    KnownLayout,
    Protobuf,
)]
#[mesh(package = "vmbus")]
pub struct GpadlId(pub u32);

#[repr(transparent)]
#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    Inspect,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    IntoBytes,
    FromBytes,
    Immutable,
    KnownLayout,
    Protobuf,
)]
#[inspect(transparent)]
pub struct ChannelId(pub u32);

pub struct ConnectionId(pub u32);

impl ConnectionId {
    /// Format a connection ID for a given channel.
    pub fn new(channel_id: u32, vtl: Vtl, sint: u8) -> Self {
        Self(channel_id | (sint as u32) << 12 | (vtl as u32) << 16)
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct InitiateContact {
    pub version_requested: u32,
    pub target_message_vp: u32,
    pub interrupt_page_or_target_info: u64, // sint, vtl, _
    pub parent_to_child_monitor_page_gpa: u64,
    pub child_to_parent_monitor_page_gpa: u64,
}

/// Initiate contact message used with `FeatureFlags::CLIENT_ID` when the feature is supported
/// (Copper and above).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct InitiateContact2 {
    pub initiate_contact: InitiateContact,
    pub client_id: Guid,
}

impl From<InitiateContact> for InitiateContact2 {
    fn from(value: InitiateContact) -> Self {
        Self {
            initiate_contact: value,
            ..FromZeros::new_zeroed()
        }
    }
}

/// Helper struct to interpret the `InitiateContact::interrupt_page_or_target_info` field.
#[bitfield(u64)]
pub struct TargetInfo {
    pub sint: u8,
    pub vtl: u8,
    pub _padding: u16,
    pub feature_flags: u32,
}

pub const fn make_version(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | (minor as u32)
}

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Inspect)]
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

open_enum! {
    /// Possible values for the `VersionResponse::connection_state` field.
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

/// Version response message used by `Version::Copper` and above.
/// N.B. The server will only send this version if the requested version is `Version::Copper` or
///      above and the version is supported. For unsupported versions, the original `VersionResponse`
///      is always sent.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VersionResponse2 {
    pub version_response: VersionResponse,
    pub supported_features: u32,
}

/// Version response message used by [`Version::Copper`] and above if
/// [`FeatureFlags::server_specified_monitor_pages`] is set.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VersionResponse3 {
    pub version_response2: VersionResponse2,
    pub _padding: u32,
    // Only valid with `FeatureFlags::server_specified_monitor_pages`.
    pub parent_to_child_monitor_page_gpa: u64,
    pub child_to_parent_monitor_page_gpa: u64,
}

impl From<VersionResponse> for VersionResponse2 {
    fn from(value: VersionResponse) -> Self {
        Self {
            version_response: value,
            ..FromZeros::new_zeroed()
        }
    }
}

impl From<VersionResponse2> for VersionResponse3 {
    fn from(value: VersionResponse2) -> Self {
        Self {
            version_response2: value,
            ..FromZeros::new_zeroed()
        }
    }
}

impl From<VersionResponse> for VersionResponse3 {
    fn from(value: VersionResponse) -> Self {
        let version_response: VersionResponse2 = value.into();
        version_response.into()
    }
}

/// User-defined data provided by a device as part of an offer or open request.
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    IntoBytes,
    FromBytes,
    Immutable,
    KnownLayout,
    Protobuf,
    Inspect,
)]
#[repr(C, align(4))]
#[mesh(transparent)]
#[inspect(transparent)]
pub struct UserDefinedData([u8; 120]);

impl UserDefinedData {
    pub fn as_pipe_params(&self) -> &PipeUserDefinedParameters {
        PipeUserDefinedParameters::ref_from_bytes(
            &self.0[0..size_of::<PipeUserDefinedParameters>()],
        )
        .expect("from bytes should not fail")
    }

    pub fn as_pipe_params_mut(&mut self) -> &mut PipeUserDefinedParameters {
        PipeUserDefinedParameters::mut_from_bytes(
            &mut self.0[0..size_of::<PipeUserDefinedParameters>()],
        )
        .expect("from bytes should not fail")
    }

    pub fn as_hvsock_params(&self) -> &HvsockUserDefinedParameters {
        HvsockUserDefinedParameters::ref_from_bytes(
            &self.0[0..size_of::<HvsockUserDefinedParameters>()],
        )
        .expect("from bytes should not fail")
    }

    pub fn as_hvsock_params_mut(&mut self) -> &mut HvsockUserDefinedParameters {
        HvsockUserDefinedParameters::mut_from_bytes(
            &mut self.0[0..size_of::<HvsockUserDefinedParameters>()],
        )
        .expect("from bytes should not fail")
    }
}

impl Deref for UserDefinedData {
    type Target = [u8; 120];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for UserDefinedData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<[u8; 120]> for UserDefinedData {
    fn from(value: [u8; 120]) -> Self {
        Self(value)
    }
}

impl From<UserDefinedData> for [u8; 120] {
    fn from(value: UserDefinedData) -> Self {
        value.0
    }
}

impl Default for UserDefinedData {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

#[repr(C)]
#[derive(
    Copy, Clone, Debug, Inspect, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout,
)]
#[inspect(extra = "Self::inspect_extra")]
pub struct OfferChannel {
    pub interface_id: Guid,
    pub instance_id: Guid,
    #[inspect(skip)]
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

impl OfferChannel {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        // TODO: There doesn't exist a single crate that has all these interface
        // IDs. Today they're defined in each individual crate, but we don't
        // want to include all those crates as dependencies here.
        //
        // In the future, it might make sense to have a common protocol crate
        // that has all of these defined, but for now just redefine the most
        // common ones here. Add more as needed.
        const SHUTDOWN_IC: Guid = guid::guid!("0e0b6031-5213-4934-818b-38d90ced39db");
        const KVP_IC: Guid = guid::guid!("a9a0f4e7-5a45-4d96-b827-8a841e8c03e6");
        const VSS_IC: Guid = guid::guid!("35fa2e29-ea23-4236-96ae-3a6ebacba440");
        const TIMESYNC_IC: Guid = guid::guid!("9527e630-d0ae-497b-adce-e80ab0175caf");
        const HEARTBEAT_IC: Guid = guid::guid!("57164f39-9115-4e78-ab55-382f3bd5422d");
        const RDV_IC: Guid = guid::guid!("276aacf4-ac15-426c-98dd-7521ad3f01fe");

        const INHERITED_ACTIVATION: Guid = guid::guid!("3375baf4-9e15-4b30-b765-67acb10d607b");

        const NET: Guid = guid::guid!("f8615163-df3e-46c5-913f-f2d2f965ed0e");
        const SCSI: Guid = guid::guid!("ba6163d9-04a1-4d29-b605-72e2ffb1dc7f");
        const VPCI: Guid = guid::guid!("44c4f61d-4444-4400-9d52-802e27ede19f");

        resp.field_with("interface_name", || match self.interface_id {
            SHUTDOWN_IC => "shutdown_ic",
            KVP_IC => "kvp_ic",
            VSS_IC => "vss_ic",
            TIMESYNC_IC => "timesync_ic",
            HEARTBEAT_IC => "heartbeat_ic",
            RDV_IC => "rdv_ic",
            INHERITED_ACTIVATION => "inherited_activation",
            NET => "net",
            SCSI => "scsi",
            VPCI => "vpci",
            _ => "unknown",
        });
    }
}

#[derive(Inspect)]
#[bitfield(u16)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq, Protobuf)]
#[mesh(transparent)]
pub struct OfferFlags {
    pub enumerate_device_interface: bool, // 0x1
    /// Indicates the channel must use an encrypted ring buffer on a hardware-isolated VM.
    pub confidential_ring_buffer: bool, // 0x2
    /// Indicates the channel must use encrypted additional GPADLs and GPA direct ranges on a
    /// hardware-isolated VM.
    pub confidential_external_memory: bool, // 0x4
    #[bits(1)]
    _reserved1: u16,
    pub named_pipe_mode: bool, // 0x10
    #[bits(8)]
    _reserved2: u16,
    pub tlnpi_provider: bool, // 0x2000
    #[bits(2)]
    _reserved3: u16,
}

open_enum! {
    /// Possible values for the `PipeUserDefinedParameters::pipe_type` field.
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum PipeType: u32 {
        BYTE = 0,
        MESSAGE = 4,
    }
}

/// First 4 bytes of user_defined for named pipe offers.
#[repr(C)]
#[derive(Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct PipeUserDefinedParameters {
    pub pipe_type: PipeType,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct HvsockUserDefinedParameters {
    pub pipe_params: PipeUserDefinedParameters,
    pub is_for_guest_accept: u8,
    pub is_for_guest_container: u8,
    pub version: Unalign<HvsockParametersVersion>, // unaligned u32
    pub silo_id: Unalign<Guid>,                    // unaligned Guid
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

open_enum! {
    /// Possible values for the `PipeUserDefinedParameters::pipe_type` field.
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum HvsockParametersVersion: u32 {
        PRE_RS5 = 0,
        RS5 = 1,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RescindChannelOffer {
    pub channel_id: ChannelId,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlHeader {
    pub channel_id: ChannelId,
    pub gpadl_id: GpadlId,
    pub len: u16,
    pub count: u16,
}

impl GpadlHeader {
    /// The maximum number of 64 bit values that fit after the message data.
    pub const MAX_DATA_VALUES: usize = (MAX_MESSAGE_SIZE - Self::MESSAGE_SIZE) / size_of::<u64>();
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlBody {
    pub rsvd: u32,
    pub gpadl_id: GpadlId,
}

impl GpadlBody {
    /// The maximum number of 64 bit values that fit after the message data.
    pub const MAX_DATA_VALUES: usize = (MAX_MESSAGE_SIZE - Self::MESSAGE_SIZE) / size_of::<u64>();
}

#[repr(C)]
#[derive(Copy, Clone, Eq, PartialEq, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlCreated {
    pub channel_id: ChannelId,
    pub gpadl_id: GpadlId,
    pub status: i32,
}

/// Target VP index value that indicates that interrupts should be disabled for the channel.
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

#[bitfield(u16)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
pub struct OpenChannelFlags {
    /// Indicates the host-to-guest interrupt for this channel should be sent to the redirected
    /// VTL and SINT. This has no effect if the server is not using redirection.
    pub redirect_interrupt: bool,

    #[bits(15)]
    pub unused: u16,
}

/// Open channel message used if `FeatureFlags::GUEST_SPECIFIED_SIGNAL_PARAMETERS` or
/// `FeatureFlags::CHANNEL_INTERRUPT_REDIRECTION` is supported.
#[repr(C)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct OpenChannel2 {
    pub open_channel: OpenChannel,

    // Only valid with FeatureFlags::GUEST_SPECIFIED_SIGNAL_PARAMETERS
    pub connection_id: u32,
    pub event_flag: u16,

    // Only valid with FeatureFlags::CHANNEL_INTERRUPT_REDIRECTION
    pub flags: OpenChannelFlags,
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

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct CloseChannel {
    pub channel_id: ChannelId,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RelIdReleased {
    pub channel_id: ChannelId,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlTeardown {
    pub channel_id: ChannelId,
    pub gpadl_id: GpadlId,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpadlTorndown {
    pub gpadl_id: GpadlId,
}

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
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TlConnectRequest {
    pub endpoint_id: Guid,
    pub service_id: Guid,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TlConnectRequest2 {
    pub base: TlConnectRequest,
    pub silo_id: Guid,
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
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TlConnectResult {
    pub endpoint_id: Guid,
    pub service_id: Guid,
    pub status: i32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
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

// The remaining structs are for empty messages, provided to simplify the vmbus_messages! macro and
// to allow for consistent use of the VmbusMessage trait for all messages.

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RequestOffers {}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Unload {}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct UnloadComplete {}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct AllOffersDelivered {}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Pause;

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct PauseResponse;

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct Resume;
