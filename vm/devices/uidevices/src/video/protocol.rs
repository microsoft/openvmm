// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub const MAX_VMBUS_PACKET_SIZE: usize = 0x4000;

// The maximum amount of data we'll send for a cursor in one packet is 8 k.
pub const CURSOR_MAX_PAYLOAD_SIZE: usize = MAX_VMBUS_PACKET_SIZE / 2;

// Maximum supported cursor is 96 pixels x 96 pixels in ARGB 32-bit format.
pub const CURSOR_MAX_X: usize = 96;
pub const CURSOR_MAX_Y: usize = 96;
pub const CURSOR_ARGB_PIXEL_SIZE: usize = 4;
pub const CURSOR_MAX_SIZE: usize = CURSOR_MAX_X * CURSOR_MAX_Y * CURSOR_ARGB_PIXEL_SIZE;

// Only use bottom 24bits of target id to be compatible with WDDM 2.0 and DWM clone
pub const HVD_CHILD_ID: u32 = 0x00545648;
pub const HVD_CHILD_ID2: u32 = 0x00325648;
pub const HVD_CHILD_ID3: u32 = 0x00335648;

pub const EDID_BLOCK: EdidBlock = EdidBlock([
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x36, 0x68, 0x2E, 0x06, 0x00, 0x00, 0x00, 0x00,
    0xFF, 0x15, 0x01, 0x04, 0x80, 0x00, 0x00, 0x78, 0x22, 0xEE, 0x95, 0xA3, 0x54, 0x4C, 0x99, 0x26,
    0x0F, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x6C, 0x20, 0x00, 0x30, 0x42, 0x00, 0x32, 0x30, 0x40, 0xC0,
    0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1E, 0x00, 0x00, 0x00, 0xFC, 0x00, 0x48, 0x79, 0x70,
    0x65, 0x72, 0x56, 0x4D, 0x6F, 0x6E, 0x69, 0x74, 0x6F, 0x72, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC6,
]);

//
// Maximum supported number of dirty regions in a single dirt message
//
pub const MAX_DIRTY_REGIONS: u8 = 255;

macro_rules! packed {
    ($wrap:ident, $prim:ident, $count:literal) => {
        #[allow(non_camel_case_types)]
        #[repr(transparent)]
        #[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub struct $wrap([u8; $count]);

        impl $wrap {
            pub fn to_ne(self) -> $prim {
                $prim::from_ne_bytes(self.0)
            }
            pub fn from_ne(n: $prim) -> Self {
                Self(n.to_ne_bytes())
            }
        }

        impl std::fmt::Debug for $wrap {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(&self.to_ne(), f)
            }
        }

        impl From<$prim> for $wrap {
            fn from(n: $prim) -> $wrap {
                $wrap::from_ne(n)
            }
        }

        impl From<$wrap> for $prim {
            fn from(n: $wrap) -> $prim {
                n.to_ne()
            }
        }
    };
}

packed!(u64p, u64, 8);
packed!(u32p, u32, 4);
packed!(u16p, u16, 2);
packed!(i32p, i32, 4);

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ScreenInfo {
    pub width: u16p,
    pub height: u16p,
}

// Maximum number of resolutions supported. Please refer to StandardResolutions in
// VideoSynthDevice.cpp (vm\dv\video\core) for list of standard resolutions supported.
pub const MAXIMUM_RESOLUTIONS_COUNT: u8 = 64;

// Largest message possible in each direction.
pub const MAX_VSC_TO_VSP_MESSAGE_SIZE: usize =
    size_of::<PointerShapeMessage>() + CURSOR_MAX_PAYLOAD_SIZE;

pub const MAX_VSP_TO_VSC_MESSAGE_SIZE: usize = size_of::<SupportedResolutionsResponseMessage>()
    + size_of::<ScreenInfo>() * MAXIMUM_RESOLUTIONS_COUNT as usize;

// Emergency reset notification I/O Port
pub const EMERGENCY_RESET_IO_PORT: u16 = 0x100;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Version(u32p);

impl Version {
    pub fn new(major: u16, minor: u16) -> Version {
        Self(((minor as u32) << 16 | (major as u32)).into())
    }

    pub fn major(self) -> u16 {
        self.0.to_ne() as u16
    }

    pub fn minor(self) -> u16 {
        (self.0.to_ne() >> 16) as u16
    }
}

// Latest version of the SynthVid protocol.
pub const VERSION_MAJOR: u16 = 3;
pub const VERSION_MINOR: u16 = 5;

pub const VERSION_MAJOR_THRESHOLD: u16 = 3;
pub const VERSION_MINOR_BLUE: u16 = 3;
pub const VERSION_MINOR_THRESHOLD_M1: u16 = 4;
pub const VERSION_MINOR_THRESHOLD_M2: u16 = 5;

