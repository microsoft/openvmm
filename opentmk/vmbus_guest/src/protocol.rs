// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-side VMBus wire types that do not live in `vmbus_core::protocol`.
//!
//! Two categories of definitions:
//!
//! 1. Ring-buffer packet descriptors (`PacketDescriptor`,
//!    `GpaDirectHeader`, `TransferPageHeader`, `TransferPageRange`,
//!    `GpaRange`). Wire-equivalent to their `vmbus_ring` counterparts
//!    but expose typed `PacketType`/`PacketFlags` (upstream stores raw
//!    `u16`) and use guest-side field names (`byte_count`/`byte_offset`
//!    vs upstream `len`/`offset`). These will fold away in a follow-up
//!    that adopts `vmbus_ring::IncomingRing`/`OutgoingRing` directly.
//! 2. Hypercall-adjacent constants (`HV_MESSAGE_TYPE_CHANNEL`,
//!    `VMBUS_CONNECTION_ID_LEGACY`, `VMBUS_CONNECTION_ID_MODERN`) and
//!    guest conveniences ([`NEGOTIATION_LADDER`],
//!    [`supported_feature_flags`], [`version_raw`]) that have no
//!    authoritative home upstream.
//!
//! Channel-manager wire types (`MessageHeader`, `MessageType`,
//! `VmbusMessage`, `Version`, `InitiateContact*`, `VersionResponse*`,
//! `OfferChannel`, `OpenChannel*`, `OpenResult`, `Gpadl*`, `Close*`,
//! `Modify*`, `Tl*`, `Unload*`, `RequestOffers`, `AllOffersDelivered`,
//! `RelIdReleased`, `RescindChannelOffer`, `TargetInfo`, `ChannelId`,
//! `GpadlId`, `FeatureFlags`, `OpenChannelFlags`,
//! `HvsockUserDefinedParameters`, `HvsockParametersVersion`,
//! `UserDefinedData`, `ConnectionState`, `HEADER_SIZE`,
//! `MAX_MESSAGE_SIZE`, and the `STATUS_*` constants) come directly
//! from [`vmbus_core::protocol`]. `Guid` comes from the workspace
//! [`guid`] crate.

#![expect(missing_docs)]

use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::Version;

// -- Guest-side hypercall constants -----------------------------------------

/// The Hyper-V message type for a vmbus channel-manager SIMP delivery.
///
/// The synic APIs identify vmbus messages by this constant.
pub const HV_MESSAGE_TYPE_CHANNEL: u32 = 1;

/// Legacy vmbus channel-manager connection id (single-client hosts).
pub const VMBUS_CONNECTION_ID_LEGACY: u32 = 1;

/// Modern (multi-client) vmbus channel-manager connection id.
pub const VMBUS_CONNECTION_ID_MODERN: u32 = 4;

// -- Guest-side conveniences over the re-exported types ---------------------

/// Guest-side extension: the version ladder we try when connecting, in
/// preferred-first order. Upstream tracks the same set as
/// `SUPPORTED_VERSIONS` in `vmbus_client`, but that constant is
/// private and reversed; this ordering lets the guest walk newer →
/// older on `VersionResponse.version_supported == 0`.
pub const NEGOTIATION_LADDER: &[Version] = &[
    Version::Copper,
    Version::Iron,
    Version::Win10Rs5,
    Version::Win10Rs4,
    Version::Win10Rs3_1,
    Version::Win10,
    Version::Win8_1,
    Version::Win8,
];

/// Guest-side extension: the feature flags this driver advertises to
/// the host in an `InitiateContact2`. Kept as a free function so the
/// [`FeatureFlags`] type stays unaltered by the guest.
pub fn supported_feature_flags() -> FeatureFlags {
    FeatureFlags::new()
        .with_guest_specified_signal_parameters(true)
        .with_channel_interrupt_redirection(true)
        .with_modify_connection(true)
        .with_client_id(true)
}

/// Guest-side extension: raw wire encoding of a [`Version`].
pub fn version_raw(v: Version) -> u32 {
    v as u32
}

// -- Ring-buffer packet descriptors with typed accessors ---------------------
//
// Upstream's `vmbus_ring::protocol::PacketDescriptor` uses raw `u16` for
// `packet_type` and `flags`. The wire layout is identical; the local
// versions below add named `PacketType` variants and a `PacketFlags`
// bitfield so guest-side dispatch code can match on typed values instead
// of magic numbers. The extended-header structs
// (`GpaDirectHeader`, `TransferPageHeader`, `TransferPageRange`) contain
// no typed fields and could re-export upstream directly, but staying
// local avoids importing partial upstream layouts alongside these local
// convenience types.

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

open_enum! {
    /// Ring-buffer packet types.
    ///
    /// Unknown values round-trip losslessly per the OpenVMM guidance
    /// on protocol enums.
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

/// Ring-buffer packet flags.
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
/// Wire-equivalent to `vmbus_ring::protocol::PacketDescriptor`; the
/// only difference is that `packet_type` and `flags` are exposed as the
/// typed [`PacketType`] and [`PacketFlags`] instead of raw `u16` for
/// ergonomics in guest-side dispatch code.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct PacketDescriptor {
    pub packet_type: PacketType,
    /// Offset from the start of the descriptor to the payload, in units
    /// of 8 bytes.
    pub data_offset8: u16,
    /// Total length of the packet including the descriptor, in units of
    /// 8 bytes.
    pub length8: u16,
    pub flags: PacketFlags,
    /// Correlator returned in the corresponding completion packet.
    pub transaction_id: u64,
}

/// Extended header for `VM_PKT_DATA_USING_GPA_DIRECT` packets. Followed
/// by `range_count` [`GpaRange`]s each followed by their PFN list.
///
/// Matches `vmbus_ring::protocol::GpaDirectHeader` on the wire.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpaDirectHeader {
    /// Reserved — may carry garbage on receive; must be zero on send.
    pub reserved: u32,
    /// Number of `GpaRange` records that follow.
    pub range_count: u32,
}

/// Extended header for `VM_PKT_DATA_USING_XFER_PAGES` packets. Followed
/// by `range_count` [`TransferPageRange`] records.
///
/// Matches `vmbus_ring::protocol::TransferPageHeader` on the wire.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TransferPageHeader {
    /// Identifies the transfer-page set (recv buffer).
    pub transfer_page_set_id: u16,
    /// Reserved — may carry garbage.
    pub reserved: u16,
    /// Number of `TransferPageRange` records that follow.
    pub range_count: u32,
}

/// One entry in a `VM_PKT_DATA_USING_XFER_PAGES` packet describing
/// where in the recv buffer the host wrote a single sub-message.
///
/// Matches `vmbus_ring::TransferPageRange` on the wire.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TransferPageRange {
    /// Length of the sub-message in bytes.
    pub byte_count: u32,
    /// Offset from the start of the recv buffer where the sub-message lives.
    pub byte_offset: u32,
}

/// GPA range as it appears on the wire for `VM_PKT_DATA_USING_GPA_DIRECT`
/// and inside `GpadlHeader` bodies.
///
/// Wire-equivalent to `vmbus_ring::gparange::GpaRange`; guest-local
/// only because the field names (`byte_count`/`byte_offset`) differ
/// from upstream (`len`/`offset`).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GpaRange {
    pub byte_count: u32,
    pub byte_offset: u32,
    // Followed by a variable number of PFNs (`[u64; N]`) on the wire.
}
