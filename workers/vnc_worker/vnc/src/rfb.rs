// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(dead_code)]

use self::packed_nums::*;
use open_enum::open_enum;
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

open_enum! {
    /// Every registered RFB framebuffer encoding and pseudo-encoding. The
    /// registry numbers are signed (the pseudo-encodings are negative), so
    /// storage is `i32`; the wire field is unsigned big-endian
    /// (`Rectangle::encoding_type`), reached with [`EncodingType::wire_u32`].
    ///
    /// Numbers outside this set are the Tight option ranges and the vendor
    /// blocks, handled by `encoding_name`.
    ///
    /// Source: IANA Remote Framebuffer (RFB) registry,
    /// <https://www.iana.org/assignments/rfb/rfb.xhtml>.
    pub enum EncodingType: i32 {
        // Standard framebuffer encodings.
        /// Uncompressed pixels, left to right then top to bottom.
        RAW = 0,
        /// Copy of a rectangle already present elsewhere in the framebuffer.
        COPY_RECT = 1,
        /// Rise-and-run-length encoding: a background plus colored subrectangles.
        RRE = 2,
        /// Compact RRE, with subrectangle counts bounded to one byte.
        CO_RRE = 4,
        /// 16x16 tiles, each sent raw or as RRE-style subrectangles.
        HEXTILE = 5,
        /// Raw pixels compressed with a single zlib stream.
        ZLIB = 6,
        /// Tight: zlib with palette and gradient filters, plus optional JPEG.
        TIGHT = 7,
        /// Hextile with each tile optionally zlib-compressed.
        ZLIBHEX = 8,
        /// Tiled run-length encoding (16x16 tiles, palette plus RLE).
        TRLE = 15,
        /// TRLE wrapped in a single zlib stream.
        ZRLE = 16,
        /// ZRLE with a wavelet filter for lossy compression.
        ZYWRLE = 17,
        /// H.264 video stream.
        H264 = 20,
        /// JPEG-compressed rectangles.
        JPEG = 21,
        /// JPEG combined with run-length encoding.
        JRLE = 22,
        /// Hardware (VA-API) H.264 video stream.
        VA_H264 = 23,
        /// RealVNC's revised ZRLE.
        ZRLE2 = 24,
        /// OpenH.264-based video, offered by TigerVNC.
        OPEN_H264 = 50,
        // Pseudo-encodings: capability and control signals, not pixel data.
        /// Client accepts server-initiated framebuffer resizes.
        DESKTOP_SIZE = -223,
        /// Sentinel rectangle ending an update of unspecified length.
        LAST_RECT = -224,
        /// Server moves the client's pointer to a given position.
        POINTER_POS = -232,
        /// Color cursor shape and hotspot (RichCursor).
        CURSOR = -239,
        /// Two-color cursor shape and hotspot (X-style).
        XCURSOR = -240,
        /// QEMU: switch between absolute and relative pointer motion.
        QEMU_POINTER_MOTION_CHANGE = -257,
        /// QEMU: key events that carry a hardware scancode.
        QEMU_EXTENDED_KEY_EVENT = -258,
        /// QEMU: audio playback channel.
        QEMU_AUDIO = -259,
        /// Tight encoding that uses PNG in place of JPEG.
        TIGHT_PNG = -260,
        /// Server reports keyboard LED (caps/num/scroll) state.
        LED_STATE = -261,
        /// General Input Interface: tablets, joysticks, and other devices.
        GII = -305,
        /// `popa` pseudo-encoding (IANA RFB registry).
        POPA = -306,
        /// Server sends an updated desktop name.
        DESKTOP_NAME = -307,
        /// Resize carrying the full screen layout, not just dimensions.
        EXTENDED_DESKTOP_SIZE = -308,
        /// xvp: client-requested power control (shutdown, reboot, reset).
        XVP = -309,
        /// Olive Call Control pseudo-encoding (IANA RFB registry).
        OLIVE_CALL_CONTROL = -310,
        /// Server redirects the client to another server.
        CLIENT_REDIRECT = -311,
        /// Fence: a synchronization barrier exchanged with the peer.
        FENCE = -312,
        /// Client requests updates without sending one request per frame.
        CONTINUOUS_UPDATES = -313,
        /// Cursor shape with a full alpha channel.
        CURSOR_WITH_ALPHA = -314,
        /// Color-map pseudo-encoding (IANA RFB registry).
        COLOR_MAP = -315,
        /// Pointer events with more than the standard button mask.
        EXTENDED_MOUSE_BUTTONS = -316,
        /// Tight encoding with zlib compression disabled.
        TIGHT_NO_ZLIB = -317,
    }
}

impl EncodingType {
    /// The encoding number as it travels in the unsigned big-endian wire field,
    /// a bit-pattern reinterpretation of the signed registry number.
    pub fn wire_u32(self) -> u32 {
        self.0 as u32
    }
}

/// ZRLE tile size: 64 pixels per side, fixed by the RFB spec.
pub const ZRLE_TILE: u16 = 64;