pub const ACCEPTED_WITH_VERSION_EXCHANGE: u8 = 2;

pub const fn feature_level(major: u16, minor: u16) -> u32 {
    (major as u32) << 16 | (minor & FEATURE_MINOR_MASK) as u32
}

pub const EDID_BLOCK_SIZE: usize = 128;

#[repr(transparent)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct EdidBlock(pub [u8; EDID_BLOCK_SIZE]);

impl std::fmt::Debug for EdidBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "edid")
    }
}

// Mask to be applied to the minor version to determine feature support.
pub const FEATURE_MINOR_MASK: u16 = 0xff;

// SynthVid features by version.

// Win7 RTM.
pub const FEATURE_WIN7_RTM: u32 = feature_level(3, 0);
pub const FEATURE_BASIC: u32 = FEATURE_WIN7_RTM;

// Win8 RTM.
pub const FEATURE_WIN8_RTM: u32 = feature_level(3, 2);

// Support for resolutions above 1600W or 1200H.
pub const FEATURE_HIGH_RESOLUTIONS: u32 = FEATURE_WIN8_RTM;

// Support for protocol version reinitialization.
pub const FEATURE_SUPPORTS_REINIT: u32 = FEATURE_WIN8_RTM;

// Win BLUE
pub const FEATURE_WIN_BLUE: u32 = feature_level(3, 3);
pub const FEATURE_QUERY_BIOS_INFO: u32 = FEATURE_WIN_BLUE;

// Win THRESHOLD M1
pub const FEATURE_WIN_THRESHOLD_M1: u32 = feature_level(3, 4);
pub const FEATURE_RESOLUTION_SET_BY_HOST: u32 = FEATURE_WIN_THRESHOLD_M1;

// Win THRESHOLD M2
pub const FEATURE_WIN_THRESHOLD_M2: u32 = feature_level(3, 5);
pub const FEATURE_LOCK_ON_DISCONNECT: u32 = FEATURE_WIN_THRESHOLD_M2;

// SynthVid Message Types
pub const MESSAGE_VERSION_REQUEST: u32 = 1;
pub const MESSAGE_VERSION_RESPONSE: u32 = 2;
pub const MESSAGE_VRAM_LOCATION: u32 = 3;
pub const MESSAGE_VRAM_LOCATION_ACK: u32 = 4;
pub const MESSAGE_SITUATION_UPDATE: u32 = 5;
pub const MESSAGE_SITUATION_UPDATE_ACK: u32 = 6;
pub const MESSAGE_POINTER_POSITION: u32 = 7;
pub const MESSAGE_POINTER_SHAPE: u32 = 8;
pub const MESSAGE_FEATURE_CHANGE: u32 = 9;
pub const MESSAGE_DIRT: u32 = 10;
pub const MESSAGE_BIOS_INFO_REQUEST: u32 = 11;
pub const MESSAGE_BIOS_INFO_RESPONSE: u32 = 12;
pub const MESSAGE_SUPPORTED_RESOLUTIONS_REQUEST: u32 = 13;
pub const MESSAGE_SUPPORTED_RESOLUTIONS_RESPONSE: u32 = 14;
pub const MESSAGE_CAPABILITY_REQUEST: u32 = 15;
pub const MESSAGE_CAPABILITY_RESPONSE: u32 = 16;

// Basic message structures.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageHeader {
    pub typ: u32p,  // Type of the enclosed message
    pub size: u32p, // Size of the enclosed message (size of the data payload)
}

// The following messages are listed in order of occurrence during startup
// and handshaking.

// VSC to VSP
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VersionRequestMessage {
    pub version: Version,
}

// VSP to VSC

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VersionResponseMessage {
    pub version: Version,
    pub is_accepted: u8,
    pub max_video_outputs: u8, // 1 in Viridian 1.0
}

// VSC to VSP
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SupportedResolutionsRequestMessage {
    pub maximum_resolution_count: u8,
}

// VSP to VSC
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SupportedResolutionsResponseMessage {
    pub edid_block: EdidBlock,
    pub resolution_count: u8,
    pub default_resolution_index: u8,
    pub is_standard: u8,
    //pub supported_resolutions: [ScreenInfo; MAXIMUM_RESOLUTIONS_COUNT],
}

// VSC to VSP
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CapabilityRequestMessage {}

// VSP to VSC

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CapabilityResponseMessage {
    pub lock_on_disconnect: u32p,
    pub reserved: [u32p; 15],
}

// VSC to VSP
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VramLocationMessage {
    pub user_context: u64p,
    pub is_vram_gpa_address_specified: u8,
    pub vram_gpa_address: u64p,
}

// VSP to VSC
// This is called "acknowledge", but in addition it indicates to the VSC
// that the new physical address location is backed with a memory block
// that the guest can safely write to knowing that the writes will actually
// be reflected in the VRAM memory block.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VramLocationAckMessage {
    pub user_context: u64p,
}

