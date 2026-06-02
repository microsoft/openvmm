// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(dead_code)]

use self::packed_nums::*;
use std::borrow::Cow;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[expect(non_camel_case_types)]
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

pub const ENCODING_TYPE_CURSOR: u32 = -239i32 as u32;
pub const ENCODING_TYPE_DESKTOP_SIZE: u32 = -223i32 as u32;
pub const ENCODING_TYPE_QEMU_EXTENDED_KEY_EVENT: u32 = -258i32 as u32;

/// Human-readable name for an RFB encoding number, for diagnostics logging.
///
/// Covers the registered RFB encoding and pseudo-encoding numbers (see the IANA
/// RFB registry), the Tight option ranges (compression level, JPEG quality,
/// fine-grained quality, subsampling), and the vendor blocks (VMware, Apple,
/// RealVNC, UltraVNC, LibVNCServer, extended clipboard). Names follow common
/// libvncserver usage where the registry only assigns a coarse range. Anything
/// unrecognized falls back to its signed-decimal and hex form, so nothing a
/// client advertises is dropped from the report.
///
/// Source: IANA Remote Framebuffer (RFB) registry,
/// <https://www.iana.org/assignments/rfb/rfb.xhtml>.
pub fn encoding_name(enc: i32) -> Cow<'static, str> {
    let raw = enc as u32;
    // Vendor blocks reserved as hex ranges, outside the contiguous
    // signed-number space handled by the match below.
    if (0x574d5600..=0x574d56ff).contains(&raw) {
        return Cow::Owned(format!("VMware({raw:#010x})"));
    }
    if (0xc0a1e5ce..=0xc0a1e5cf).contains(&raw) {
        return Cow::Borrowed("ExtendedClipboard");
    }
    if (0xfffe0000..=0xfffe00ff).contains(&raw) {
        return Cow::Owned(format!("LibVNCServer({raw:#010x})"));
    }
    if (0xffff0000..=0xffff8003).contains(&raw) {
        return Cow::Owned(format!("UltraVNC({raw:#010x})"));
    }
    let name = match enc {
        // Standard framebuffer encodings.
        0 => "Raw",
        1 => "CopyRect",
        2 => "RRE",
        4 => "CoRRE",
        5 => "Hextile",
        6 => "zlib",
        7 => "Tight",
        8 => "zlibhex",
        15 => "TRLE",
        16 => "ZRLE",
        17 => "ZYWRLE",
        20 => "H.264",
        21 => "JPEG",
        22 => "JRLE",
        23 => "VA-H.264",
        24 => "ZRLE2",
        50 => "OpenH.264",
        // Pseudo-encodings.
        -223 => "DesktopSize",
        -224 => "LastRect",
        -232 => "PointerPos",
        -239 => "Cursor",
        -240 => "XCursor",
        -257 => "QEMUPointerMotionChange",
        -258 => "QEMUExtendedKeyEvent",
        -259 => "QEMUAudio",
        -260 => "TightPNG",
        -261 => "LedState",
        -305 => "gii",
        -306 => "popa",
        -307 => "DesktopName",
        -308 => "ExtendedDesktopSize",
        -309 => "xvp",
        -310 => "OliveCallControl",
        -311 => "ClientRedirect",
        -312 => "Fence",
        -313 => "ContinuousUpdates",
        -314 => "CursorWithAlpha",
        -315 => "ColorMap",
        -316 => "ExtendedMouseButtons",
        -317 => "TightNoZlib",
        // Range and vendor-block arms come after the single-value arms so a
        // future named encoding takes precedence. These ranges are disjoint
        // from each other and from the single values above (per the IANA RFB
        // registry), so the order among them does not matter; an overlapping
        // literal added later would be an unreachable_pattern compile error
        // under -D warnings rather than a silent shadow.
        -32..=-23 => return Cow::Owned(format!("TightJpegQuality({})", enc + 32)),
        -256..=-247 => return Cow::Owned(format!("TightCompressionLevel({})", enc + 256)),
        -304..=-273 => return Cow::Owned(format!("VMware({enc})")),
        -512..=-412 => return Cow::Owned(format!("TightFineQuality({})", enc + 512)),
        -528..=-523 => return Cow::Owned(format!("CarConnectivity({enc})")),
        -768..=-763 => return Cow::Owned(format!("TightSubsampling({})", enc + 768)),
        1024..=1099 => return Cow::Owned(format!("RealVNC({enc})")),
        1000..=1002 | 1011 | 1100..=1109 => return Cow::Owned(format!("Apple({enc})")),
        _ => return Cow::Owned(format!("Unknown({enc}/{raw:#010x})")),
    };
    Cow::Borrowed(name)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_name_known_and_pseudo() {
        assert_eq!(encoding_name(0), "Raw");
        assert_eq!(encoding_name(6), "zlib");
        assert_eq!(encoding_name(15), "TRLE");
        assert_eq!(encoding_name(16), "ZRLE");
        assert_eq!(encoding_name(20), "H.264");
        assert_eq!(encoding_name(24), "ZRLE2");
        assert_eq!(encoding_name(50), "OpenH.264");
        assert_eq!(encoding_name(-239), "Cursor");
        assert_eq!(encoding_name(-240), "XCursor");
        assert_eq!(encoding_name(-223), "DesktopSize");
        assert_eq!(encoding_name(-313), "ContinuousUpdates");
        assert_eq!(encoding_name(-316), "ExtendedMouseButtons");
        assert_eq!(encoding_name(-317), "TightNoZlib");
    }

    #[test]
    fn encoding_name_vendor_blocks() {
        // VMware reserves 0x574d56xx; report the block with its sub-code.
        assert_eq!(encoding_name(0x574d5664u32 as i32), "VMware(0x574d5664)");
        assert_eq!(encoding_name(0x574d56ffu32 as i32), "VMware(0x574d56ff)");
        // VMware also reserves a negative pseudo-encoding block.
        assert_eq!(encoding_name(-273), "VMware(-273)");
        // Extended clipboard pseudo-encoding.
        assert_eq!(encoding_name(0xc0a1e5ceu32 as i32), "ExtendedClipboard");
        assert_eq!(
            encoding_name(0xfffe0000u32 as i32),
            "LibVNCServer(0xfffe0000)"
        );
        assert_eq!(encoding_name(0xffff0000u32 as i32), "UltraVNC(0xffff0000)");
        // RealVNC owns 1024-1099; Apple owns 1000-1002, 1011, 1100-1109. The
        // two must not overlap, and the gaps between Apple's sub-blocks are
        // unassigned (not Apple).
        assert_eq!(encoding_name(1050), "RealVNC(1050)");
        assert_eq!(encoding_name(1024), "RealVNC(1024)");
        assert_eq!(encoding_name(1000), "Apple(1000)");
        assert_eq!(encoding_name(1011), "Apple(1011)");
        assert_eq!(encoding_name(1100), "Apple(1100)");
        assert_eq!(encoding_name(1015), "Unknown(1015/0x000003f7)");
    }

    #[test]
    fn encoding_name_ranges() {
        // Tight compression levels 0..=9 map to -256..=-247.
        assert_eq!(encoding_name(-256), "TightCompressionLevel(0)");
        assert_eq!(encoding_name(-247), "TightCompressionLevel(9)");
        // JPEG quality levels 0..=9 map to -32..=-23.
        assert_eq!(encoding_name(-32), "TightJpegQuality(0)");
        assert_eq!(encoding_name(-23), "TightJpegQuality(9)");
        // Fine-grained quality 0..=100 map to -512..=-412.
        assert_eq!(encoding_name(-512), "TightFineQuality(0)");
        assert_eq!(encoding_name(-412), "TightFineQuality(100)");
        // Subsampling 0..=5 map to -768..=-763.
        assert_eq!(encoding_name(-768), "TightSubsampling(0)");
        assert_eq!(encoding_name(-763), "TightSubsampling(5)");
    }

    #[test]
    fn encoding_name_unknown_keeps_value() {
        assert_eq!(encoding_name(3), "Unknown(3/0x00000003)");
        assert_eq!(encoding_name(-5), "Unknown(-5/0xfffffffb)");
    }
}
