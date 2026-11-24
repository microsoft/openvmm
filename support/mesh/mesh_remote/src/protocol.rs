// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PacketHeader {
    pub packet_type: PacketType,
    pub reserved: [u8; 3],
    /// For `EVENT` and `LARGE_EVENT`: how many of the event's file descriptors
    /// were sent ahead of it in `EVENT_FDS` packets. Zero otherwise.
    pub fds_ahead: u32,
}

// Both ends must agree on the header size; `fds_ahead` took reserved bytes.
const _: () = assert!(size_of::<PacketHeader>() == 8);

#[cfg(unix)] // Only used for unix nodes
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReleaseFds {
    pub header: PacketHeader,
    pub count: u64,
}

/// Maximum number of file descriptors that can be passed in a single message.
/// Both ends must agree: the receiver sizes its `SCM_RIGHTS` control buffer for
/// this many, and the sender rejects messages that exceed it.
#[cfg(unix)] // Only used for unix nodes
pub const MAX_FDS_PER_MESSAGE: usize = 64;

/// Maximum number of file descriptors in one event, across the `EVENT_FDS`
/// packets that precede it and the event packet itself. A sanity bound on what
/// a peer can make the receiver hold while it waits for the event.
#[cfg(unix)] // Only used for unix nodes
pub const MAX_FDS_PER_EVENT: usize = 4096;

open_enum::open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
   pub enum PacketType: u8 {
        EVENT = 1,
        RELEASE_FDS = 2,
        LARGE_EVENT = 3,
        /// A header-only packet carrying some of the file descriptors of the
        /// next `EVENT` or `LARGE_EVENT`, for an event with more descriptors
        /// than fit in one message. Unix only.
        EVENT_FDS = 4,
    }
}
