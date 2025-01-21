// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// f912ad6d-2b17-48ea-bd65-f927a61c7684
pub const INTERFACE_GUID: Guid = Guid::from_static_str("f912ad6d-2b17-48ea-bd65-f927a61c7684");

// d34b2567-b9b6-42b9-8778-0a4ec0b955bf
pub const INSTANCE_GUID: Guid = Guid::from_static_str("d34b2567-b9b6-42b9-8778-0a4ec0b955bf");

const fn make_version(major: u16, minor: u16) -> u32 {
    (major as u32) << 16 | minor as u32
}
pub const VERSION_WIN8: u32 = make_version(1, 0);

pub const MESSAGE_PROTOCOL_REQUEST: u32 = 1;
pub const MESSAGE_PROTOCOL_RESPONSE: u32 = 2;
pub const MESSAGE_EVENT: u32 = 3;
pub const MESSAGE_SET_LED_INDICATORS: u32 = 4;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageHeader {
    pub message_type: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageProtocolRequest {
    pub version: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageLedIndicatorsState {
    pub led_flags: u16,
    pub padding: u16,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageProtocolResponse {
    pub accepted: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageKeystroke {
    pub make_code: u16,
    pub padding: u16,
    pub flags: u32,
}

pub const KEYSTROKE_IS_UNICODE: u32 = 1 << 0;
pub const KEYSTROKE_IS_BREAK: u32 = 1 << 1;
pub const KEYSTROKE_IS_E0: u32 = 1 << 2;
pub const KEYSTROKE_IS_E1: u32 = 1 << 3;

pub const MAXIMUM_MESSAGE_SIZE: usize = 256;
