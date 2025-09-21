// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod srb;

use bitfield_struct::bitfield;
use core::fmt::Debug;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

type U16BE = zerocopy::byteorder::U16<zerocopy::byteorder::BigEndian>;
type U32BE = zerocopy::byteorder::U32<zerocopy::byteorder::BigEndian>;
type U64BE = zerocopy::byteorder::U64<zerocopy::byteorder::BigEndian>;

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum ScsiOp: u8 {
        TEST_UNIT_READY = 0x00,
        REZERO_UNIT = 0x01,
        REWIND = 0x01,
        REQUEST_BLOCK_ADDR = 0x02,
        REQUEST_SENSE = 0x03,
        FORMAT_UNIT = 0x04,
        READ_BLOCK_LIMITS = 0x05,
        REASSIGN_BLOCKS = 0x07,
        INIT_ELEMENT_STATUS = 0x07,
        READ6 = 0x08,
        RECEIVE = 0x08,
        WRITE6 = 0x0A,
        PRINT = 0x0A,
        SEND = 0x0A,
        SEEK6 = 0x0B,
        TRACK_SELECT = 0x0B,
        SLEW_PRINT = 0x0B,
        SET_CAPACITY = 0x0B, // tape
        SEEK_BLOCK = 0x0C,
        PARTITION = 0x0D,
        READ_REVERSE = 0x0F,
        WRITE_FILEMARKS = 0x10,
        FLUSH_BUFFER = 0x10,
        SPACE = 0x11,
        INQUIRY = 0x12,
        VERIFY6 = 0x13,
        RECOVER_BUF_DATA = 0x14,
        MODE_SELECT = 0x15,
        RESERVE_UNIT = 0x16,
        RELEASE_UNIT = 0x17,
        COPY = 0x18,
        ERASE = 0x19,
        MODE_SENSE = 0x1A,
        START_STOP_UNIT = 0x1B,
        STOP_PRINT = 0x1B,
        LOAD_UNLOAD = 0x1B,
        RECEIVE_DIAGNOSTIC = 0x1C,
        SEND_DIAGNOSTIC = 0x1D,
        MEDIUM_REMOVAL = 0x1E,
        READ_FORMATTED_CAPACITY = 0x23,
        READ_CAPACITY = 0x25,
        READ = 0x28,
        WRITE = 0x2A,
        SEEK = 0x2B,
        LOCATE = 0x2B,
        POSITION_TO_ELEMENT = 0x2B,
        WRITE_VERIFY = 0x2E,
        VERIFY = 0x2F,
        SEARCH_DATA_HIGH = 0x30,
        SEARCH_DATA_EQUAL = 0x31,
        SEARCH_DATA_LOW = 0x32,
        SET_LIMITS = 0x33,
        READ_POSITION = 0x34,
        SYNCHRONIZE_CACHE = 0x35,
        COMPARE = 0x39,
        COPY_COMPARE = 0x3A,
        WRITE_DATA_BUFF = 0x3B,
        READ_DATA_BUFF = 0x3C,
        WRITE_LONG = 0x3F,
        CHANGE_DEFINITION = 0x40,
        WRITE_SAME = 0x41,
        READ_SUB_CHANNEL = 0x42,
        UNMAP = 0x42, // block device
        READ_TOC = 0x43,
        READ_HEADER = 0x44,
        REPORT_DENSITY_SUPPORT = 0x44, // tape
        PLAY_AUDIO = 0x45,
        GET_CONFIGURATION = 0x46,
        PLAY_AUDIO_MSF = 0x47,
        PLAY_TRACK_INDEX = 0x48,
        SANITIZE = 0x48, // block device
        PLAY_TRACK_RELATIVE = 0x49,
        GET_EVENT_STATUS = 0x4A,
        PAUSE_RESUME = 0x4B,
        LOG_SELECT = 0x4C,
        LOG_SENSE = 0x4D,
        STOP_PLAY_SCAN = 0x4E,
        XDWRITE = 0x50,
        XPWRITE = 0x51,
        READ_DISC_INFORMATION = 0x51,
        READ_TRACK_INFORMATION = 0x52,
        XDWRITE_READ = 0x53,
        RESERVE_TRACK_RZONE = 0x53,
        SEND_OPC_INFORMATION = 0x54, // optimum power calibration
        MODE_SELECT10 = 0x55,
        RESERVE_UNIT10 = 0x56,
        RESERVE_ELEMENT = 0x56,
        RELEASE_UNIT10 = 0x57,
        RELEASE_ELEMENT = 0x57,
        REPAIR_TRACK = 0x58,
        MODE_SENSE10 = 0x5A,
        CLOSE_TRACK_SESSION = 0x5B,
        READ_BUFFER_CAPACITY = 0x5C,
        SEND_CUE_SHEET = 0x5D,
        PERSISTENT_RESERVE_IN = 0x5E,
        PERSISTENT_RESERVE_OUT = 0x5F,
        REPORT_LUNS = 0xA0,
        BLANK = 0xA1,
        ATA_PASSTHROUGH12 = 0xA1,
        SEND_EVENT = 0xA2,
        SECURITY_PROTOCOL_IN = 0xA2,
        SEND_KEY = 0xA3,
        MAINTENANCE_IN = 0xA3,
        REPORT_KEY = 0xA4,
        MAINTENANCE_OUT = 0xA4,
        MOVE_MEDIUM = 0xA5,
        LOAD_UNLOAD_SLOT = 0xA6,
        EXCHANGE_MEDIUM = 0xA6,
        SET_READ_AHEAD = 0xA7,
        MOVE_MEDIUM_ATTACHED = 0xA7,
        READ12 = 0xA8,
        GET_MESSAGE = 0xA8,
        SERVICE_ACTION_OUT12 = 0xA9,
        WRITE12 = 0xAA,
        SEND_MESSAGE = 0xAB,
        SERVICE_ACTION_IN12 = 0xAB,
        GET_PERFORMANCE = 0xAC,
        READ_DVD_STRUCTURE = 0xAD,
        WRITE_VERIFY12 = 0xAE,
        VERIFY12 = 0xAF,
        SEARCH_DATA_HIGH12 = 0xB0,
        SEARCH_DATA_EQUAL12 = 0xB1,
        SEARCH_DATA_LOW12 = 0xB2,
        SET_LIMITS12 = 0xB3,
        READ_ELEMENT_STATUS_ATTACHED = 0xB4,
        REQUEST_VOL_ELEMENT = 0xB5,
        SECURITY_PROTOCOL_OUT = 0xB5,
        SEND_VOLUME_TAG = 0xB6,
        SET_STREAMING = 0xB6, // CD/DVD
        READ_DEFECT_DATA = 0xB7,
        READ_ELEMENT_STATUS = 0xB8,
        READ_CD_MSF = 0xB9,
        SCAN_CD = 0xBA,
        REDUNDANCY_GROUP_IN = 0xBA,
        SET_CD_SPEED = 0xBB,
        REDUNDANCY_GROUP_OUT = 0xBB,
        PLAY_CD = 0xBC,
        SPARE_IN = 0xBC,
        MECHANISM_STATUS = 0xBD,
        SPARE_OUT = 0xBD,
        READ_CD = 0xBE,
        VOLUME_SET_IN = 0xBE,
        SEND_DVD_STRUCTURE = 0xBF,
        VOLUME_SET_OUT = 0xBF,
        INIT_ELEMENT_RANGE = 0xE7,
        XDWRITE_EXTENDED16 = 0x80, // disk
        WRITE_FILEMARKS16 = 0x80, // tape
        REBUILD16 = 0x81, // disk
        READ_REVERSE16 = 0x81, // tape
        REGENERATE16 = 0x82, // disk
        EXTENDED_COPY = 0x83,
        POPULATE_TOKEN = 0x83, // disk
        WRITE_USING_TOKEN = 0x83, // disk
        RECEIVE_COPY_RESULTS = 0x84,
        RECEIVE_ROD_TOKEN_INFORMATION = 0x84, //disk
        ATA_PASSTHROUGH16 = 0x85,
        ACCESS_CONTROL_IN = 0x86,
        ACCESS_CONTROL_OUT = 0x87,
        READ16 = 0x88,
        COMPARE_AND_WRITE = 0x89,
        WRITE16 = 0x8A,
        READ_ATTRIBUTES = 0x8C,
        WRITE_ATTRIBUTES = 0x8D,
        WRITE_VERIFY16 = 0x8E,
        VERIFY16 = 0x8F,
        PREFETCH16 = 0x90,
        SYNCHRONIZE_CACHE16 = 0x91,
        SPACE16 = 0x91, // tape
        LOCK_UNLOCK_CACHE16 = 0x92,
        LOCATE16 = 0x92, // tape
        WRITE_SAME16 = 0x93,
        ERASE16 = 0x93, // tape
        ZBC_OUT = 0x94, // Close Zone, Finish Zone, Open Zone, Reset Write Pointer, etc.
        ZBC_IN = 0x95, // Report Zones, etc.
        READ_DATA_BUFF16 = 0x9B,
        WRITE_ATOMIC16 = 0x9C,
        READ_CAPACITY16 = 0x9E,
        GET_LBA_STATUS = 0x9E,
        GET_PHYSICAL_ELEMENT_STATUS = 0x9E,
        REMOVE_ELEMENT_AND_TRUNCATE = 0x9E,
        SERVICE_ACTION_IN16 = 0x9E,
        SERVICE_ACTION_OUT16 = 0x9F,
    }
}

