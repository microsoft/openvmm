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
    pub reserved: [u8; 7],
}

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

open_enum::open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
   pub enum PacketType: u8 {
        EVENT = 1,
        RELEASE_FDS = 2,
        LARGE_EVENT = 3,
    }
}