/// The four `0xffff80xx` codes in UltraVNC's vendor block, used as its marker.
const ULTRAVNC_MARKER_RANGE: std::ops::RangeInclusive<u32> = 0xffff8000..=0xffff8003;

/// Human-readable name for an RFB encoding number, for diagnostics logging.
///
/// Covers the registered RFB encoding and pseudo-encoding numbers, the Tight
/// option ranges (compression level, JPEG quality, fine-grained quality,
/// subsampling), and the vendor blocks (VMware, Apple, RealVNC, UltraVNC,
/// LibVNCServer, extended clipboard). Names follow common libvncserver usage
/// where the registry only assigns a coarse range. Anything unrecognized falls
/// back to its signed-decimal and hex form.
///
/// Source: IANA Remote Framebuffer (RFB) registry,
/// <https://www.iana.org/assignments/rfb/rfb.xhtml>.
pub fn encoding_name(enc: EncodingType) -> Cow<'static, str> {
    let raw = enc.wire_u32();
    // Vendor blocks live in high u32 hex ranges, outside the contiguous
    // signed-number space the named variants cover; match them on the raw value.
    match raw {
        0x574d5600..=0x574d56ff => return Cow::Owned(format!("VMware({raw:#010x})")),
        0xc0a1e5ce..=0xc0a1e5cf => return Cow::Borrowed("ExtendedClipboard"),
        0xfffe0000..=0xfffe00ff => return Cow::Owned(format!("LibVNCServer({raw:#010x})")),
        0xffff0000..=0xffff8003 => return Cow::Owned(format!("UltraVNC({raw:#010x})")),
        _ => {}
    }
    // Single registered values, named from the typed variants.
    let name = match enc {
        // Standard framebuffer encodings.
        EncodingType::RAW => "Raw",
        EncodingType::COPY_RECT => "CopyRect",
        EncodingType::RRE => "RRE",
        EncodingType::CO_RRE => "CoRRE",
        EncodingType::HEXTILE => "Hextile",
        EncodingType::ZLIB => "zlib",
        EncodingType::TIGHT => "Tight",
        EncodingType::ZLIBHEX => "zlibhex",
        EncodingType::TRLE => "TRLE",
        EncodingType::ZRLE => "ZRLE",
        EncodingType::ZYWRLE => "ZYWRLE",
        EncodingType::H264 => "H.264",
        EncodingType::JPEG => "JPEG",
        EncodingType::JRLE => "JRLE",
        EncodingType::VA_H264 => "VA-H.264",
        EncodingType::ZRLE2 => "ZRLE2",
        EncodingType::OPEN_H264 => "OpenH.264",
        // Pseudo-encodings.
        EncodingType::DESKTOP_SIZE => "DesktopSize",
        EncodingType::LAST_RECT => "LastRect",
        EncodingType::POINTER_POS => "PointerPos",
        EncodingType::CURSOR => "Cursor",
        EncodingType::XCURSOR => "XCursor",
        EncodingType::QEMU_POINTER_MOTION_CHANGE => "QEMUPointerMotionChange",
        EncodingType::QEMU_EXTENDED_KEY_EVENT => "QEMUExtendedKeyEvent",
        EncodingType::QEMU_AUDIO => "QEMUAudio",
        EncodingType::TIGHT_PNG => "TightPNG",
        EncodingType::LED_STATE => "LedState",
        EncodingType::GII => "gii",
        EncodingType::POPA => "popa",
        EncodingType::DESKTOP_NAME => "DesktopName",
        EncodingType::EXTENDED_DESKTOP_SIZE => "ExtendedDesktopSize",
        EncodingType::XVP => "xvp",
        EncodingType::OLIVE_CALL_CONTROL => "OliveCallControl",
        EncodingType::CLIENT_REDIRECT => "ClientRedirect",
        EncodingType::FENCE => "Fence",
        EncodingType::CONTINUOUS_UPDATES => "ContinuousUpdates",
        EncodingType::CURSOR_WITH_ALPHA => "CursorWithAlpha",
        EncodingType::COLOR_MAP => "ColorMap",
        EncodingType::EXTENDED_MOUSE_BUTTONS => "ExtendedMouseButtons",
        EncodingType::TIGHT_NO_ZLIB => "TightNoZlib",
        // Option ranges carry a parameter in the name. These ranges are
        // disjoint from each other and from the single values, so order does
        // not matter.
        _ => {
            let v = enc.0;
            return match v {
                -32..=-23 => Cow::Owned(format!("TightJpegQuality({})", v + 32)),
                -256..=-247 => Cow::Owned(format!("TightCompressionLevel({})", v + 256)),
                -304..=-273 => Cow::Owned(format!("VMware({v})")),
                -512..=-412 => Cow::Owned(format!("TightFineQuality({})", v + 512)),
                -528..=-523 => Cow::Owned(format!("CarConnectivity({v})")),
                -768..=-763 => Cow::Owned(format!("TightSubsampling({})", v + 768)),
                1024..=1099 => Cow::Owned(format!("RealVNC({v})")),
                1000..=1002 | 1011 | 1100..=1109 => Cow::Owned(format!("Apple({v})")),
                _ => Cow::Owned(format!("Unknown({v}/{raw:#010x})")),
            };
        }
    };
    Cow::Borrowed(name)
}

