// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IC protocol definitions.

#![allow(dead_code)]

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
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Version {
    /// Major version.
    pub major: u16,
    /// Minor version.
    pub minor: u16,
}

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
    pub status: u32,
    /// Transaction ID; should be matched by response message.
    pub transaction_id: u8,
    /// Message flags.
    pub flags: HeaderFlags,
    /// Reserved -- should be zero.
    pub reserved: [u8; 2],
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

/// Heartbeat component protocol.
pub mod heartbeat {
    use open_enum::open_enum;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    /// Heartbeat message from guest to host.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HeartbeatMessage {
        /// Incrementing sequence counter.
        pub sequence_number: u64,
        /// Current state of the guest.
        pub application_state: ApplicationState,
        /// Reserved.
        pub reserved: [u8; 4],
    }

    open_enum! {
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        /// Current state of guest.
        pub enum ApplicationState: u32 {
            /// Guest is in an unknown state.
            UNKNOWN = 0,
            /// Guest is healthy.
            HEALTHY = 1,
            /// Guest encountered a critical error.
            CRITICAL = 2,
            /// Guest is no longer running.
            STOPPED = 3,
        }
    }
}

/// Protocol for shutdown IC.
pub mod shutdown {
    use crate::Version;
    use bitfield_struct::bitfield;
    use guid::Guid;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    /// The unique vmbus interface ID of the shutdown IC.
    pub const INTERFACE_ID: Guid = Guid::from_static_str("0e0b6031-5213-4934-818b-38d90ced39db");
    /// The unique vmbus instance ID of the shutdown IC.
    pub const INSTANCE_ID: Guid = Guid::from_static_str("b6650ff7-33bc-4840-8048-e0676786f393");

    /// Supported framework versions.
    pub const FRAMEWORK_VERSIONS: &[Version] = &[Version::new(1, 0), Version::new(3, 0)];

    /// Supported message versions.
    pub const SHUTDOWN_VERSIONS: &[Version] = &[
        Version::new(1, 0),
        Version::new(3, 0),
        Version::new(3, 1),
        Version::new(3, 2),
    ];

    /// The message for shutdown initiated from the host.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ShutdownMessage {
        /// The shutdown reason.
        pub reason_code: u32,
        /// The maximum amount of time allotted to the guest to perform the
        /// shutdown.
        pub timeout_secs: u32,
        /// Flags for the shutdown request.
        pub flags: ShutdownFlags,
        /// Friendly text string for the shutdown request.
        pub message: [u8; 2048],
    }

    /// Flags for shutdown.
    #[bitfield(u32)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ShutdownFlags {
        /// Whether the shutdown operation is being forced.
        pub force: bool,
        /// Flag indicating the shutdown behavior is guest restart.
        pub restart: bool,
        /// Flag indicating the shutdown behavior is guest hibernate.
        pub hibernate: bool,
        /// Reserved -- must be zero.
        #[bits(29)]
        _reserved: u32,
    }

    /// Reason code for '[ShutdownMessage]', from Windows SDK.
    pub const SHTDN_REASON_FLAG_PLANNED: u32 = 0x80000000;
}
