// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]
#![allow(unused_macros)]

use guid::Guid;
use static_assertions::const_assert_eq;
use std::fmt;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

//cfa8b69e-5b4a-4cc0-b98b-8ba1a1f3f95a
pub const INTERFACE_GUID: Guid = Guid::from_static_str("cfa8b69e-5b4a-4cc0-b98b-8ba1a1f3f95a");

//58f75a6d-d949-4320-99e1-a2a2576d581c
pub const INSTANCE_GUID: Guid = Guid::from_static_str("58f75a6d-d949-4320-99e1-a2a2576d581c");

//SynthHID protocol
const fn make_version(major: u16, minor: u16) -> u32 {
    (minor as u32) | ((major as u32) << 16)
}

pub const SYNTHHID_INPUT_VERSION: u32 = make_version(2, 0);

pub const SYNTHHID_PROTOCOL_REQUEST: u32 = 0;
pub const SYNTHHID_PROTOCOL_RESPONSE: u32 = 1;
pub const SYNTHHID_INIT_DEVICE_INFO: u32 = 2;
pub const SYNTHHID_INIT_DEVICE_INFO_ACK: u32 = 3;
pub const SYNTHHID_PROTOCOL_INPUT_REPORT: u32 = 4;
pub const SYNTHHID_HID_MAX: u32 = 5;

pub const HID_VENDOR_ID: u16 = 0x045e;
pub const HID_PRODUCT_ID: u16 = 0x0621;
pub const HID_VERSION_ID: u16 = 0x0001;

pub const SYNTHHID_INPUT_REPORT_SIZE: u8 = 16;
pub const MAX_HID_MESSAGE_SIZE: u16 = 512;
pub const MAX_HID_REPORT_DESCRIPTOR: u16 = 256;

//HID Protocol Structs
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HidAttributes {
    pub size: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version_id: u16,
    pub padding: [u16; 11],
}

const_assert_eq!(0x20, size_of::<HidAttributes>());

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HidDescriptor {
    pub length: u8,
    pub descriptor_type: u8,
    pub hid: u16,
    pub country: u8,
    pub num_descriptors: u8,
    pub descriptor_list: HidDescriptorList,
}

const_assert_eq!(0x9, size_of::<HidDescriptor>());

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HidDescriptorList {
    pub report_type: u8,
    pub report_length: u16,
}

const_assert_eq!(0x3, size_of::<HidDescriptorList>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageHeader {
    pub message_type: u32,
    pub message_size: u32,
}

const_assert_eq!(0x8, size_of::<MessageHeader>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageProtocolRequest {
    pub version: u32,
}

const_assert_eq!(0x4, size_of::<MessageProtocolRequest>());

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageProtocolResponse {
    pub version_requested: u32,
    pub accepted: u8,
}

const_assert_eq!(0x5, size_of::<MessageProtocolResponse>());

#[repr(C, packed)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageDeviceInfo {
    pub device_attributes: HidAttributes,
    pub descriptor_info: HidDescriptor,
    pub report_descriptor: [u8; 128],
}

const_assert_eq!(169, size_of::<MessageDeviceInfo>());

impl fmt::Debug for MessageDeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageDeviceInfo")
            .field("descriptor_info", &self.descriptor_info)
            .finish()
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageDeviceInfoAck {
    pub acknowledged: u8,
}

const_assert_eq!(1, size_of::<MessageDeviceInfoAck>());

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageInputReport {
    pub input_report: MousePacket,
}

const_assert_eq!(7, size_of::<MessageInputReport>());

pub const REPORT_DESCRIPTOR: [u8; 67] = [
    0x05, 0x01, // USAGE_PAGE (Generic Desktop)
    0x09, 0x02, // USAGE (Mouse)
    0xA1, 0x01, // COLLECTION (Application)
    0x09, 0x01, //   USAGE (Pointer)
    0xA1, 0x00, //   COLLECTION (Physical)
    0x05, 0x09, //     USAGE_PAGE (Buttons)
    0x19, 0x01, //     Usage Minimum (01)
    0x29, 0x05, //     Usage Maximum (05)
    0x15, 0x00, //     Logical Minimum (00)
    0x25, 0x01, //     Logical Maximum (01)
    0x95, 0x05, //     Report Count (5)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Variable, Absolute) ;
    //         5 button bits
    0x95, 0x01, //     Report Count (1)
    0x75, 0x03, //     Report Size (3)
    0x81, 0x01, //     Input (Constant) ; 3 bit padding
    0x05, 0x01, //     USAGE_PAGE (Generic Desktop)
    0x09, 0x30, //     USAGE (X)
    0x09, 0x31, //     USAGE (Y)
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x02, //     Input (Data, Variable, Absolute) ;
    //         2 Axes absolute data.
    0x05, 0x01, //     USAGE_PAGE (Generic Desktop)
    0x09, 0x38, //     USAGE (Wheel)
    0x16, 0x01, 0x80, //     Logical Minimum (-32767)
    0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Variable, Relative) ;
    //         1 Axes relative data.
    0xC0, //   END_COLLECTION
    0xC0, // END_COLLECTION
];