// These messages are used to communicate "situation updates" or changes
// in the layout of the primary surface.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VideoOutputSituation {
    pub active: u8,                        // Determine if the device is active or not.
    pub primary_surface_vram_offset: u32p, // Removed in Threshold -- must be zero.
    pub depth_bits: u8,                    // Number of bits used for each color component.
    pub width_pixels: u32p, // Number of pixels that represent the width of the image.
    pub height_pixels: u32p, // Number of pixels that represent the height of the image.
    pub pitch_bytes: u32p, // Number of bytes from one row of pixels in memory to the next row of pixels in memory.
                           // Also called stride.If padding bytes are present after the WidthPixels,the stride/pitch
                           // is wider than the width of the image.
}

// VSC to VSP
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SituationUpdateMessage {
    pub user_context: u64p,
    pub video_output_count: u8, // 1 in Viridian 1.0
    pub video_output: VideoOutputSituation,
}

// VSP to VSC
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SituationUpdateAckMessage {
    pub user_context: u64p,
}

// These messages are used to communicate the BIOS Information of the VM.
// VSC to VSP
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct BiosInfoRequestMessage {}

// VSP to VSC
// Note that the BiosFlags field used to just contain a UINT32 that indicated
// the VmGeneration.  For compatibility, we know that VM generations (0,1)
// map to the least significant bit and generation 2 (value 1) maps
// functionally to the StopDeviceSupported flag below.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct BiosInfoResponseMessage {
    pub stop_device_supported: u32p,
    pub reserved: [u8; 12],
}

// These messages are used to communicate changes in the pointer position or
// shape.

// VSC to VSP
// This message is ignored unless we're in relative mouse mode.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PointerPositionMessage {
    // LDDM may specify FALSE here, XDDM generally will probably always specify TRUE.
    pub is_visible: u8,

    // 0 is the only valid value for 2D Video VSP 1.0
    pub video_output: u8,

    // Coordinates of upper-left pixel of pointer image.
    pub image_x: i32p,
    pub image_y: i32p,
}

// VSC to VSP
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PointerShapeMessage {
    // When a cursor is larger than the maximum VMBus payload size,
    // it is split up.  This 0-based index indicates which portion
    // of the cursor payload is in this message.  -1 means final
    // portion.  If the cursor is not split, this field contains
    // -1 as the completion sentinel value.
    pub partial_index: u8,

    // VideoSynthDevice only support color cursor and monochrome cursor
    // FALSE means monochrome cursor (2 bits per pixel),
    // TRUE means color cursor (32 bits per pixel)
    pub cursor_flags: u8,

    // Max legal value is CURSOR_MAX_X
    pub width_pixels: u32p,

    // Max legal value is CURSOR_MAX_Y
    pub height_pixels: u32p,

    // Stride is implicit based on smallest possible value given width
    // in pixels and format.

    // Pointer hotspot relative to upper-left of pointer image
    pub hotspot_x: u32p,
    pub hotspot_y: u32p,
    // Max length of pixel data is 16k based on CursorFlags,
    // WidthPixels == CURSOR_MAX_X,
    // HeightPixels == CURSOR_MAX_Y.
    // At this time, we can send the whole cursor shape.
    //
    // Pointer data length can be calculated as follows:
    // if (CursorFlags)
    // {
    //     pointerDataLength = WidthPixels * HeightPixels * 4
    // }
    // else
    // {
    //     pointerDataLength = (WidthPixels + 7) / 8 * HeightPixels * 2
    // }
    // pub pixel_data: [u8; N],
}

pub const CURSOR_COMPLETE: u8 = 0xff;

// VSP to VSC
// This message is used to squelch portions of the synthvid protocol
//
// Can be sent from VSP to VSC at any time after handshaking is complete.
// VSC responsible for bringing VSP up-to-date with at least one message
// of the relevant type if one of these goes from FALSE to TRUE.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureChangeMessage {
    pub is_dirt_needed: u8,
    pub is_pointer_position_updates_needed: u8,
    pub is_pointer_shape_updates_needed: u8,
    pub is_video_situation_updates_needed: u8,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureChangeMessageV2 {
    pub is_dirt_needed: u8,
    pub is_pointer_position_updates_needed: u8,
    pub is_pointer_shape_updates_needed: u8,
    pub is_video_situation_updates_needed: u8,
    pub edid_block: EdidBlock,
}

// VSC to VSP
// This message is used to communicate dirty regions to the VSP.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DirtMessage {
    // 0 is the only valid value for 2D Video VSP 1.0
    pub video_output: u8,
    pub dirt_count: u8,
    //pub dirt: [Rectangle; 1],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Rectangle {
    pub left: i32p,
    pub top: i32p,
    pub right: i32p,
    pub bottom: i32p,
}
