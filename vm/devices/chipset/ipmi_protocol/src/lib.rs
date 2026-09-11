// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wire-level definitions for the IPMI KCS interface and System Event Log.

#![forbid(unsafe_code)]

use core::mem::offset_of;
use core::mem::size_of;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::LittleEndian;
use zerocopy::U16;
use zerocopy::U32;
use zerocopy::Unaligned;

/// Size of an IPMI System Event Log record.
pub const SEL_RECORD_SIZE: usize = 16;

/// IPMI SEL version 1.5.
pub const SEL_VERSION: u8 = 0x51;

/// The SEL implementation supports reservation commands.
pub const SEL_OPERATION_SUPPORT_RESERVE: u8 = 0x02;

/// Signature required by the Clear SEL command.
pub const CLEAR_SEL_SIGNATURE: [u8; 3] = *b"CLR";

/// Clear SEL operation that queries erase progress.
pub const CLEAR_SEL_GET_STATUS: u8 = 0x00;

/// Clear SEL operation that initiates an erase.
pub const CLEAR_SEL_INITIATE_ERASE: u8 = 0xaa;

/// Clear SEL status indicating that the erase is complete.
pub const CLEAR_SEL_ERASE_COMPLETE: u8 = 0x01;

/// KCS output-buffer-full status bit.
pub const STATUS_OBF: u8 = 0x01;

/// KCS input-buffer-full status bit.
pub const STATUS_IBF: u8 = 0x02;

/// KCS system-management-software-attention status bit.
pub const STATUS_SMS_ATN: u8 = 0x04;

/// KCS command/data status bit.
pub const STATUS_CD: u8 = 0x08;

/// KCS state field mask.
pub const STATUS_STATE_MASK: u8 = 0xc0;

/// KCS idle state.
pub const KCS_STATE_IDLE: u8 = 0x00;

/// KCS read state.
pub const KCS_STATE_READ: u8 = 0x40;

/// KCS write state.
pub const KCS_STATE_WRITE: u8 = 0x80;

/// KCS error state.
pub const KCS_STATE_ERROR: u8 = 0xc0;

/// KCS Get Status/Abort command.
pub const KCS_COMMAND_GET_STATUS_ABORT: u8 = 0x60;

/// KCS Write Start command.
pub const KCS_COMMAND_WRITE_START: u8 = 0x61;

/// KCS Write End command.
pub const KCS_COMMAND_WRITE_END: u8 = 0x62;

/// KCS Read Next control byte.
pub const KCS_DATA_READ_NEXT: u8 = 0x68;

/// IPMI application network function.
pub const NETFN_APPLICATION: u8 = 0x06;

/// IPMI storage network function.
pub const NETFN_STORAGE: u8 = 0x0a;

/// Bit added to a request network function to form its response network function.
pub const NETFN_RESPONSE: u8 = 0x01;

/// Get Device ID command.
pub const COMMAND_GET_DEVICE_ID: u8 = 0x01;

/// Get SEL Info command.
pub const COMMAND_GET_SEL_INFO: u8 = 0x40;

/// Reserve SEL command.
pub const COMMAND_RESERVE_SEL: u8 = 0x42;

/// Get SEL Entry command.
pub const COMMAND_GET_SEL_ENTRY: u8 = 0x43;

/// Add SEL Entry command.
pub const COMMAND_ADD_SEL_ENTRY: u8 = 0x44;

/// Clear SEL command.
pub const COMMAND_CLEAR_SEL: u8 = 0x47;

/// Get SEL Time command.
pub const COMMAND_GET_SEL_TIME: u8 = 0x48;

/// Set SEL Time command.
pub const COMMAND_SET_SEL_TIME: u8 = 0x49;

/// Successful IPMI completion code.
pub const COMPLETION_SUCCESS: u8 = 0x00;

/// Invalid or unsupported command completion code.
pub const COMPLETION_INVALID_COMMAND: u8 = 0xc1;

/// SEL full completion code.
pub const COMPLETION_SEL_FULL: u8 = 0xc4;

