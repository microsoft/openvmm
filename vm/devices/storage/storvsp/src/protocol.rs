// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use guid::Guid;
use open_enum::open_enum;
use scsi_defs::srb::SrbStatusAndFlags;
use scsi_defs::ScsiStatus;
use std::fmt::Debug;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub const SCSI_INTERFACE_ID: Guid = Guid::from_static_str("ba6163d9-04a1-4d29-b605-72e2ffb1dc7f");

pub const IDE_ACCELERATOR_INTERFACE_ID: Guid =
    Guid::from_static_str("32412632-86cb-44a2-9b5c-50d1417354f5");

/// Sent as part of the channel offer. Old versions of Windows drivers look at
/// this to determine the IDE device the channel is for. Newer drivers and Linux
/// just look at instance ID.
#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct OfferProperties {
    pub reserved: u16,
    pub path_id: u8,
    pub target_id: u8,
    pub reserved2: u32,
    pub flags: u32,
    pub reserved3: [u32; 3],
}

pub const OFFER_PROPERTIES_FLAG_IDE_DEVICE: u32 = 0x2;

const fn version(major: u8, minor: u8) -> u16 {
    (major as u16) << 8 | minor as u16
}

pub const VERSION_WIN6: u16 = version(2, 0);
pub const VERSION_WIN7: u16 = version(4, 2);
pub const VERSION_WIN8: u16 = version(5, 1);
pub const VERSION_BLUE: u16 = version(6, 0);
pub const VERSION_THRESHOLD: u16 = version(6, 2);

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
    pub enum NtStatus: u32 {
        SUCCESS = 0x0000_0000,
        BUFFER_OVERFLOW = 0x8000_0005, // The data was too large to fit into the specified buffer.
        DEVICE_BUSY = 0x8000_0011, // The device is currently busy.

        UNSUCCESSFUL = 0xC000_0001, // The requested operation was unsuccessful.
        INVALID_PARAMETER = 0xC000_000D, // An invalid parameter was passed to a service or function.
        INVALID_DEVICE_REQUEST = 0xC000_0010, // The specified request is not a valid operation for the target device.
        REVISION_MISMATCH = 0xC000_0059, // Used to indicate the requested version is not supported.
        DEVICE_NOT_CONNECTED = 0xC000_009D,
        IO_TIMEOUT = 0xC000_00B5, // The specified I/O operation was not completed before the time-out period expired.
        DEVICE_DOES_NOT_EXIST = 0xC000_00C0, // This device does not exist.
        CANCELLED = 0xC000_0120, // The I/O request was canceled.

        INVALID_DEVICE_STATE = 0xC000_0184, // The device is not in a valid state to perform this request.
        IO_DEVICE_ERROR = 0xC000_0185, // The I/O device reported an I/O error.
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct Packet {
    // Requested operation type
    pub operation: Operation,
    //  Flags - see below for values
    pub flags: u32,
    // Status of the request returned from the server side.
    pub status: NtStatus,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
    pub enum Operation: u32 {
        COMPLETE_IO = 1,
        REMOVE_DEVICE = 2,
        EXECUTE_SRB = 3,
        RESET_LUN = 4,
        RESET_ADAPTER = 5,
        RESET_BUS = 6,
        BEGIN_INITIALIZATION = 7,
        END_INITIALIZATION = 8,
        QUERY_PROTOCOL_VERSION = 9,
        QUERY_PROPERTIES = 10,
        ENUMERATE_BUS = 11,
        FC_HBA_DATA = 12,
        CREATE_SUB_CHANNELS = 13,
        EVENT_NOTIFICATION = 14,
    }
}

pub const CDB16GENERIC_LENGTH: usize = 0x10;
pub const MAX_DATA_BUFFER_LENGTH_WITH_PADDING: usize = 0x14;
pub const VMSCSI_SENSE_BUFFER_SIZE: usize = 0x14;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct ScsiRequest {
    pub length: u16,
    pub srb_status: SrbStatusAndFlags,
    pub scsi_status: ScsiStatus,

    pub reserved1: u8,
    pub path_id: u8,
    pub target_id: u8,
    pub lun: u8,

    pub cdb_length: u8,
    pub sense_info_ex_length: u8,
    pub data_in: u8,
    pub properties: u8,

    pub data_transfer_length: u32,

    pub payload: [u8; MAX_DATA_BUFFER_LENGTH_WITH_PADDING],

    // The following were added in Windows 8
    pub reserve: u16,
    pub queue_tag: u8,
    pub queue_action: u8,
    pub srb_flags: u32,
    pub time_out_value: u32,
    pub queue_sort_key: u32,
}

pub const SCSI_REQUEST_LEN_V1: usize = 0x24;
pub const SCSI_REQUEST_LEN_V2: usize = 0x34;
pub const SCSI_REQUEST_LEN_MAX: usize = SCSI_REQUEST_LEN_V2;

const _: () = assert!(SCSI_REQUEST_LEN_MAX == size_of::<ScsiRequest>());

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ChannelProperties {
    pub reserved: u32,
    pub maximum_sub_channel_count: u16,
    pub reserved2: u16,
    pub flags: u32,
    pub max_transfer_bytes: u32,
    pub reserved3: [u32; 2],
}

// ChannelProperties flags
pub const STORAGE_CHANNEL_SUPPORTS_MULTI_CHANNEL: u32 = 0x1;

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ProtocolVersion {
    // Major (MSB) and minor (LSB) version numbers.
    pub major_minor: u16,
    pub reserved: u16,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ClientProperties {
    flags: u32, // AsyncNotifyCapable
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NotificationPacket {
    pub lun: u8,
    pub target: u8,
    pub path: u8,
    pub flags: u8,
}

pub const SCSI_IOCTL_DATA_OUT: u8 = 0;
pub const SCSI_IOCTL_DATA_IN: u8 = 1;
