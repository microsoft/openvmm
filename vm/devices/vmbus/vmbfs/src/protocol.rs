// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use bitfield_struct::bitfield;
use guid::Guid;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::little_endian::U64 as u64_le;

pub const INTERFACE_TYPE: Guid = guid::guid!("c376c1c3-d276-48d2-90a9-c04748072c60");
pub const IMC_INSTANCE: Guid = guid::guid!("c4e5e7d1-d748-4afc-979d-683167910a55");
pub const _BOOT_INSTANCE: Guid = guid::guid!("c63c9bdf-5fa5-4208-b03f-6b458b365592");

pub const MAX_MESSAGE_SIZE: usize = 12288;
pub const MAX_READ_SIZE: usize =
    MAX_MESSAGE_SIZE - size_of::<MessageHeader>() - size_of::<ReadFileResponse>();

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum Version: u32 {
        WIN10 = 0x00010000,
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum MessageType: u32 {
        INVALID = 0,
        VERSION_REQUEST = 1,
        VERSION_RESPONSE = 2,
        GET_FILE_INFO_REQUEST = 3,
        GET_FILE_INFO_RESPONSE = 4,
        READ_FILE_REQUEST = 5,
        READ_FILE_RESPONSE = 6,
        READ_FILE_RDMA_REQUEST = 7,
        READ_FILE_RDMA_RESPONSE = 8,
    }
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FileInfoFlags {
    pub directory: bool,
    pub rdma_capable: bool,
    #[bits(30)]
    _reserved: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageHeader {
    pub message_type: MessageType,
    pub reserved: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VersionRequest {
    pub requested_version: Version,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum VersionStatus: u32 {
        SUPPORTED = 0,
        UNSUPPORTED = 1,
    }
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VersionResponse {
    pub status: VersionStatus,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetFileInfoRequest {
    // Followed by a UTF-16 file path.
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum Status: u32 {
        SUCCESS = 0,
        NOT_FOUND = 1,
        END_OF_FILE = 2,
        ERROR = 3,
    }
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetFileInfoResponse {
    pub status: Status,
    pub flags: FileInfoFlags,
    pub file_size: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadFileRequest {
    pub byte_count: u32,
    pub offset: u64_le,
    // Followed by a UTF-16 file path.
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadFileResponse {
    pub status: Status,
    // Followed by the data.
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadFileRdmaRequest {
    pub handle: u32,
    pub byte_count: u32,
    pub file_offset: u64,
    pub token_offset: u64,
    // Followed by a UTF-16 file path.
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadFileRdmaResponse {
    pub status: Status,
    pub byte_count: u32,
}