/// Reservation canceled or invalid completion code.
pub const COMPLETION_RESERVATION_CANCELED: u8 = 0xc5;

/// Invalid request length completion code.
pub const COMPLETION_INVALID_REQUEST_LENGTH: u8 = 0xc7;

/// Parameter out of range completion code.
pub const COMPLETION_PARAMETER_OUT_OF_RANGE: u8 = 0xc9;

/// Requested record not present completion code.
pub const COMPLETION_RECORD_NOT_PRESENT: u8 = 0xcb;

/// Invalid data field completion code.
pub const COMPLETION_INVALID_DATA_FIELD: u8 = 0xcc;

/// A completed IPMI SEL record.
pub type SelRecord = [u8; SEL_RECORD_SIZE];

/// Common header for an IPMI request or response carried over KCS.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct MessageHeader {
    /// Network function in bits 7:2 and logical unit number in bits 1:0.
    pub netfn_lun: u8,
    /// Command identifier.
    pub command: u8,
}

impl MessageHeader {
    /// Returns the six-bit network function.
    pub const fn netfn(&self) -> u8 {
        self.netfn_lun >> 2
    }

    /// Creates the response header corresponding to this request.
    pub const fn response(self) -> Self {
        Self {
            netfn_lun: self.netfn_lun | (NETFN_RESPONSE << 2),
            command: self.command,
        }
    }
}

/// A response containing only an IPMI completion code.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct CompletionResponse {
    /// IPMI completion code.
    pub completion_code: u8,
}

impl CompletionResponse {
    /// Creates a completion-only response.
    pub const fn new(completion_code: u8) -> Self {
        Self { completion_code }
    }
}

/// Get Device ID response body.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct GetDeviceIdResponse {
    /// IPMI completion code.
    pub completion_code: u8,
    /// Device identifier.
    pub device_id: u8,
    /// Device revision.
    pub device_revision: u8,
    /// Firmware revision byte 1.
    pub firmware_revision_1: u8,
    /// Firmware revision byte 2.
    pub firmware_revision_2: u8,
    /// Supported IPMI version.
    pub ipmi_version: u8,
    /// Additional device support bitmap.
    pub additional_device_support: u8,
    /// IANA manufacturer identifier.
    pub manufacturer_id: [u8; 3],
    /// Product identifier.
    pub product_id: U16<LittleEndian>,
}

impl GetDeviceIdResponse {
    /// Creates the identity returned by the virtual BMC.
    pub const fn new() -> Self {
        Self {
            completion_code: COMPLETION_SUCCESS,
            device_id: 0x20,
            device_revision: 0x01,
            firmware_revision_1: 0x02,
            firmware_revision_2: 0x00,
            ipmi_version: 0x02,
            additional_device_support: 0x04,
            manufacturer_id: [0; 3],
            product_id: U16::ZERO,
        }
    }
}

impl Default for GetDeviceIdResponse {
    fn default() -> Self {
        Self::new()
    }
}

/// Get SEL Info response body.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct GetSelInfoResponse {
    /// IPMI completion code.
    pub completion_code: u8,
    /// SEL format version.
    pub sel_version: u8,
    /// Number of SEL entries.
    pub entry_count: U16<LittleEndian>,
    /// Remaining SEL storage in bytes.
    pub free_space: U16<LittleEndian>,
    /// Timestamp of the most recently added entry.
    pub last_addition_timestamp: U32<LittleEndian>,
    /// Timestamp of the most recent erase.
    pub last_erase_timestamp: U32<LittleEndian>,
    /// Supported SEL operations bitmap.
    pub operation_support: u8,
}

/// Reserve SEL response body.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct ReserveSelResponse {
    /// IPMI completion code.
    pub completion_code: u8,
    /// New reservation identifier.
    pub reservation_id: U16<LittleEndian>,
}