/// Best-effort guess at the connecting client from its `SetEncodings` list, or
/// `None` when nothing matches.
///
/// RFB has no field that names the client software, so the offered encoding set
/// is the only fingerprint. The checks run from most specific to most generic,
/// because the broad shapes overlap: UltraVNC also offers Tight and TigerVNC
/// also offers ZRLE, so the vendor and PNG markers are tested first.
///
/// The markers come from captured offers of each client; see the "Common VNC
/// client capabilities" page in the Guide.
pub fn guess_client(encodings: &[EncodingType]) -> Option<&'static str> {
    let has = |code: EncodingType| encodings.contains(&code);

    if encodings
        .iter()
        .any(|e| ULTRAVNC_MARKER_RANGE.contains(&e.wire_u32()))
    {
        return Some("UltraVNC");
    }
    if has(EncodingType::TIGHT_PNG) {
        return Some("noVNC");
    }
    if has(EncodingType::OPEN_H264) && has(EncodingType::XCURSOR) {
        return Some("TigerVNC");
    }
    if has(EncodingType::ZRLE2) || has(EncodingType::JRLE) {
        return Some("RealVNC");
    }
    // MobaXterm has no unique marker; its tell is a tiny offer carrying TRLE
    // but neither Tight nor DesktopSize. The captured MobaXterm offer is 6
    // encodings, so the size is bounded at 8.
    if encodings.len() <= 8
        && has(EncodingType::TRLE)
        && !has(EncodingType::TIGHT)
        && !has(EncodingType::DESKTOP_SIZE)
    {
        return Some("MobaXterm");
    }
    None
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

    // Each fingerprint below is the encoding set the named client actually
    // offered when captured against the server (see the Guide capability page).
    #[test]
    fn guess_client_identifies_novnc() {
        // TightPNG (-260) is the tell; only noVNC offers it.
        let novnc = [1, 7, -260, 16, 21, 5, 2, 6, 0, -223, -224, -258, -308].map(EncodingType);
        assert_eq!(guess_client(&novnc), Some("noVNC"));
    }

    #[test]
    fn guess_client_identifies_tigervnc() {
        // OpenH.264 (50) plus XCursor (-240); TigerVNC also offers ZRLE and
        // Tight, so it must not be mistaken for RealVNC or a generic client.
        let tiger = [
            -314, -239, -240, -223, -308, -224, 7, 1, 50, 21, 16, 5, 2, 0,
        ]
        .map(EncodingType);
        assert_eq!(guess_client(&tiger), Some("TigerVNC"));
    }

    #[test]
    fn guess_client_identifies_realvnc() {
        // ZRLE2 (24) and JRLE (22); RealVNC offers no Tight.
        let real = [24, 16, 22, 21, 15, 6, 5, 2, 0, 1, -314, -239, -223].map(EncodingType);
        assert_eq!(guess_client(&real), Some("RealVNC"));
    }

    #[test]
    fn guess_client_identifies_mobaxterm() {
        // Small set, no Tight, no DesktopSize, but TRLE (15) present.
        let moba = [16, 15, 6, 1, -239, 0].map(EncodingType);
        assert_eq!(guess_client(&moba), Some("MobaXterm"));
    }

    #[test]
    fn guess_client_large_trle_set_is_not_mobaxterm() {
        // A bigger offer that carries TRLE but none of the distinguishing
        // markers must not be guessed as MobaXterm; the size bound keeps it at
        // no guess rather than a wrong label.
        let big = [0, 1, 2, 4, 5, 6, 15, 16, 17].map(EncodingType);
        assert_eq!(guess_client(&big), None);
    }

    #[test]
    fn guess_client_identifies_ultravnc() {
        // A 0xffff80xx vendor code wins even though UltraVNC also offers Tight.
        let ultra = [
            16,
            17,
            8,
            7,
            6,
            5,
            4,
            2,
            1,
            0,
            -239,
            -232,
            0xffff8001u32 as i32,
            -224,
            -223,
            -308,
        ]
        .map(EncodingType);
        assert_eq!(guess_client(&ultra), Some("UltraVNC"));
    }

    #[test]
    fn guess_client_marker_order_beats_generic_shapes() {
        // An UltraVNC marker plus a noVNC-style TightPNG still reads as UltraVNC,
        // because the vendor marker is checked first.
        let mixed = [7, -260, 16, 0xffff8000u32 as i32].map(EncodingType);
        assert_eq!(guess_client(&mixed), Some("UltraVNC"));
    }

    #[test]
    fn guess_client_none_for_unknown() {
        // Raw-only and a generic Tight client (no distinguishing marker) get no guess.
        assert_eq!(guess_client(&[0].map(EncodingType)), None);
        assert_eq!(
            guess_client(&[0, 1, 7, 16, 5, -223].map(EncodingType)),
            None
        );
    }
}
