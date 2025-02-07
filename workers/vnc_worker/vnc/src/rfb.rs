// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use self::packed_nums::*;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[allow(non_camel_case_types)]
mod packed_nums {
    pub type u16_be = zerocopy::U16<zerocopy::BigEndian>;
    pub type u32_be = zerocopy::U32<zerocopy::BigEndian>;
}

// As defined in https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst#handshaking-messages

#[repr(transparent)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ProtocolVersion(pub [u8; 12]);

pub const PROTOCOL_VERSION_33: [u8; 12] = *b"RFB 003.003\n";
pub const PROTOCOL_VERSION_37: [u8; 12] = *b"RFB 003.007\n";
pub const PROTOCOL_VERSION_38: [u8; 12] = *b"RFB 003.008\n";

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Security33 {
    pub padding: [u8; 3],
    pub security_type: u8,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Security37 {
    pub type_count: u8,
    // types: [u8; N]
}

pub const SECURITY_TYPE_INVALID: u8 = 0;
pub const SECURITY_TYPE_NONE: u8 = 1;
pub const SECURITY_TYPE_VNC_AUTHENTICATION: u8 = 2;
pub const SECURITY_TYPE_TIGHT: u8 = 16;
pub const SECURITY_TYPE_VENCRYPT: u8 = 19;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SecurityResult {
    pub status: u32_be,
}

pub const SECURITY_RESULT_STATUS_OK: u32 = 0;
pub const SECURITY_RESULT_STATUS_FAILED: u32 = 1;
pub const SECURITY_RESULT_STATUS_FAILED_TOO_MANY_ATTEMPTS: u32 = 2;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ClientInit {
    pub shared_flag: u8,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ServerInit {
    pub framebuffer_width: u16_be,
    pub framebuffer_height: u16_be,
    pub server_pixel_format: PixelFormat,
    pub name_length: u32_be,
    // name_string: [u8; N],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian_flag: u8,
    pub true_color_flag: u8,
    pub red_max: u16_be,
    pub green_max: u16_be,
    pub blue_max: u16_be,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
    pub padding: [u8; 3],
}

// Client to server messages

pub const CS_MESSAGE_SET_PIXEL_FORMAT: u8 = 0;
pub const CS_MESSAGE_SET_ENCODINGS: u8 = 2;
pub const CS_MESSAGE_FRAMEBUFFER_UPDATE_REQUEST: u8 = 3;
pub const CS_MESSAGE_KEY_EVENT: u8 = 4;
pub const CS_MESSAGE_POINTER_EVENT: u8 = 5;
pub const CS_MESSAGE_CLIENT_CUT_TEXT: u8 = 6;
pub const CS_MESSAGE_QEMU: u8 = 255;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetPixelFormat {
    pub message_type: u8,
    pub padding: [u8; 3],
    pub pixel_format: PixelFormat,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetEncodings {
    pub message_type: u8,
    pub padding: u8,
    pub encoding_count: u16_be,
    // encoding_type: [i32_be; N],
}

pub const ENCODING_TYPE_RAW: u32 = 0;
pub const ENCODING_TYPE_COPY_RECT: u32 = 1;
pub const ENCODING_TYPE_RRE: u32 = 2;
pub const ENCODING_TYPE_CO_RRE: u32 = 4;
pub const ENCODING_TYPE_HEXTILE: u32 = 5;
pub const ENCODING_TYPE_ZLIB: u32 = 6;
pub const ENCODING_TYPE_TIGHT: u32 = 7;
pub const ENCODING_TYPE_ZLIBHEX: u32 = 8;
pub const ENCODING_TYPE_ZRLE: u32 = 16;
pub const ENCODING_TYPE_TIGHT_PNG: u32 = -260i32 as u32;

pub const ENCODING_TYPE_DESKTOP_SIZE: u32 = -223i32 as u32;
pub const ENCODING_TYPE_QEMU_EXTENDED_KEY_EVENT: u32 = -258i32 as u32;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FramebufferUpdateRequest {
    pub message_type: u8,
    pub incremental: u8,
    pub x: u16_be,
    pub y: u16_be,
    pub width: u16_be,
    pub height: u16_be,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct KeyEvent {
    pub message_type: u8,
    pub down_flag: u8,
    pub padding: [u8; 2],
    pub key: u32_be,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PointerEvent {
    pub message_type: u8,
    pub button_mask: u8,
    pub x: u16_be,
    pub y: u16_be,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ClientCutText {
    pub message_type: u8,
    pub padding: [u8; 3],
    pub length: u32_be,
    // text: [u8; N],
}

// Server to client messages

pub const SC_MESSAGE_TYPE_FRAMEBUFFER_UPDATE: u8 = 0;
pub const SC_MESSAGE_TYPE_SET_COLOR_MAP_ENTRIES: u8 = 1;
pub const SC_MESSAGE_TYPE_BELL: u8 = 2;
pub const SC_MESSAGE_TYPE_SERVER_CUT_TEXT: u8 = 3;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FramebufferUpdate {
    pub message_type: u8,
    pub padding: u8,
    pub rectangle_count: u16_be,
    // rectangles: [Rectangle; N],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Rectangle {
    pub x: u16_be,
    pub y: u16_be,
    pub width: u16_be,
    pub height: u16_be,
    pub encoding_type: u32_be,
    // data: ...
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetColorMapEntries {
    pub message_type: u8,
    pub padding: u8,
    pub first_color: u16_be,
    pub color_count: u16_be,
    // colors: [Color; N],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Color {
    pub red: u16_be,
    pub green: u16_be,
    pub blue: u16_be,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Bell {
    pub message_type: u8,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ServerCutText {
    pub message_type: u8,
    pub padding: [u8; 3],
    pub length: u32_be,
    // text: [u8; N],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QemuMessageHeader {
    pub message_type: u8,
    pub submessage_type: u8,
}

pub const QEMU_MESSAGE_EXTENDED_KEY_EVENT: u8 = 0;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QemuExtendedKeyEvent {
    pub message_type: u8,
    pub submessage_type: u8,
    pub down_flag: u16_be,
    pub keysym: u32_be,
    pub keycode: u32_be,
}
