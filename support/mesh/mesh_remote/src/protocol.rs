// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use zerocopy::AsBytes;
use zerocopy::FromBytes;
use zerocopy::FromZeroes;

#[repr(C)]
#[derive(AsBytes, FromBytes, FromZeroes)]
pub struct PacketHeader {
    pub packet_type: PacketType,
    pub reserved: [u8; 7],
}

#[cfg(unix)] // Only used for unix nodes
#[repr(C)]
#[derive(AsBytes, FromBytes, FromZeroes)]
pub struct ReleaseFds {
    pub header: PacketHeader,
    pub count: u64,
}

open_enum::open_enum! {
    #[derive(AsBytes, FromBytes, FromZeroes)]
   pub enum PacketType: u8 {
        EVENT = 1,
        RELEASE_FDS = 2,
        LARGE_EVENT = 3,
    }
}