pub const HID_MOUSE_BUTTON_LEFT: u8 = 0x01;
pub const HID_MOUSE_BUTTON_RIGHT: u8 = 0x02;
pub const HID_MOUSE_BUTTON_MIDDLE: u8 = 0x04;

pub const MOUSE_NUMBER_BUTTONS: usize = 5;

pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    Fourth = 3,
    Fifth = 4,
}

#[derive(Copy, Clone, Debug)]
pub enum ScrollType {
    //we use -1, 1, because we want to increment the z-value (i16) in the corresponding direction
    Down = -1,
    NoChange = 0,
    Up = 1,
}

pub const MOUSE_EVENT_FLAG_XY_ABSOLUTE: u32 = 1 << 0;

pub const fn event_no_change(button: u8) -> u32 {
    1 << ((button as u32) + 1)
}

pub const MOUSE_EVENT_FLAG_LEFT_BUTTON_NO_CHANGE: u32 = event_no_change(MouseButton::Left as u8);
pub const MOUSE_EVENT_FLAG_RIGHT_BUTTON_NO_CHANGE: u32 = event_no_change(MouseButton::Right as u8);
pub const MOUSE_EVENT_FLAG_MIDDLE_BUTTON_NO_CHANGE: u32 =
    event_no_change(MouseButton::Middle as u8);
pub const MOUSE_EVENT_FLAG_FOURTH_BUTTON_NO_CHANGE: u32 =
    event_no_change(MouseButton::Fourth as u8);
pub const MOUSE_EVENT_FLAG_FIFTH_BUTTON_NO_CHANGE: u32 = event_no_change(MouseButton::Fifth as u8);

pub const MOUSE_EVENT_FLAG_ALL_BUTTONS_NO_CHANGE: u32 = MOUSE_EVENT_FLAG_LEFT_BUTTON_NO_CHANGE
    | MOUSE_EVENT_FLAG_MIDDLE_BUTTON_NO_CHANGE
    | MOUSE_EVENT_FLAG_RIGHT_BUTTON_NO_CHANGE
    | MOUSE_EVENT_FLAG_FOURTH_BUTTON_NO_CHANGE
    | MOUSE_EVENT_FLAG_FIFTH_BUTTON_NO_CHANGE;

pub const fn event_single_change(button: MouseButton) -> u32 {
    MOUSE_EVENT_FLAG_ALL_BUTTONS_NO_CHANGE & (!(event_no_change(button as u8)))
}

pub const MOUSE_EVENT_FLAG_FORCE_REPORT_EVENT: u32 = 1 << 8;

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MousePacket {
    pub button_data: u8,

    pub x: u16,
    pub y: u16,
    pub z: i16,
}

const_assert_eq!(7, size_of::<MousePacket>());