pub const VPD_SUPPORTED_PAGES: u8 = 0x00;
pub const VPD_SERIAL_NUMBER: u8 = 0x80;
pub const VPD_DEVICE_IDENTIFIERS: u8 = 0x83;
pub const VPD_THIRD_PARTY_COPY: u8 = 0x8F;
pub const VPD_BLOCK_LIMITS: u8 = 0xB0;
pub const VPD_BLOCK_DEVICE_CHARACTERISTICS: u8 = 0xB1;
pub const VPD_LOGICAL_BLOCK_PROVISIONING: u8 = 0xB2;
pub const VPD_MSFT_VIRTUAL_DEVICE_PROPERTIES: u8 = 0xCF;
pub const VPD_MSFT_PAGING_EXTENT_PROPERTIES: u8 = 0xCE;
pub const DIRECT_ACCESS_DEVICE: u8 = 0x00;
pub const READ_ONLY_DIRECT_ACCESS_DEVICE: u8 = 0x05;
pub const ISO_INQUIRY_VPD_SUPPORTED_PAGE_COUNT: u8 = 0x02;

pub const VPD_CODE_SET_RESERVED: u8 = 0;
pub const VPD_CODE_SET_BINARY: u8 = 1;
pub const VPD_CODE_SET_ASCII: u8 = 2;
pub const VPD_CODE_SET_UTF8: u8 = 3;

pub const VPD_IDENTIFIER_TYPE_VENDOR_ID: u8 = 1;
pub const VPD_IDENTIFIER_TYPE_FCPH_NAME: u8 = 3;

pub const INQUIRY_DATA_BUFFER_SIZE: u8 = 36;

