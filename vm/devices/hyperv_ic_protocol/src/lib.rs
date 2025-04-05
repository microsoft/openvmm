// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IC protocol definitions.

pub mod heartbeat;
pub mod kvp;
pub mod shutdown;
pub mod timesync;
pub mod vss;

use bitfield_struct::bitfield;
use open_enum::open_enum;
use std::fmt::Display;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Maximum message size between guest and host for IC devices.
pub const MAX_MESSAGE_SIZE: usize = 13312;

/// Protocol version.
#[repr(C)]
#[derive(
    Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct Version {
    /// Major version.
    pub major: u16,
    /// Minor version.
    pub minor: u16,
}

/// Framework version 1.0.
pub const FRAMEWORK_VERSION_1: Version = Version::new(1, 0);
/// Framework version 3.0.
pub const FRAMEWORK_VERSION_3: Version = Version::new(3, 0);

impl Version {
    /// Create a new IC version instance.
    pub const fn new(major: u16, minor: u16) -> Self {
        Version { major, minor }
    }
}

impl Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

open_enum! {
    /// Type of message
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum MessageType: u16 {
        /// Initial version negotiation between host and guest.
        VERSION_NEGOTIATION = 0,
        /// Heartbeat / check if alive.
        HEARTBEAT = 1,
        /// KVP exchange.
        KVP_EXCHANGE = 2,
        /// Request shutdown.
        SHUTDOWN = 3,
        /// Synchronize time.
        TIME_SYNC = 4,
        /// VSS
        VSS = 5,
        /// RDV
        RDV = 6,
        /// Guest interface.
        GUEST_INTERFACE = 7,
        /// VM Session.
        VM_SESSION = 8,
    }
}

/// Common message header for IC messages.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct Header {
    /// Version of the IC framework.
    pub framework_version: Version,
    /// Type of message.
    pub message_type: MessageType,
    /// Version of message content.
    pub message_version: Version,
    /// Size in bytes of the message.
    pub message_size: u16,
    /// Status code used for message response.
    pub status: Status,
    /// Transaction ID; should be matched by response message.
    pub transaction_id: u8,
    /// Message flags.
    pub flags: HeaderFlags,
    /// Reserved -- should be zero.
    pub reserved: [u8; 2],
}

open_enum! {
    /// Status code for a message response.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum Status: u32 {
        /// Message was processed successfully.
        SUCCESS = 0,
        /// There are no more items to process.
        NO_MORE_ITEMS = 0x80070103,
        /// Generic failure.
        FAIL = 0x80004005,
        /// The operation is not supported.
        NOT_SUPPORTED = 0x80070032,
        /// Not found.
        NOT_FOUND = 0x80041002,
    }
}

/// Flags for IC messages.
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HeaderFlags {
    /// Message expects a response.
    pub transaction: bool,
    /// Message is a request.
    pub request: bool,
    /// Message is a response.
    pub response: bool,
    /// Reserved - must be zero.
    #[bits(5)]
    _reserved: u8,
}

/// Version negotiation message.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NegotiateMessage {
    /// The number of supported framework versions, located directly after
    /// this structure.
    pub framework_version_count: u16,
    /// The number of supported message versions, located after the framework
    /// versions.
    pub message_version_count: u16,
    /// Reserved -- must be zero.
    pub reserved: u32,
}