/// Get SEL Entry request data.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct GetSelEntryRequest {
    /// Reservation identifier, or zero when no reservation is used.
    pub reservation_id: U16<LittleEndian>,
    /// Requested record identifier.
    pub record_id: U16<LittleEndian>,
    /// Byte offset into the SEL record.
    pub offset: u8,
    /// Maximum number of record bytes to return.
    pub bytes_to_read: u8,
}

/// Fixed header of a Get SEL Entry response body.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct GetSelEntryResponseHeader {
    /// IPMI completion code.
    pub completion_code: u8,
    /// Identifier of the next SEL record.
    pub next_record_id: U16<LittleEndian>,
}

/// Add SEL Entry request data.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct AddSelEntryRequest {
    /// Record supplied by the management software.
    pub record: SelRecord,
}

/// Add SEL Entry response body.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct AddSelEntryResponse {
    /// IPMI completion code.
    pub completion_code: u8,
    /// Identifier assigned to the new record.
    pub record_id: U16<LittleEndian>,
}

/// Clear SEL request data.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct ClearSelRequest {
    /// Reservation identifier.
    pub reservation_id: U16<LittleEndian>,
    /// Required `CLR` signature.
    pub signature: [u8; 3],
    /// Erase or status-query operation.
    pub operation: u8,
}

/// Clear SEL response body.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct ClearSelResponse {
    /// IPMI completion code.
    pub completion_code: u8,
    /// Current erase status.
    pub erase_status: u8,
}

/// Get SEL Time response body.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct GetSelTimeResponse {
    /// IPMI completion code.
    pub completion_code: u8,
    /// Current SEL time in seconds since the Unix epoch.
    pub timestamp: U32<LittleEndian>,
}

/// Set SEL Time request data.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct SetSelTimeRequest {
    /// Requested SEL time in seconds since the Unix epoch.
    pub timestamp: U32<LittleEndian>,
}