// readiness errors
pub const MEDIUM_NOT_PRESENT_TRAY_CLOSED: u8 = 0x01;
pub const MEDIUM_NOT_PRESENT_TRAY_OPEN: u8 = 0x02;

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbInquiry {
    pub operation_code: u8, // 0x12 - SCSIOP_INQUIRY
    pub flags: InquiryFlags,
    pub page_code: u8,
    pub allocation_length: U16BE,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct InquiryFlags {
    #[bits(1)]
    pub vpd: bool,
    #[bits(1)]
    pub csd: bool,
    #[bits(6)]
    pub reserved: u8,
}

pub const INQUIRY_ENABLE_VPD: u8 = 0x1;
pub const INQUIRY_COMMAND_SUPPORT_DATA: u8 = 0x2;
pub const T10_VERSION_SPC3: u8 = 0x05;
pub const T10_RESPONSE_DATA_SPC3: u8 = 0x02;

/*
struct InquiryData {
    UCHAR DeviceType : 5;
    UCHAR DeviceTypeQualifier : 3;

    UCHAR DeviceTypeModifier : 7;
    UCHAR RemovableMedia : 1;

    union {
        UCHAR Versions;
        struct {
            UCHAR ANSIVersion : 3;
            UCHAR ECMAVersion : 3;
            UCHAR ISOVersion : 2;
        };
    };

    UCHAR ResponseDataFormat : 4;
    UCHAR HiSupport : 1;
    UCHAR NormACA : 1;
    UCHAR TerminateTask : 1;
    UCHAR AERC : 1;

    UCHAR AdditionalLength;

    //5
    union {
        UCHAR Reserved;
        struct {
            UCHAR PROTECT : 1;
            UCHAR Reserved_1 : 2;
            UCHAR ThirdPartyCopy : 1;
            UCHAR TPGS : 2;
            UCHAR ACC : 1;
            UCHAR SCCS : 1;
       };
    };

    UCHAR Addr16 : 1;               // defined only for SIP devices.
    UCHAR Addr32 : 1;               // defined only for SIP devices.
    UCHAR AckReqQ: 1;               // defined only for SIP devices.
    UCHAR MediumChanger : 1;
    UCHAR MultiPort : 1;
    UCHAR ReservedBit2 : 1;
    UCHAR EnclosureServices : 1;
    UCHAR ReservedBit3 : 1;

    // 7
    UCHAR SoftReset : 1;
    UCHAR CommandQueue : 1;
    UCHAR TransferDisable : 1;      // defined only for SIP devices.
    UCHAR LinkedCommands : 1;
    UCHAR Synchronous : 1;          // defined only for SIP devices.
    UCHAR Wide16Bit : 1;            // defined only for SIP devices.
    UCHAR Wide32Bit : 1;            // defined only for SIP devices.
    UCHAR RelativeAddressing : 1;

    // 8
    UCHAR VendorId[8];
    //16
    UCHAR ProductId[16];
    //32
    UCHAR ProductRevisionLevel[4];
    //36
    UCHAR VendorSpecific[20];
    //56
    UCHAR Reserved3[2];
    //58
    VERSION_DESCRIPTOR VersionDescriptors[8];
    //74
    UCHAR Reserved4[30];
    //104
} INQUIRYDATA, *PINQUIRYDATA;
*/

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct InquiryDataHeader {
    /*
    UCHAR DeviceType : 5;
    UCHAR DeviceTypeQualifier : 3;
    */
    pub device_type: u8,
    pub flags2: InquiryDataFlag2,
    pub versions: u8,
    pub flags3: InquiryDataFlag3,
    pub additional_length: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct InquiryDataFlag2 {
    #[bits(7)]
    pub device_type_modifier: u8,
    #[bits(1)]
    pub removable_media: bool,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct InquiryDataFlag3 {
    #[bits(4)]
    pub response_data_format: u8,
    #[bits(1)]
    pub hi_support: bool,
    #[bits(1)]
    pub norm_aca: bool,
    #[bits(1)]
    pub reserved_bit: bool,
    #[bits(1)]
    pub aerc: bool,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct InquiryData {
    pub header: InquiryDataHeader,
    pub reserved: [u8; 2],
    /*
    UCHAR SoftReset : 1;
    UCHAR CommandQueue : 1;
    UCHAR TransferDisable : 1;      // defined only for SIP devices.
    UCHAR LinkedCommands : 1;
    UCHAR Synchronous : 1;          // defined only for SIP devices.
    UCHAR Wide16Bit : 1;            // defined only for SIP devices.
    UCHAR Wide32Bit : 1;            // defined only for SIP devices.
    UCHAR RelativeAddressing : 1;
     */
    pub misc: u8,
    pub vendor_id: [u8; 8],
    pub product_id: [u8; 16],
    pub product_revision_level: [u8; 4],
    pub vendor_specific: [u8; 20],
    pub reserved3: [u8; 2],
    pub version_descriptors: [u16; 8],
    pub reserved4: [u8; 30],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpdPageHeader {
    /*
    UCHAR DeviceType : 5;
    UCHAR DeviceTypeQualifier : 3;
     */
    pub device_type: u8,
    pub page_code: u8,
    pub reserved: u8,
    pub page_length: u8,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpdT10Id {
    pub header: VpdIdentificationDescriptor,
    pub vendor_id: [u8; 8],
    pub context_guid: [u8; 16],
}

/// NAA IEEE Registered Extended designator format.
/// => NAA = 6.
///
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpdNaaId {
    pub header: VpdIdentificationDescriptor,
    /*
    UCHAR OuidMSB : 4;
    UCHAR Naa : 4;
     */
    pub ouid_msb: u8,
    pub ouid_middle: [u8; 2],
    /*
    UCHAR Reserved4 : 4; // part of vendor specific id, always set to 0.
    UCHAR OuidLSB : 4;
     */
    pub ouid_lsb: u8,
    pub vendor_specific_id: [u8; 12],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpdIdentificationDescriptor {
    pub code_set: u8,
    pub identifiertype: u8,
    pub reserved3: u8,
    pub identifier_length: u8,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpdBlockLimitsDescriptor {
    pub reserved0: u8,
    pub max_compare_and_write_length: u8,
    pub optimal_transfer_length_granularity: U16BE,
    pub max_transfer_length: U32BE,
    pub optimal_transfer_length: U32BE,
    pub max_prefetch_xd_read_xd_write_transfer_length: U32BE,
    pub max_unmap_lba_count: U32BE,
    pub max_unmap_block_descriptor_count: U32BE,
    pub optimal_unmap_granularity: U32BE,
    pub unmap_granularity_alignment: [u8; 4],
    pub max_write_same_length: U64BE,
    pub max_atomic_transfer_length: U32BE,
    pub atomic_alignment: U32BE,
    pub atomic_transfer_length_granularity: U32BE,
    pub maximum_atomic_transfer_length_with_atomic_boundary: U32BE,
    pub maximum_atomic_boundary_size: U32BE,
}

/// VPD Page 0xB1, Block Device Characteristics
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpdBlockDeviceCharacteristicsPage {
    pub medium_rotation_rate: U16BE,
    pub data: [u8; 58], // Needn't know the details
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpdMsftVirtualDevicePropertiesPage {
    pub version: u8,
    /*
    UCHAR LBPRZ     : 1;
    UCHAR DisableIoRetries : 1; // Added in v2, indicates Classpnp should not perform any retries (any relevant retries would have been performed at a lower, physical stack).
    UCHAR Spaces : 1; // Added in v2, indicates this is a storage space
    UCHAR Reserved1 : 5;
    */
    pub flags: u8,
    pub reserved: [u8; 2],
    pub signature: [u8; 16],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpdMsftPagingExtentPropertiesPage {
    pub version: u8,
    /*
    UCHAR ModeSelectExtension : 1;
    UCHAR Reserved1 : 7;
     */
    pub mode_select_extension: u8,
    pub reserved: [u8; 2],
}

// VPD Page 0xB2, Logical Block Provisioning

pub const PROVISIONING_TYPE_UNKNOWN: u8 = 0x0;
pub const PROVISIONING_TYPE_RESOURCE: u8 = 0x1;
pub const PROVISIONING_TYPE_THIN: u8 = 0x2;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpdLogicalBlockProvisioningPage {
    pub threshold_exponent: u8,
    /*
    UCHAR DP                : 1;
    UCHAR ANC_SUP           : 1;
    UCHAR LBPRZ             : 1;
    UCHAR Reserved0         : 2;
    UCHAR LBPWS10           : 1;
    UCHAR LBPWS             : 1;
    UCHAR LBPU              : 1;
    */
    pub flags: u8,
    /*
    UCHAR ProvisioningType  : 3;
    UCHAR Reserved1         : 5;
    */
    pub provisioning_type: u8,
    pub reserved2: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SenseDataHeader {
    /*
    UCHAR ErrorCode:7;
    UCHAR Valid:1;
     */
    pub error_code: SenseDataErrorCode,
    pub segment_number: u8,
    /*
    UCHAR SenseKey:4;
    UCHAR Reserved:1;
    UCHAR IncorrectLength:1;
    UCHAR EndOfMedia:1;
    UCHAR FileMark:1;
     */
    pub sense_key: SenseKey,
    /*
    reserved:1 : u8,
    incorrect_length:1 : u8,
    end_of_media:1 : u8,
    file_mark:1 : u8,
    */
    pub information: [u8; 4],
    pub additional_sense_length: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SenseData {
    pub header: SenseDataHeader,
    pub command_specific_information: [u8; 4],
    pub additional_sense_code: AdditionalSenseCode,
    pub additional_sense_code_qualifier: u8,
    pub field_replaceable_unit_code: u8,
    pub sense_key_specific: [u8; 3],
}

impl SenseData {
    pub const fn new(
        sense_key: SenseKey,
        additional_sense_code: AdditionalSenseCode,
        additional_sense_code_qualifier: u8,
    ) -> Self {
        // Fill in sense info
        // Since MM Drives support only a 32-bit LBA format, MM Drives ignore the setting of the
        // Desc bit in the REQUEST SENSE command CDB and return only fixed format sense data.
        SenseData {
            header: SenseDataHeader {
                error_code: SenseDataErrorCode::FIXED_CURRENT,
                segment_number: 0,
                sense_key,
                information: [0; 4],
                additional_sense_length: (size_of::<SenseData>() - size_of::<SenseDataHeader>())
                    as u8,
            },
            command_specific_information: [0; 4],
            additional_sense_code,
            additional_sense_code_qualifier,
            field_replaceable_unit_code: 0,
            sense_key_specific: [0; 3],
        }
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum SenseKey: u8 {
        NO_SENSE = 0x00,
        RECOVERED_ERROR = 0x01,
        NOT_READY = 0x02,
        MEDIUM_ERROR = 0x03,
        HARDWARE_ERROR = 0x04,
        ILLEGAL_REQUEST = 0x05,
        UNIT_ATTENTION = 0x06,
        DATA_PROTECT = 0x07,
        BLANK_CHECK = 0x08,
        UNIQUE = 0x09,
        COPY_ABORTED = 0x0A,
        ABORTED_COMMAND = 0x0B,
        EQUAL = 0x0C,
        VOL_OVERFLOW = 0x0D,
        MISCOMPARE = 0x0E,
        RESERVED = 0x0F,
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum SenseDataErrorCode: u8 {
        FIXED_CURRENT = 0x70,
        FIXED_DEFERRED = 0x71,
        DESCRIPTOR_CURRENT = 0x72,
        DESCRIPTOR_DEFERRED = 0x73,
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum AdditionalSenseCode: u8 {
        NO_SENSE = 0x00,
        NO_SEEK_COMPLETE = 0x02,
        WRITE = 0x03,
        LUN_NOT_READY = 0x04,
        LUN_COMMUNICATION = 0x08,
        SERVO_ERROR = 0x09,
        WARNING = 0x0B,
        WRITE_ERROR = 0x0C,
        COPY_TARGET_DEVICE_ERROR = 0x0D,
        CRC_OR_ECC_ERROR = 0x10,
        UNRECOVERED_ERROR = 0x11,
        TRACK_ERROR = 0x14,
        SEEK_ERROR = 0x15,
        REC_DATA_NOECC = 0x17,
        REC_DATA_ECC = 0x18,
        DEFECT_LIST_ERROR = 0x19,
        PARAMETER_LIST_LENGTH = 0x1A,
        MISCOMPARE_DURING_VERIFY_OPERATION = 0x1D,
        ILLEGAL_COMMAND = 0x20,
        ACCESS_DENIED = 0x20,
        ILLEGAL_BLOCK = 0x21,
        INVALID_TOKEN = 0x23,
        INVALID_CDB = 0x24,
        INVALID_LUN = 0x25,
        INVALID_FIELD_PARAMETER_LIST = 0x26,
        WRITE_PROTECT = 0x27,
        MEDIUM_CHANGED = 0x28,
        BUS_RESET = 0x29,
        PARAMETERS_CHANGED = 0x2A,
        COMMAND_SEQUENCE_ERROR = 0x2C,
        INSUFFICIENT_TIME_FOR_OPERATION = 0x2E,
        INVALID_MEDIA = 0x30,
        MEDIUM_FORMAT_CORRUPTED = 0x31,
        DEFECT_LIST = 0x32,
        LB_PROVISIONING = 0x38,
        SAVING_PARAMETER_NOT_SUPPORTED = 0x39,
        NO_MEDIA_IN_DEVICE = 0x3a,
        POSITION_ERROR = 0x3b,
        LOGICAL_UNIT_ERROR = 0x3e,
        OPERATING_CONDITIONS_CHANGED = 0x3f,
        DATA_PATH_FAILURE = 0x41,
        POWER_ON_SELF_TEST_FAILURE = 0x42,
        INTERNAL_TARGET_FAILURE = 0x44,
        DATA_TRANSFER_ERROR = 0x4b,
        LUN_FAILED_SELF_CONFIGURATION = 0x4c,
        MEDIUM_REMOVAL_PREVENTED = 0x53,
        RESOURCE_FAILURE = 0x55,
        OPERATOR_REQUEST = 0x5a, // see below
        FAILURE_PREDICTION_THRESHOLD_EXCEEDED = 0x5d,
        ILLEGAL_MODE_FOR_THIS_TRACK = 0x64,
        COPY_PROTECTION_FAILURE = 0x6f,
        POWER_CALIBRATION_ERROR = 0x73,
        VENDOR_UNIQUE = 0x80, // and higher
        MUSIC_AREA = 0xA0,
        DATA_AREA = 0xA1,
        VOLUME_OVERFLOW = 0xA7,
    }
}

// SCSI_ADSENSE_LUN_NOT_READY (0x04) qualifiers
pub const SCSI_SENSEQ_FORMAT_IN_PROGRESS: u8 = 0x04;

// SCSI_ADSENSE_WARNING (0x0B) qualifiers
pub const SCSI_SENSEQ_POWER_LOSS_EXPECTED: u8 = 0x08;

// SCSI_ADSENSE_CRC_OR_ECC_ERROR (0x10) qualifiers
pub const SCSI_SENSEQ_LOGICAL_BLOCK_GUARD_CHECK_FAILED: u8 = 0x01;
pub const SCSI_SENSEQ_LOGICAL_BLOCK_TAG_CHECK_FAILED: u8 = 0x02;
pub const SCSI_SENSEQ_LOGICAL_BLOCK_REF_TAG_CHECK_FAILED: u8 = 0x03;

// SCSI_ADSENSE_ACCESS_DENIED (0x20) qualifiers
pub const SCSI_SENSEQ_NO_ACCESS_RIGHTS: u8 = 0x02;
pub const SCSI_SENSEQ_INVALID_LU_ID: u8 = 0x09;

// SCSI_ADSENSE_PARAMETERS_CHANGED (0x2A) qualifiers
pub const SCSI_SENSEQ_CAPACITY_DATA_CHANGED: u8 = 0x09;

// SCSI_ADSENSE_INVALID_MEDIA (0x30) qualifiers
pub const SCSI_SENSEQ_INCOMPATIBLE_FORMAT: u8 = 0x02;

// SCSI_ADSENSE_OPERATING_CONDITIONS_CHANGED (0x3f) qualifiers
pub const SCSI_SENSEQ_OPERATING_DEFINITION_CHANGED: u8 = 0x02;

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
    pub enum ScsiStatus: u8 {
        GOOD = 0x00,
        CHECK_CONDITION = 0x02,
        CONDITION_MET = 0x04,
        BUSY = 0x08,
        INTERMEDIATE = 0x10,
        INTERMEDIATE_COND_MET = 0x14,
        RESERVATION_CONFLICT = 0x18,
        COMMAND_TERMINATED = 0x22,
        QUEUE_FULL = 0x28,
        TASK_ABORTED = 0x40,
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadCapacityData {
    pub logical_block_address: U32BE,
    pub bytes_per_block: U32BE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeSense {
    pub operation_code: ScsiOp,
    pub flags1: u8,
    pub flags2: ModeSenseFlags,
    pub sub_page_code: u8,
    pub allocation_length: u8,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeSense10 {
    pub operation_code: ScsiOp,
    pub flags1: u8,
    pub flags2: ModeSenseFlags,
    pub sub_page_code: u8,
    pub reserved2: [u8; 3],
    pub allocation_length: U16BE,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeSenseFlags {
    #[bits(6)]
    pub page_code: u8,
    #[bits(2)]
    pub pc: u8,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeReadWriteRecoveryPage {
    /*
        UCHAR PageCode : 6;
        UCHAR Reserved1 : 1;
        UCHAR PSBit : 1;
    */
    pub page_code: u8,
    pub page_length: u8,
    /*
       UCHAR DCRBit : 1;
       UCHAR DTEBit : 1;
       UCHAR PERBit : 1;
       UCHAR EERBit : 1;
       UCHAR RCBit : 1;
       UCHAR TBBit : 1;
       UCHAR ARRE : 1;
       UCHAR AWRE : 1;
    */
    pub bit_info: u8,
    pub read_retry_count: u8,
    pub reserved: [u8; 4],
    pub write_retry_count: u8,
    pub reserved2: [u8; 3],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeSenseModePageTimeoutProtect {
    /*
       UCHAR PageCode : 6;
       UCHAR Reserved1 : 1;
       UCHAR PSBit : 1;
    */
    pub page_code: u8,
    pub page_length: u8,
    pub reserved: [u8; 2],
    /*
       UCHAR SWPP : 1;
       UCHAR DISP : 1;
       UCHAR TMOE : 1;
       UCHAR G3Enable : 1;
       UCHAR Reserved3 : 4;
    */
    pub bit_info: u8,
    pub reserved2: u8,
    pub group_one_minimum_timeout: [u8; 2],
    pub group_two_minimum_timeout: [u8; 2],
    pub group_three_timeout: [u8; 2],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PowerConditionPage {
    /*
       UCHAR PageCode : 6;
       UCHAR Reserved1 : 1;
       UCHAR PSBit : 1;
    */
    pub page_code: u8,
    pub page_length: u8,
    pub reserved: u8,
    /*
       UCHAR Standby : 1;
       UCHAR Idle : 1;
       UCHAR Reserved3 : 6;
    */
    pub flags: u8,
    pub idle_timer: U32BE,
    pub standby_timer: U32BE,
}

pub const LIST_OF_MODE_PAGES: [u8; 3] = [
    MODE_PAGE_ERROR_RECOVERY,
    MODE_PAGE_POWER_CONDITION,
    MODE_PAGE_CDVD_INACTIVITY,
];

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeSelect {
    pub operation_code: ScsiOp,
    pub flags: ModeSelectFlags,
    pub reserved2: [u8; 2],
    pub parameter_list_length: u8,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeSelect10 {
    pub operation_code: ScsiOp,
    pub flags: ModeSelectFlags,
    pub reserved2: [u8; 5],
    pub parameter_list_length: U16BE,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeSelectFlags {
    #[bits(1)]
    pub spbit: bool,
    #[bits(7)]
    pub other_flags: u8,
}

pub const MODE_CONTROL_CURRENT_VALUES: u8 = 0x00;
pub const MODE_CONTROL_CHANGEABLE_VALUES: u8 = 0x40;
pub const MODE_CONTROL_DEFAULT_VALUES: u8 = 0x80;
pub const MODE_CONTROL_SAVED_VALUES: u8 = 0xc0;

pub const MODE_PAGE_VENDOR_SPECIFIC: u8 = 0x00;
pub const MODE_PAGE_ERROR_RECOVERY: u8 = 0x01;
pub const MODE_PAGE_DISCONNECT: u8 = 0x02;
pub const MODE_PAGE_FORMAT_DEVICE: u8 = 0x03; // disk
pub const MODE_PAGE_MRW: u8 = 0x03; // cdrom
pub const MODE_PAGE_RIGID_GEOMETRY: u8 = 0x04;
pub const MODE_PAGE_FLEXIBLE: u8 = 0x05; // disk
pub const MODE_PAGE_WRITE_PARAMETERS: u8 = 0x05; // cdrom
pub const MODE_PAGE_VERIFY_ERROR: u8 = 0x07;
pub const MODE_PAGE_CACHING: u8 = 0x08;
pub const MODE_PAGE_PERIPHERAL: u8 = 0x09;
pub const MODE_PAGE_CONTROL: u8 = 0x0A;
pub const MODE_PAGE_MEDIUM_TYPES: u8 = 0x0B;
pub const MODE_PAGE_NOTCH_PARTITION: u8 = 0x0C;
pub const MODE_PAGE_CD_AUDIO_CONTROL: u8 = 0x0E;
pub const MODE_PAGE_DATA_COMPRESS: u8 = 0x0F;
pub const MODE_PAGE_DEVICE_CONFIG: u8 = 0x10;
pub const MODE_PAGE_XOR_CONTROL: u8 = 0x10; // disk
pub const MODE_PAGE_MEDIUM_PARTITION: u8 = 0x11;
pub const MODE_PAGE_ENCLOSURE_SERVICES_MANAGEMENT: u8 = 0x14;
pub const MODE_PAGE_EXTENDED: u8 = 0x15;
pub const MODE_PAGE_EXTENDED_DEVICE_SPECIFIC: u8 = 0x16;
pub const MODE_PAGE_CDVD_FEATURE_SET: u8 = 0x18;
pub const MODE_PAGE_PROTOCOL_SPECIFIC_LUN: u8 = 0x18;
pub const MODE_PAGE_PROTOCOL_SPECIFIC_PORT: u8 = 0x19;
pub const MODE_PAGE_POWER_CONDITION: u8 = 0x1A;
pub const MODE_PAGE_LUN_MAPPING: u8 = 0x1B;
pub const MODE_PAGE_FAULT_REPORTING: u8 = 0x1C;
pub const MODE_PAGE_CDVD_INACTIVITY: u8 = 0x1D; // cdrom
pub const MODE_PAGE_ELEMENT_ADDRESS: u8 = 0x1D;
pub const MODE_PAGE_TRANSPORT_GEOMETRY: u8 = 0x1E;
pub const MODE_PAGE_DEVICE_CAPABILITIES: u8 = 0x1F;
pub const MODE_PAGE_CAPABILITIES: u8 = 0x2A; // cdrom
pub const MODE_PAGE_ALL: u8 = 0x3f;
pub const MODE_SENSE_SAVED_VALUES: u8 = 0xc0;
pub const MODE_SENSE_RETURN_ALL: u8 = 0x3f;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeParameterHeader {
    pub mode_data_length: u8,
    pub medium_type: u8,
    pub device_specific_parameter: u8,
    pub block_descriptor_length: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeParameterHeader10 {
    pub mode_data_length: U16BE,
    pub medium_type: u8,
    pub device_specific_parameter: u8,
    pub reserved: [u8; 2],
    pub block_descriptor_length: U16BE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ModeCachingPage {
    /*
    UCHAR PageCode : 6;
    UCHAR Reserved : 1;
    UCHAR PageSavable : 1;
     */
    pub page_code: u8,
    pub page_length: u8,
    /*
    UCHAR ReadDisableCache : 1;
    UCHAR MultiplicationFactor : 1;
    UCHAR WriteCacheEnable : 1;
    UCHAR Reserved2 : 5;
     */
    pub flags: u8,
    /*
    UCHAR WriteRetensionPriority : 4;
    UCHAR ReadRetensionPriority : 4;
     */
    pub retension_priority: u8,
    pub disable_prefetch_transfer: [u8; 2],
    pub minimum_prefetch: [u8; 2],
    pub maximum_prefetch: [u8; 2],
    pub maximum_prefetch_ceiling: [u8; 2],
}

pub const MODE_CACHING_WRITE_CACHE_ENABLE: u8 = 0x4;
pub const WRITE_CACHE_ENABLE_BYTE_OFFSET: usize = 3;

pub const MODE_DSP_FUA_SUPPORTED: u8 = 0x10;
pub const MODE_DSP_WRITE_PROTECT: u8 = 0x80;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct LunList {
    pub length: U32BE,
    pub reserved: [u8; 4],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct LunListEntry(pub [u8; 8]);

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Cdb10 {
    pub operation_code: ScsiOp,
    pub flags: CdbFlags,
    pub logical_block: U32BE,
    pub reserved2: u8,
    pub transfer_blocks: U16BE,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Cdb6ReadWrite {
    pub operation_code: u8, // 0x08, 0x0A - SCSIOP_READ, SCSIOP_WRITE
    pub logical_block: [u8; 3],
    pub transfer_blocks: u8,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Cdb12 {
    pub operation_code: ScsiOp,
    pub flags: CdbFlags,
    pub logical_block: U32BE,
    pub transfer_blocks: U32BE,
    pub reserved2: u8,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Cdb16 {
    pub operation_code: ScsiOp,
    pub flags: Cdb16Flags,
    pub logical_block: U64BE,
    pub transfer_blocks: U32BE,
    pub reserved2: u8,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Cdb16Atomic {
    pub operation_code: ScsiOp,
    pub flags: Cdb16Flags,
    pub logical_block: U64BE,
    pub atomic_boundary: U16BE,
    pub transfer_blocks: U16BE,
    pub reserved2: u8,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbFlags {
    pub relative_address: bool,
    #[bits(2)]
    pub reserved1: u8,
    pub fua: bool,
    pub disable_page_out: bool,
    #[bits(3)]
    pub protection: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Cdb16Flags {
    #[bits(3)]
    pub reserved1: u8,
    pub fua: bool,
    pub disable_page_out: bool,
    #[bits(3)]
    pub protection: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ServiceActionIn16 {
    pub operation_code: ScsiOp,
    pub service_action: u8,
    pub logical_block: [u8; 8],
    pub allocation_length: [u8; 4],
    pub flags: u8,
    pub control: u8,
}

pub const SERVICE_ACTION_READ_CAPACITY16: u8 = 0x10;
pub const SERVICE_ACTION_GET_LBA_STATUS: u8 = 0x12;
pub const SERVICE_ACTION_GET_PHYSICAL_ELEMENT_STATUS: u8 = 0x17;
pub const SERVICE_ACTION_REMOVE_ELEMENT_AND_TRUNCATE: u8 = 0x18;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadCapacityDataEx {
    pub logical_block_address: U64BE,
    pub bytes_per_block: U32BE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadCapacity16Data {
    pub ex: ReadCapacityDataEx,
    /*
    UCHAR ProtectionEnable : 1;
    UCHAR ProtectionType : 3;
    UCHAR RcBasis  : 2;
    UCHAR Reserved : 2;
    */
    pub flags: u8,
    /*
    UCHAR LogicalPerPhysicalExponent : 4;
    UCHAR ProtectionInfoExponent : 4;
     */
    pub exponents: u8, // low: logical per physical, high: protection info
    /*
    UCHAR LowestAlignedBlock_MSB : 6;
    UCHAR LBPRZ : 1;
    UCHAR LBPME : 1;
    UCHAR LowestAlignedBlock_LSB;
    */
    pub lowest_aligned_block_msb: u8,
    pub lowest_aligned_block_lsb: u8,
    pub reserved: [u8; 16],
}

pub const READ_CAPACITY16_LBPRZ: u8 = 1 << 6;
pub const READ_CAPACITY16_LBPME: u8 = 1 << 7;

// This flag indicates that there is some special behavior required for the
// request.  The precise meaning of the flag depends on the type/context of the
// request.
pub const SRB_FLAGS_MS_SPECIAL_BEHAVIOR: u32 = 0x00008000;
// For SCSI GET LBA STATUS, the special behavior flag indicates that the device
// should only return blocks that are consolidateable as "mapped".  Blocks that
// shouldn't be consolidated should be returned as "unmapped".
pub const SRB_FLAGS_CONSOLIDATEABLE_BLOCKS_ONLY: u32 = SRB_FLAGS_MS_SPECIAL_BEHAVIOR;
// For SCSI UNMAP, the special behavior flag indicates that the request is a
// block-level request. If this flag is not set, it does NOT imply the request
// is a file-level request.
pub const SRB_FLAGS_BLOCK_LEVEL_ONLY: u32 = SRB_FLAGS_MS_SPECIAL_BEHAVIOR;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetLbaStatus {
    pub operation_code: ScsiOp,
    /*
    UCHAR ServiceAction : 5;
    UCHAR Reserved1     : 3;
    */
    pub service_action: u8,
    pub start_lba: U64BE,
    pub allocation_length: U32BE,
    pub reserved: u8,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct LbaStatusDescriptor {
    pub start_lba: U64BE,
    pub logical_block_count: U32BE,
    /*
    UCHAR ProvisioningStatus : 4;
    UCHAR Reserved1 : 4;
    */
    pub provisioning_status: u8,
    pub reserved2: [u8; 3],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct LbaStatusListHeader {
    pub parameter_length: U32BE,
    pub reserved: u32,
}

pub const LBA_STATUS_MAPPED: u8 = 0x0;
pub const LBA_STATUS_DEALLOCATED: u8 = 0x1;
pub const LBA_STATUS_ANCHORED: u8 = 0x2;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Unmap {
    pub operation_code: ScsiOp,
    /*
    UCHAR Anchor        : 1;
    UCHAR Reserved1     : 7;
    */
    pub anchor: u8,
    pub reserved2: [u8; 4],
    /*
    UCHAR GroupNumber   : 5;
    UCHAR Reserved3     : 3;
    */
    pub group_number: u8,
    pub allocation_length: U16BE,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct UnmapBlockDescriptor {
    pub start_lba: U64BE,
    pub lba_count: U32BE,
    pub reserved: [u8; 4],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct UnmapListHeader {
    pub data_length: U16BE,
    pub block_descriptor_data_length: U16BE,
    pub reserved: [u8; 4],
}
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct StartStop {
    pub operation_code: ScsiOp,
    /*
    UCHAR Immediate: 1;
    UCHAR Reserved1 : 4;
    UCHAR LogicalUnitNumber : 3;
     */
    pub immediate: u8,
    pub reserved2: [u8; 2],
    pub flag: StartStopFlags,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct StartStopFlags {
    #[bits(1)]
    pub start: bool,
    #[bits(1)]
    pub load_eject: bool,
    #[bits(6)]
    pub reserved: u8,
}

pub const IMMEDIATE_BIT: u8 = 1;
pub const START_BIT: u8 = 1;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PersistentReserveIn {
    pub operation_code: ScsiOp,
    pub service_action: PersistentReserveServiceActionIn,
    pub reserved2: [u8; 5],
    pub allocation_length: U16BE,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PersistentReserveOut {
    pub operation_code: ScsiOp,
    pub service_action: PersistentReserveServiceActionOut,
    pub type_scope: PersistentReserveTypeScope,
    pub reserved2: [u8; 4],
    pub parameter_list_length: U16BE,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PersistentReserveTypeScope {
    #[bits(4)]
    reserve_type_bits: u8,
    #[bits(4)]
    pub scope: u8,
}

impl PersistentReserveTypeScope {
    pub fn reserve_type(&self) -> ReservationType {
        ReservationType(self.reserve_type_bits())
    }

    pub fn set_reserve_type(&mut self, ty: ReservationType) {
        self.set_reserve_type_bits(ty.0)
    }

    pub fn with_reserve_type(self, ty: ReservationType) -> Self {
        self.with_reserve_type_bits(ty.0)
    }
}

pub const RESERVATION_SCOPE_LU: u8 = 0x00;
pub const RESERVATION_SCOPE_ELEMENT: u8 = 0x02;

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PersistentReserveServiceActionIn {
    #[bits(5)]
    service_action_bits: u8,
    #[bits(3)]
    pub reserved1: u8,
}

impl PersistentReserveServiceActionIn {
    pub fn service_action(&self) -> ServiceActionIn {
        ServiceActionIn(self.service_action_bits())
    }

    pub fn set_service_action(&mut self, act: ServiceActionIn) {
        self.set_service_action_bits(act.0)
    }

    pub fn with_service_action(self, act: ServiceActionIn) -> Self {
        self.with_service_action_bits(act.0)
    }
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PersistentReserveServiceActionOut {
    #[bits(5)]
    service_action_bits: u8,
    #[bits(3)]
    pub reserved1: u8,
}

impl PersistentReserveServiceActionOut {
    pub fn service_action(&self) -> ServiceActionOut {
        ServiceActionOut(self.service_action_bits())
    }

    pub fn set_service_action(&mut self, act: ServiceActionOut) {
        self.set_service_action_bits(act.0)
    }

    pub fn with_service_action(self, act: ServiceActionOut) -> Self {
        self.with_service_action_bits(act.0)
    }
}

//
// PERSISTENT_RESERVE_* definitions
//
open_enum! {
    pub enum ServiceActionIn: u8 {
        READ_KEYS = 0x00,
        READ_RESERVATIONS = 0x01,
        REPORT_CAPABILITIES = 0x02,
        READ_FULL_STATUS = 0x03,
    }
}

open_enum! {
    pub enum ServiceActionOut: u8 {
        REGISTER = 0x00,
        RESERVE = 0x01,
        RELEASE = 0x02,
        CLEAR = 0x03,
        PREEMPT = 0x04,
        PREEMPT_ABORT = 0x05,
        REGISTER_IGNORE_EXISTING = 0x06,
        REGISTER_AND_MOVE = 0x07,
        REPLACE_LOST_RESERVATION = 0x08,
    }
}

open_enum! {
    pub enum ReservationType: u8 {
        WRITE_EXCLUSIVE = 0x01,
        EXCLUSIVE = 0x03,
        WRITE_EXCLUSIVE_REGISTRANTS = 0x05,
        EXCLUSIVE_REGISTRANTS = 0x06,
        WRITE_EXCLUSIVE_ALL_REGISTRANTS = 0x07,
        EXCLUSIVE_ALL_REGISTRANTS = 0x08,
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ProParameterList {
    pub reservation_key: U64BE,
    pub service_action_reservation_key: U64BE,
    pub obsolete: [u8; 4],
    pub flags: ProParameterListFlags,
    pub reserved3: u8,
    pub obsolete2: [u8; 2],
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ProParameterListFlags {
    pub aptpl: bool,
    pub reserved1: bool,
    pub all_target_ports: bool,
    pub specify_initiator_ports: bool,
    #[bits(4)]
    pub reserved2: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SendDiagnosticFlags {
    pub unit_offline: bool,
    pub device_offline: bool,
    pub self_test: bool,
    pub reserved1: bool,
    pub page_format: bool,
    #[bits(3)]
    pub self_test_code: u8,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SendDiagnostic {
    pub op_code: u8, // 0x1D - SCSIOP_SEND_DIAGNOSTIC
    pub flags: SendDiagnosticFlags,
    pub reserved2: u8,
    pub parameter_list_length: U16BE,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PriReportCapabilities {
    pub length: U16BE,
    pub flags: PriReportCapabilitiesFlags,
    pub type_mask: PriReportCapabilitiesTypeMask,
    pub reserved7: [u8; 2],
}

#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PriReportCapabilitiesFlags {
    pub persist_through_power_loss_capable: bool,
    _reserved: bool,
    pub all_target_ports_capable: bool,
    pub specify_initiator_ports_capable: bool,
    pub compatible_reservation_handling: bool,
    #[bits(2)]
    _reserved1: u8,
    pub replace_lost_reservation_capable: bool,
    pub persist_through_power_loss_activated: bool,
    #[bits(3)]
    _reserved2: u8,
    #[bits(3)]
    pub allow_commands: u8,
    pub type_mask_valid: bool,
}

#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PriReportCapabilitiesTypeMask {
    _reserved: bool,
    pub write_exclusive: bool,
    _reserved2: bool,
    pub exclusive_access: bool,
    _reserved3: bool,
    pub write_exclusive_registrants_only: bool,
    pub exclusive_access_registrants_only: bool,
    pub write_exclusive_all_registrants: bool,
    pub exclusive_access_all_registrants: bool,
    #[bits(7)]
    _reserved4: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PriRegistrationListHeader {
    pub generation: U32BE,
    pub additional_length: U32BE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PriFullStatusListHeader {
    pub generation: U32BE,
    pub additional_length: U32BE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PriReservationListHeader {
    pub generation: U32BE,
    pub additional_length: U32BE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PriReservationDescriptor {
    pub reservation_key: U64BE,
    pub obsolete: [u8; 4],
    pub reserved: u8,
    pub type_scope: PersistentReserveTypeScope,
    pub obsolete2: [u8; 2],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PriFullStatusDescriptorHeader {
    pub reservation_key: U64BE,
    pub reserved: [u8; 4],
    pub flags: PriFullStatusDescriptorHeaderFlags,
    pub type_scope: PersistentReserveTypeScope,
    pub reserved2: [u8; 4],
    pub relative_target_port_identifier: U16BE,
    pub additional_descriptor_length: U32BE,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PriFullStatusDescriptorHeaderFlags {
    pub reservation_holder: bool,
    pub all_target_ports: bool,
    #[bits(6)]
    pub _reserved1: u8,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IsoVpdIdentifiers {
    pub id_page: VpdIdentificationDescriptor,
    pub vendor_id: [u8; 8],
    pub context_guid: [u8; 16],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbGetEventStatusNotification {
    pub operation_code: ScsiOp,
    pub flags: GetEventStatusFlags,
    pub reserved: [u8; 2],
    pub notification_class_request: u8,
    pub reserved2: [u8; 2],
    pub event_list_length: U16BE,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetEventStatusFlags {
    #[bits(1)]
    pub immediate: bool,
    #[bits(4)]
    pub reserved: u8,
    #[bits(3)]
    pub lun: u8,
}

// GET_EVENT_STATUS_NOTIFICATION
pub const NOTIFICATION_OPERATIONAL_CHANGE_CLASS_MASK: u8 = 0x02;
pub const NOTIFICATION_POWER_MANAGEMENT_CLASS_MASK: u8 = 0x04;
pub const NOTIFICATION_EXTERNAL_REQUEST_CLASS_MASK: u8 = 0x08;
pub const NOTIFICATION_MEDIA_STATUS_CLASS_MASK: u8 = 0x10;
pub const NOTIFICATION_MULTI_HOST_CLASS_MASK: u8 = 0x20;
pub const NOTIFICATION_DEVICE_BUSY_CLASS_MASK: u8 = 0x40;
pub const NOTIFICATION_NO_CLASS_EVENTS: u8 = 0x0;
pub const NOTIFICATION_OPERATIONAL_CHANGE_CLASS_EVENTS: u8 = 0x1;
pub const NOTIFICATION_POWER_MANAGEMENT_CLASS_EVENTS: u8 = 0x2;
pub const NOTIFICATION_EXTERNAL_REQUEST_CLASS_EVENTS: u8 = 0x3;
pub const NOTIFICATION_MEDIA_STATUS_CLASS_EVENTS: u8 = 0x4;
pub const NOTIFICATION_MULTI_HOST_CLASS_EVENTS: u8 = 0x5;
pub const NOTIFICATION_DEVICE_BUSY_CLASS_EVENTS: u8 = 0x6;

pub const NOTIFICATION_MEDIA_EVENT_NO_CHANGE: u8 = 0x0;
pub const NOTIFICATION_MEDIA_EVENT_EJECT_REQUEST: u8 = 0x1;
pub const NOTIFICATION_MEDIA_EVENT_NEW_MEDIA: u8 = 0x2;
pub const NOTIFICATION_MEDIA_EVENT_MEDIA_REMOVAL: u8 = 0x3;
pub const NOTIFICATION_MEDIA_EVENT_MEDIA_CHANGE: u8 = 0x4;

pub const NOTIFICATION_OPERATIONAL_EVENT_NO_CHANGE: u8 = 0x0;
pub const NOTIFICATION_OPERATIONAL_STATUS_AVAILABLE: u8 = 0x0;
pub const NOTIFICATION_OPERATIONAL_OPCODE_NONE: u8 = 0x0;

pub const NOTIFICATION_POWER_EVENT_NO_CHANGE: u8 = 0x0;
pub const NOTIFICATION_POWER_STATUS_ACTIVE: u8 = 0x1;

pub const NOTIFICATION_EXTERNAL_EVENT_NO_CHANGE: u8 = 0x0;
pub const NOTIFICATION_EXTERNAL_STATUS_READY: u8 = 0x0;

pub const NOTIFICATION_MULTI_HOST_EVENT_NO_CHANGE: u8 = 0x0;
pub const NOTIFICATION_MULTI_HOST_STATUS_READY: u8 = 0x0;

pub const NOTIFICATION_BUSY_EVENT_NO_EVENT: u8 = 0x0;
pub const NOTIFICATION_BUSY_STATUS_NO_EVENT: u8 = 0x0;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NotificationMediaStatus {
    /*
        UCHAR MediaEvent : 4;
        UCHAR Reserved : 4;
    */
    pub media_event: u8,
    pub status_info: MediaFlags,
    pub start_slot: u8,
    pub end_slot: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MediaFlags {
    #[bits(1)]
    pub door_tray_open: bool,
    #[bits(1)]
    pub media_present: bool,
    #[bits(6)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NotificationEventStatusHeader {
    pub event_data_length: U16BE,
    pub flags: EventStatusFlags,
    pub supported_event_classes: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct EventStatusFlags {
    #[bits(3)]
    pub notification_class: u8,
    #[bits(4)]
    pub reserved: u8,
    #[bits(1)]
    pub nea: bool,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NotificationOperationalStatus {
    /*
        UCHAR OperationalEvent : 4;
        UCHAR Reserved1 : 4;
    */
    pub operation_event: u8,
    pub flags: OperationalStatusFlags,
    pub operation: U16BE,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct OperationalStatusFlags {
    #[bits(4)]
    pub operational_status: u8,
    #[bits(3)]
    pub reserved2: u8,
    #[bits(1)]
    pub persistent_prevented: bool,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NotificationPowerStatus {
    /*
       UCHAR PowerEvent : 4;
       UCHAR Reserved : 4;
    */
    pub power_event: u8,
    pub power_status: u8,
    pub reserved: [u8; 2],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NotificationExternalStatus {
    /*
        UCHAR ExternalEvent : 4;
        UCHAR Reserved1 : 4;
    */
    pub external_event: u8,
    pub flags: ExternalStatusFlags,
    pub reserved: [u8; 2],
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ExternalStatusFlags {
    #[bits(4)]
    pub external_status: u8,
    #[bits(3)]
    pub reserved2: u8,
    #[bits(1)]
    pub persistent_prevented: bool,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NotificationMultiHostStatus {
    /*
        UCHAR MultiHostEvent : 4;
        UCHAR Reserved1 : 4;
    */
    pub multi_host_event: u8,
    pub flags: MultiHostStatusFlags,
    pub priority: U16BE,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MultiHostStatusFlags {
    #[bits(4)]
    pub multi_host_status: u8,
    #[bits(3)]
    pub reserved2: u8,
    #[bits(1)]
    pub persistent_prevented: bool,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NotificationBusyStatus {
    /*
        UCHAR DeviceBusyEvent : 4;
        UCHAR Reserved : 4;
    */
    pub device_busy_event: u8,
    pub device_busy_status: u8,
    pub time: U16BE,
}

open_enum! {
    pub enum RequestType: u8 {
        ALL = 0,
        CURRENT = 1,
        ONE = 2,
    }
}

/// CD-Rom feature list
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FeatureNumber {
    FeatureProfileList = 0x0000,
    FeatureCore = 0x0001,
    FeatureMorphing = 0x0002,
    FeatureRemovableMedium = 0x0003,
    FeatureWriteProtect = 0x0004,
    // Reserved                  0x0005 - 0x000f
    FeatureRandomReadable = 0x0010,
    // Reserved                  0x0011 - 0x001c
    FeatureMultiRead = 0x001D,
    FeatureCdRead = 0x001E,
    FeatureDvdRead = 0x001F,
    FeatureRandomWritable = 0x0020,
    FeatureIncrementalStreamingWritable = 0x0021,
    FeatureSectorErasable = 0x0022,
    FeatureFormattable = 0x0023,
    FeatureDefectManagement = 0x0024,
    FeatureWriteOnce = 0x0025,
    FeatureRestrictedOverwrite = 0x0026,
    FeatureCdrwCAVWrite = 0x0027,
    FeatureMrw = 0x0028,
    // Reserved                  0x0029
    FeatureDvdPlusRW = 0x002A,
    FeatureDvdPlusR = 0x002B,
    FeatureRigidRestrictedOverwrite = 0x002C,
    FeatureCdTrackAtOnce = 0x002D,
    FeatureCdMastering = 0x002E,
    FeatureDvdRecordableWrite = 0x002F, // both -R and -RW
    FeatureDDCDRead = 0x0030,
    FeatureDDCDRWrite = 0x0031,
    FeatureDDCDRWWrite = 0x0032,
    // Reserved                  0x0033 - 0x00ff
    FeaturePowerManagement = 0x0100,
    FeatureSMART = 0x0101,
    FeatureEmbeddedChanger = 0x0102,
    FeatureCDAudioAnalogPlay = 0x0103,
    FeatureMicrocodeUpgrade = 0x0104,
    FeatureTimeout = 0x0105,
    FeatureDvdCSS = 0x0106,
    FeatureRealTimeStreaming = 0x0107,
    FeatureLogicalUnitSerialNumber = 0x0108,
    // Reserved                      0x0109
    FeatureDiscControlBlocks = 0x010A,
    FeatureDvdCPRM = 0x010B,
    FeatureFirmwareDate = 0x010C,
    // Reserved                  0x010D - 0xfeff
    // Vendor Unique             0xff00 - 0xffff
    FeatureUnknown,
}

pub const FEATURE_SIZE: u8 = 39;
pub const PROFILE_DVD_ROM: u16 = 0x0010;
pub const PROFILE_CD_ROM: u16 = 0x0008;
pub const ISO_SECTOR_SIZE: u32 = 0x00000800_u32;

pub const LIST_OF_FEATURES: [FeatureNumber; 10] = [
    FeatureNumber::FeatureProfileList,
    FeatureNumber::FeatureCore,
    FeatureNumber::FeatureMorphing,
    FeatureNumber::FeatureRemovableMedium,
    FeatureNumber::FeatureRandomReadable,
    FeatureNumber::FeatureCdRead,
    FeatureNumber::FeatureDvdRead,
    FeatureNumber::FeaturePowerManagement,
    FeatureNumber::FeatureTimeout,
    FeatureNumber::FeatureRealTimeStreaming,
];

impl TryFrom<usize> for FeatureNumber {
    type Error = std::io::Error;

    fn try_from(v: usize) -> Result<Self, Self::Error> {
        Ok(match v {
            0x00 => Self::FeatureProfileList,
            0x01 => Self::FeatureCore,
            0x02 => Self::FeatureMorphing,
            0x03 => Self::FeatureRemovableMedium,
            0x10 => Self::FeatureRandomReadable,
            0x1E => Self::FeatureCdRead,
            0x1F => Self::FeatureDvdRead,
            0x100 => Self::FeaturePowerManagement,
            0x105 => Self::FeatureTimeout,
            0x107 => Self::FeatureRealTimeStreaming,
            _ => Self::FeatureUnknown,
        })
    }
}
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetConfigurationHeader {
    pub data_length: U32BE,
    pub reserved: [u8; 2],
    pub current_profile: U16BE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbGetConfiguration {
    pub operation_code: ScsiOp,
    pub flags: GetConfigurationFlags,
    pub starting_feature: U16BE,
    pub reserved2: [u8; 3],
    pub allocation_length: U16BE,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetConfigurationFlags {
    #[bits(2)]
    request_type_bits: u8,
    #[bits(6)]
    pub reserved: u8,
}

impl GetConfigurationFlags {
    pub fn request_type(&self) -> RequestType {
        RequestType(self.request_type_bits())
    }

    pub fn set_request_type(&mut self, ty: RequestType) {
        self.set_request_type_bits(ty.0)
    }

    pub fn with_request_type(self, ty: RequestType) -> Self {
        self.with_request_type_bits(ty.0)
    }
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetConfigurationFeatureDataProfileList {
    pub header: FeatureHeader,
    pub profile: [FeatureDataProfileList; 2],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataProfileList {
    pub profile_number: U16BE,
    /*
        UCHAR Current                   : 1;
        UCHAR Reserved1                 : 7;
    */
    pub current: u8,
    pub reserved: u8,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureHeader {
    pub feature_code: U16BE,
    pub flags: FeatureHeaderFlags,
    pub additional_length: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureHeaderFlags {
    #[bits(1)]
    pub current: bool,
    #[bits(1)]
    pub persistent: bool,
    #[bits(4)]
    pub version: u8,
    #[bits(2)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataRandomReadable {
    pub header: FeatureHeader,
    pub logical_block_size: U32BE,
    pub blocking: U16BE,
    /*
       UCHAR ErrorRecoveryPagePresent : 1;
       UCHAR Reserved1                : 7;
    */
    pub error_recovery_page_present: u8,
    pub reserved: u8,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataDvdRead {
    pub header: FeatureHeader,
    /*
       UCHAR Multi110                 : 1;
       UCHAR Reserved1                : 7;
    */
    pub multi_110: u8,
    pub reserved: u8,
    /*
        UCHAR DualDashR                : 1;
        UCHAR Reserved3                : 7;
    */
    pub dual_dash_r: u8,
    pub reserved2: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct RealTimeStreamingFlags {
    #[bits(1)]
    pub stream_recording: bool,
    #[bits(1)]
    pub write_speed_in_get_perf: bool,
    #[bits(1)]
    pub write_speed_in_mp2_a: bool,
    #[bits(1)]
    pub set_cdspeed: bool,
    #[bits(1)]
    pub read_buffer_capacity_block: bool,
    #[bits(3)]
    pub reserved1: u8,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataRealTimeStreaming {
    pub header: FeatureHeader,
    pub flags: RealTimeStreamingFlags,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataCore {
    pub header: FeatureHeader,
    pub physical_interface: U32BE,
    /*
        UCHAR DeviceBusyEvent           : 1;
        UCHAR INQUIRY2                  : 1;
        UCHAR Reserved1                 : 6;
    */
    pub device_busy_event: u8,
    pub reserved2: [u8; 3],
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureMorphingFlags {
    #[bits(1)]
    pub asynchronous: bool,
    #[bits(1)]
    pub ocevent: bool,
    #[bits(6)]
    pub reserved1: u8,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataMorphing {
    pub header: FeatureHeader,
    pub flags: FeatureMorphingFlags,
    pub reserved2: [u8; 3],
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct RemovableMediumFlags {
    #[bits(1)]
    pub lockable: bool,
    #[bits(1)]
    pub dbml: bool,
    #[bits(1)]
    pub default_to_prevent: bool,
    #[bits(1)]
    pub eject: bool,
    #[bits(1)]
    pub load: bool,
    #[bits(3)]
    pub loading_mechanism: u8,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataRemovableMedium {
    pub header: FeatureHeader,
    pub flags: RemovableMediumFlags,
    pub reserved2: [u8; 3],
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CDReadFlags {
    #[bits(1)]
    pub cd_text: bool,
    #[bits(1)]
    pub c2_error_data: bool,
    #[bits(5)]
    pub reserved: u8,
    #[bits(1)]
    pub digital_audio_play: bool,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataCdRead {
    pub header: FeatureHeader,
    pub flags: CDReadFlags,
    pub reserved2: [u8; 3],
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataPowerManagement {
    pub header: FeatureHeader,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FeatureDataTimeout {
    pub header: FeatureHeader,
    /*
       UCHAR Group3    : 1;
       UCHAR Reserved1 : 7;
    */
    pub group: u8,
    pub reserved: u8,
    pub unit_length: U16BE,
}

pub const CDROM_READ_TOC_EX_FORMAT_TOC: u8 = 0x00;
pub const CDROM_READ_TOC_EX_FORMAT_SESSION: u8 = 0x01;
pub const CDROM_READ_TOC_EX_FORMAT_FULL_TOC: u8 = 0x02;
pub const CDROM_READ_TOC_EX_FORMAT_PMA: u8 = 0x03;
pub const CDROM_READ_TOC_EX_FORMAT_ATIP: u8 = 0x04;
pub const CDROM_READ_TOC_EX_FORMAT_CDTEXT: u8 = 0x05;
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbReadToc {
    pub operation_code: ScsiOp,
    /*
        UCHAR Reserved0 : 1;
        UCHAR Msf : 1;
        UCHAR Reserved1 : 3;
        UCHAR LogicalUnitNumber : 3;
    */
    pub flag1: ReadTocFlag,
    /*
        UCHAR Format2 : 4;
        UCHAR Reserved2 : 4;
    */
    pub format2: u8,
    pub reserved: [u8; 3],
    pub starting_track: u8,
    pub allocation_length: U16BE,
    /*
        UCHAR Control : 6;
        UCHAR Format : 2;
    */
    pub reserved1: u8,
}
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadTocFlag {
    #[bits(1)]
    pub reserved0: bool,
    #[bits(1)]
    pub msf: bool,
    #[bits(3)]
    pub reserved1: u8,
    #[bits(3)]
    pub location_unit_number: u8,
}
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadTocFormattedToc {
    pub length: U16BE,
    pub first_complete_session: u8,
    pub last_complete_session: u8,
    pub track1: TrackData,
    pub trackaa: TrackData,
}
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TrackData {
    pub reserved: u8,
    pub flag: TrackDataFlag,
    pub track_number: u8,
    pub reserved1: u8,
    pub address: [u8; 4],
}
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TrackDataFlag {
    #[bits(4)]
    pub control: u8,
    #[bits(4)]
    pub adr: u8,
}
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdromTocSessionData {
    // Header
    pub length: U16BE, // add two bytes for this field
    pub first_complete_session: u8,
    pub last_complete_session: u8,
    // One track, representing the first track
    // of the last finished session
    pub track_data: TrackData,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbRequestSense {
    pub operation_code: ScsiOp,
    pub desc: u8,
    pub reserved: [u8; 2],
    pub allocation_length: u8,
    pub control: u8,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbMediaRemoval {
    pub operation_code: ScsiOp,
    /*
        UCHAR Reserved1 : 5;
        UCHAR LogicalUnitNumber : 3;
    */
    pub lun: u8,
    pub reserved: [u8; 2],
    pub flags: MediaRemovalFlags,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MediaRemovalFlags {
    #[bits(1)]
    pub prevent: bool,
    #[bits(1)]
    pub persistent: bool,
    #[bits(6)]
    pub reserved: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadTrackInfoFlag {
    #[bits(2)]
    pub number_type: u8,
    #[bits(1)]
    pub open: bool,
    #[bits(5)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(Copy, Clone, FromBytes)]
pub struct CdbReadTrackInformation {
    pub operation_code: ScsiOp,
    /*
        UCHAR NumberType : 2;
        UCHAR Open: 1;
        UCHAR Reserved1 : 5;
    */
    pub flag: ReadTrackInfoFlag,
    pub logical_track_number: U32BE,
    pub reserved: u8,
    pub allocation_length: U16BE,
    pub control: u8,
}

#[repr(C)]
#[derive(Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct TrackInformation3 {
    pub length: U16BE,
    pub track_number_lsb: u8,
    pub session_number_lsb: u8,
    pub reserved: u8,
    /*
       UCHAR TrackMode : 4;
       UCHAR Copy      : 1;
       UCHAR Damage    : 1;
       UCHAR Reserved5 : 2;
    */
    pub track_mode: u8,
    /*
       UCHAR DataMode      : 4;
       UCHAR FixedPacket   : 1;
       UCHAR Packet        : 1;
       UCHAR Blank         : 1;
       UCHAR ReservedTrack : 1;
    */
    pub data_mode: u8,
    /*
       UCHAR NWA_V     : 1;
       UCHAR LRA_V     : 1;
       UCHAR Reserved6 : 6;
    */
    pub nwa_v: u8,
    pub track_start_address: U32BE,
    pub next_writable_address: U32BE,
    pub free_blocks: U32BE,
    pub fixed_packet_size: U32BE, // blocking factor
    pub track_size: U32BE,
    pub last_recorded_address: U32BE,
    pub track_number_msb: u8,
    pub session_number_msb: u8,
    pub reserved2: [u8; 2],
    pub read_compatibility_lba: U32BE,
}

// Read DVD Structure Definitions and Constants
pub const DVD_FORMAT_LEAD_IN: u8 = 0x00;
pub const DVD_FORMAT_COPYRIGHT: u8 = 0x01;
pub const DVD_FORMAT_DISK_KEY: u8 = 0x02;
pub const DVD_FORMAT_BCA: u8 = 0x03;
pub const DVD_FORMAT_MANUFACTURING: u8 = 0x04;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbReadDVDStructure {
    pub op: u8,
    /*
       UCHAR MediaType : 4;
       UCHAR Reserved1 : 4;
    */
    pub media_type: u8,
    // RMDBlockNumber[4]
    pub reserved: [u8; 4],
    pub layer: u8,
    pub format: u8,
    pub allocation_length: U16BE,
    /*
       UCHAR Reserved3 : 6;
       UCHAR AGID : 2;
    */
    pub reserved3: u8,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadDVDStructurePhysicalFormatInformation {
    pub length: U16BE,
    pub reserved: [u8; 2],
    /*
        UCHAR PartVersion : 4;
        UCHAR DiskCategory : 4;
    */
    pub reserved2: u8,
    /*
        UCHAR MaximumRate : 4;
        UCHAR DiscSize : 4;
    */
    pub maximum_rate: u8,
    /*
        UCHAR LayerType : 4;
        UCHAR Track : 1;
        UCHAR NumberOfLayers : 2;
        UCHAR Reserved1: 1;
    */
    pub layer: u8,
    /*
        UCHAR TrackDensity : 4;
        UCHAR LinearDensity : 4;
    */
    pub reserved3: u8,
    pub reserved4: u8,
    pub starting_physical_sector: [u8; 3],
    pub reserved5: u8,
    pub end_physical_sector: [u8; 3],
    pub reserved6: u8,
    pub end_physical_sector_in_layer0: [u8; 3],
    /*
        UCHAR Reserved5 : 7;
        UCHAR BCA : 1;
    */
    pub bca: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadDVDStructureCopyrightInformation {
    pub data_length: U16BE,
    pub reserved: u8,
    pub copyright_protection_system: u8,
    pub region_management_information: u8,
    pub reserved2: [u8; 2],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadDVDStructureManufacturingStructure {
    pub data_length: U16BE,
    pub reserved: [u8; 2],
}

pub const PERFORMANCE_TYPE_PERFORMANCE_DATA: u8 = 0x00;
pub const PERFORMANCE_TYPE_UNUSABLE_AREA_DATA: u8 = 0x01;
pub const PERFORMANCE_TYPE_DEFECT_STATUS_DATA: u8 = 0x02;
pub const PERFORMANCE_TYPE_WRITE_SPEED_DESCRIPTOR: u8 = 0x03;
pub const PERFORMANCE_TYPE_DBI: u8 = 0x04;
pub const PERFORMANCE_TYPE_DBI_CACHE_ZONE: u8 = 0x05;
pub const PERFORMANCE_EXCEPT_NOMINAL_PERFORMANCE: u8 = 0x0;
pub const PERFORMANCE_EXCEPT_ENTIRE_PERFORMANCE_LIST: u8 = 0x1;
pub const PERFORMANCE_EXCEPT_PERFORMANCE_EXCEPTIONS_ONLY: u8 = 0x2;
pub const PERFORMANCE_1000_BYTES_PER_SECOND: u32 = 1350 * 24;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbGetPerformance {
    pub op: u8,
    pub flags: GetPerformanceFlags,
    pub starting_lba: U32BE,
    pub reserved: [u8; 2],
    pub maximum_number_of_descriptors: U16BE,
    pub data_type: u8,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetPerformanceFlags {
    #[bits(2)]
    pub except: u8,
    #[bits(1)]
    pub write: bool,
    #[bits(2)]
    pub tolerance: u8,
    #[bits(3)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetPerformanceNominalPerformanceDescriptor {
    pub start_lba: U32BE,
    pub start_performance: U32BE,
    pub end_lba: U32BE,
    pub end_performance: U32BE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetPerformanceHeader {
    pub total_data_length: U32BE,
    /*
        UCHAR Except : 1;
        UCHAR Write  : 1;
        UCHAR Reserved0 : 6;
    */
    pub except: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbMechStatus {
    pub op: u8,
    /*
        UCHAR Reserved : 5;
        UCHAR Lun : 3;
        UCHAR Reserved1[6];
    */
    pub reserved1: [u8; 7],
    pub allocation_length: U16BE,
    pub reserved2: u8,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MechanismStatusHeader {
    /*
        UCHAR CurrentSlotLow5 : 5;
        UCHAR ChangerState : 2;
        UCHAR Fault : 1;
    */
    pub reserved: u8,
    pub flags: MechanismStatusHeaderFlags,
    pub current_logical_block_address: [u8; 3],
    pub number_available_slots: u8,
    pub slot_table_length: U16BE,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MechanismStatusHeaderFlags {
    #[bits(3)]
    pub current_slot_high3: u8,
    #[bits(1)]
    pub reserved: bool,
    #[bits(1)]
    pub door_open: bool,
    #[bits(3)]
    pub mechanism_state: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbReadBufferCapacity {
    pub op: u8,
    pub flags: ReadBufferCapacityFlags,
    pub reserved: [u8; 5],
    pub allocation_length: U16BE,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadBufferCapacityFlags {
    #[bits(1)]
    pub block_info: bool,
    #[bits(7)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadBufferCapacityData {
    pub data_length: U16BE,
    pub reserved1: u8,
    /*
        UCHAR BlockDataReturned : 1;
        UCHAR Reserved4         : 7;
    */
    pub block_data_returned: u8,
    pub total_buffer_size: U32BE,
    pub available_buffer_size: U32BE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbReadDiscInformation {
    pub operation_code: ScsiOp,
    pub flags: ReadDiscFlags,
    pub reserved1: [u8; 5],
    pub allocation_length: U16BE,
    pub control: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReadDiscFlags {
    #[bits(3)]
    pub data_type: u8,
    #[bits(5)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DiscInformation {
    pub length: U16BE,
    pub flags1: DiscInfoFlags1,
    pub first_track_number: u8,

    pub number_of_sessions_lsb: u8,
    pub last_session_first_track_lsb: u8,
    pub last_session_last_track_lsb: u8,

    pub flags2: DiscInfoFlags2,
    pub disc_type: u8,
    pub number_of_sessions_msb: u8,
    pub last_session_first_track_msb: u8,
    pub last_session_last_track_msb: u8,

    pub disk_identification: U32BE,
    pub last_session_lead_in: U32BE,
    pub last_possible_lead_out_start_time: U32BE,
    pub disk_bar_code: [u8; 8],

    pub reserved4: u8,
    pub number_opcentries: u8,

    pub speed: U16BE,
    pub opcvalue: [u8; 6],
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DiscInfoFlags1 {
    #[bits(2)]
    pub disc_status: u8,
    #[bits(2)]
    pub last_session_status: u8,
    #[bits(1)]
    pub erasable: bool,
    #[bits(3)]
    pub reserved1: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DiscInfoFlags2 {
    #[bits(2)]
    pub mrw_status: u8,
    #[bits(1)]
    pub mrw_dirty_bit: bool,
    #[bits(2)]
    pub reserved2: u8,
    #[bits(1)]
    pub uru: bool,
    #[bits(1)]
    pub dbc_v: bool,
    #[bits(1)]
    pub did_v: bool,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdbSetStreaming {
    pub operation_code: ScsiOp,
    pub reserved: [u8; 8],
    pub parameter_list_length: U16BE,
    pub control: u8,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetStreamingPerformanceDescriptor {
    pub flags: SetStreamingFlags,
    pub reserved2: [u8; 3],
    pub start_lba: U32BE,
    pub end_lba: U32BE,
    pub read_size: U32BE,
    pub read_time: U32BE,
    pub write_size: U32BE,
    pub write_time: U32BE,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetStreamingFlags {
    #[bits(1)]
    pub mrw: bool,
    #[bits(1)]
    pub exact: bool,
    #[bits(1)]
    pub rdd: bool,
    #[bits(2)]
    pub wrc: u8,
    #[bits(1)]
    pub hie: bool,
    #[bits(2)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReportLuns {
    pub operation_code: ScsiOp,
    pub reserved1: [u8; 5],
    pub allocation_length: U32BE,
    pub reserved2: u8,
    pub control: u8,
}
