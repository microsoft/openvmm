// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module defines the crash dump protocol.

use bitfield_struct::bitfield;
use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;

pub const CRASHDUMP_GUID: Guid = Guid::from_static_str("427b03e7-4ceb-4286-b5fc-486f4a1dd439");

/// Capabilities supported by the host crash dump services
#[bitfield(u64)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
pub struct Capabilities {
    pub windows_config_v1: bool,
    pub linux_config_v1: bool,
    #[bits(62)]
    pub reserved: u64,
}

open_enum::open_enum! {
    /// Dump types
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum DumpType: u32 {
        NONE = 0x00000000,
        ELF = 0x00000001,
        KDUMP = 0x00000002,
    }
}

/// Crash dump configuration.
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct ConfigV1 {
    pub max_dump_size: u64,
    pub dump_type: DumpType,
}

/// Dump completion information
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct CompletionInfoV1 {
    pub major_version: u32,
    pub minor_version: u32,
    pub version_banner: [u8; 256],
    pub vtl: u8,
}

//
// Protocol messages are packaged in request/response packets
// The format of these packets is defined below.
//

open_enum::open_enum! {
    /// Message types
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum MessageType : u64 {
        INVALID = 0, // The default invalid type

        // Request Types
        REQUEST_GET_CAPABILITIES_V1 = 0x00000001,
        REQUEST_GET_WINDOWS_DUMP_CONFIG_V1 = 0x00000002,
        REQUEST_WINDOWS_DUMP_START_V1 = 0x00000003,
        REQUEST_WINDOWS_DUMP_WRITE_V1 = 0x00000004,
        REQUEST_WINDOWS_DUMP_COMPLETE_V1 = 0x00000005,
        REQUEST_GET_NIX_DUMP_CONFIG_V1 = 0x00000102,
        REQUEST_NIX_DUMP_START_V1 = 0x00000103,
        REQUEST_NIX_DUMP_WRITE_V1 = 0x00000104,
        REQUEST_NIX_DUMP_COMPLETE_V1 = 0x00000105,

        // Response Types
        RESPONSE_GET_CAPABILITIES_V1 = 0x00010001,
        RESPONSE_GET_WINDOWS_DUMP_CONFIG_V1 = 0x00010002,
        RESPONSE_WINDOWS_DUMP_START_V1 = 0x00010003,
        RESPONSE_WINDOWS_DUMP_WRITE_V1 = 0x00010004,
        RESPONSE_WINDOWS_DUMP_COMPLETE_V1 = 0x00010005,
        RESPONSE_GET_NIX_DUMP_CONFIG_V1 = 0x00010102,
        RESPONSE_NIX_DUMP_START_V1 = 0x00010103,
        RESPONSE_NIX_DUMP_WRITE_V1 = 0x00010104,
        RESPONSE_NIX_DUMP_COMPLETE_V1 = 0x00010105,
    }
}

/// Common message header for all requests and responses.
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Header {
    /// Correlates messages across guest/host
    pub activity_id: Guid,
    pub message_type: MessageType,
}

/// Complete message payload for ResponseGetCapabilities_v1
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct DumpCapabilitiesRequestV1 {
    pub header: Header,
}

/// Complete message payload for ResponseGetCapabilities_v1
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct DumpCapabilitiesResponseV1 {
    pub header: Header,
    pub capabilities: Capabilities,
}

/// Complete message payload for RequestGetNixDumpConfig_v1
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct DumpConfigRequestV1 {
    pub header: Header,
}

/// Complete message payload for ResponseGetNixDumpConfig_v1
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct DumpConfigResponseV1 {
    pub header: Header,
    pub config: ConfigV1,
}

/// Complete message payload for RequestGetNixDumpConfig_v1
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct DumpStartRequestV1 {
    pub header: Header,
}

/// Complete message payload for ResponseGetNixDumpConfig_v1
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct DumpStartResponseV1 {
    pub header: Header,
    /// HRESULT return by the host vdev.
    pub status: i32,
}

/// Complete message payload for RequestNixDumpWrite_v1
/// Data follows in a separate message with no headers.
#[derive(
    Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned,
)]
#[repr(C, packed)]
pub struct DumpWriteRequestV1 {
    pub header: Header,
    pub offset: u64,
    pub size: u32,
}

/// Response to a RequestNixDumpWrite_v1
/// A response is only sent if an error has occurred.
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct DumpWriteResponseV1 {
    pub header: Header,
    /// HRESULT returned by the host vdev.
    pub status: i32,
}

/// Completes a Nix crash dump
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct DumpCompleteRequestV1 {
    pub header: Header,
    pub info: CompletionInfoV1,
}