const_assert_eq!(size_of::<MessageHeader>(), 2);
const_assert_eq!(offset_of!(MessageHeader, command), 1);
const_assert_eq!(size_of::<CompletionResponse>(), 1);
const_assert_eq!(size_of::<GetDeviceIdResponse>(), 12);
const_assert_eq!(offset_of!(GetDeviceIdResponse, manufacturer_id), 7);
const_assert_eq!(offset_of!(GetDeviceIdResponse, product_id), 10);
const_assert_eq!(size_of::<GetSelInfoResponse>(), 15);
const_assert_eq!(offset_of!(GetSelInfoResponse, entry_count), 2);
const_assert_eq!(offset_of!(GetSelInfoResponse, free_space), 4);
const_assert_eq!(offset_of!(GetSelInfoResponse, last_addition_timestamp), 6);
const_assert_eq!(offset_of!(GetSelInfoResponse, last_erase_timestamp), 10);
const_assert_eq!(offset_of!(GetSelInfoResponse, operation_support), 14);
const_assert_eq!(size_of::<ReserveSelResponse>(), 3);
const_assert_eq!(offset_of!(ReserveSelResponse, reservation_id), 1);
const_assert_eq!(size_of::<GetSelEntryRequest>(), 6);
const_assert_eq!(offset_of!(GetSelEntryRequest, record_id), 2);
const_assert_eq!(offset_of!(GetSelEntryRequest, offset), 4);
const_assert_eq!(offset_of!(GetSelEntryRequest, bytes_to_read), 5);
const_assert_eq!(size_of::<GetSelEntryResponseHeader>(), 3);
const_assert_eq!(offset_of!(GetSelEntryResponseHeader, next_record_id), 1);
const_assert_eq!(size_of::<AddSelEntryRequest>(), SEL_RECORD_SIZE);
const_assert_eq!(size_of::<AddSelEntryResponse>(), 3);
const_assert_eq!(offset_of!(AddSelEntryResponse, record_id), 1);
const_assert_eq!(size_of::<ClearSelRequest>(), 6);
const_assert_eq!(offset_of!(ClearSelRequest, signature), 2);
const_assert_eq!(offset_of!(ClearSelRequest, operation), 5);
const_assert_eq!(size_of::<ClearSelResponse>(), 2);
const_assert_eq!(size_of::<GetSelTimeResponse>(), 5);
const_assert_eq!(offset_of!(GetSelTimeResponse, timestamp), 1);
const_assert_eq!(size_of::<SetSelTimeRequest>(), 4);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_device_id_wire_layout() {
        assert_eq!(
            GetDeviceIdResponse::new().as_bytes(),
            [0x00, 0x20, 0x01, 0x02, 0x00, 0x02, 0x04, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn message_header_wire_layout() {
        let request = MessageHeader {
            netfn_lun: NETFN_STORAGE << 2,
            command: COMMAND_GET_SEL_INFO,
        };

        assert_eq!(request.as_bytes(), [0x28, 0x40]);
        assert_eq!(request.netfn(), NETFN_STORAGE);
        assert_eq!(request.response().as_bytes(), [0x2c, 0x40]);
    }

    #[test]
    fn get_sel_info_wire_layout() {
        let response = GetSelInfoResponse {
            completion_code: COMPLETION_SUCCESS,
            sel_version: SEL_VERSION,
            entry_count: U16::new(2),
            free_space: U16::new(0x07e0),
            last_addition_timestamp: U32::new(0x12345678),
            last_erase_timestamp: U32::new(0x90abcdef),
            operation_support: SEL_OPERATION_SUPPORT_RESERVE,
        };

        assert_eq!(
            response.as_bytes(),
            [
                0x00, 0x51, 0x02, 0x00, 0xe0, 0x07, 0x78, 0x56, 0x34, 0x12, 0xef, 0xcd, 0xab, 0x90,
                0x02
            ]
        );
    }

    #[test]
    fn get_sel_entry_request_wire_layout() {
        let request = GetSelEntryRequest::read_from_prefix(&[0x01, 0x00, 0x34, 0x12, 0x04, 0x08])
            .unwrap()
            .0;

        assert_eq!(request.reservation_id.get(), 1);
        assert_eq!(request.record_id.get(), 0x1234);
        assert_eq!(request.offset, 4);
        assert_eq!(request.bytes_to_read, 8);
    }

    #[test]
    fn sel_command_wire_layouts() {
        assert_eq!(
            ReserveSelResponse {
                completion_code: COMPLETION_SUCCESS,
                reservation_id: U16::new(0x1234),
            }
            .as_bytes(),
            [0x00, 0x34, 0x12]
        );
        assert_eq!(
            GetSelEntryResponseHeader {
                completion_code: COMPLETION_SUCCESS,
                next_record_id: U16::new(0xabcd),
            }
            .as_bytes(),
            [0x00, 0xcd, 0xab]
        );
        assert_eq!(
            AddSelEntryResponse {
                completion_code: COMPLETION_SUCCESS,
                record_id: U16::new(0x1234),
            }
            .as_bytes(),
            [0x00, 0x34, 0x12]
        );
        assert_eq!(
            ClearSelRequest {
                reservation_id: U16::new(1),
                signature: CLEAR_SEL_SIGNATURE,
                operation: CLEAR_SEL_INITIATE_ERASE,
            }
            .as_bytes(),
            [0x01, 0x00, b'C', b'L', b'R', 0xaa]
        );
        assert_eq!(
            ClearSelResponse {
                completion_code: COMPLETION_SUCCESS,
                erase_status: CLEAR_SEL_ERASE_COMPLETE,
            }
            .as_bytes(),
            [0x00, 0x01]
        );
        assert_eq!(
            GetSelTimeResponse {
                completion_code: COMPLETION_SUCCESS,
                timestamp: U32::new(0x12345678),
            }
            .as_bytes(),
            [0x00, 0x78, 0x56, 0x34, 0x12]
        );
        assert_eq!(
            SetSelTimeRequest {
                timestamp: U32::new(0x12345678),
            }
            .as_bytes(),
            [0x78, 0x56, 0x34, 0x12]
        );
    }
}
