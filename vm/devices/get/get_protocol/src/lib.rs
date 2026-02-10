// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protocol used to interact between the Guest and Host via the
//! GET (Guest Emulation Transport)

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use bitfield_struct::bitfield;
use guid::Guid;
use open_enum::open_enum;
use static_assertions::const_assert;
use static_assertions::const_assert_eq;
use std::fmt::Debug;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub mod crash;
pub mod dps_json; // TODO: split into separate crate, so get_protocol can be no_std

/// The vmbus max response size is INCOMING_PACKET_BUFFER_PAGES (currently 12K)
pub const MAX_MESSAGE_SIZE: usize = 12288;

pub const MAX_HEADER_SIZE: usize = 256;

// Maximum payload size for fragmenting VMGS read/writes and saved state.
// (required due to underlying vmbus pipe message size constraints)
pub const MAX_PAYLOAD_SIZE: usize = 8192;

const_assert!(MAX_MESSAGE_SIZE >= MAX_HEADER_SIZE + MAX_PAYLOAD_SIZE);

/// {455C0F1B-D51B-40B1-BEAC-87377FE6E041}
pub const GUEST_EMULATION_DEVICE_ID: Guid = guid::guid!("455c0f1b-d51b-40b1-beac-87377fe6e041");

/// {8DEDD1AA-9056-49E4-BFD6-1BF90DC38EF0}
pub const GUEST_EMULATION_INTERFACE_TYPE: Guid =
    guid::guid!("8dedd1aa-9056-49e4-bfd6-1bf90dc38ef0");

/// {D3E4454D-62AF-44EC-B851-3170915E5F56}
pub const GUEST_EMULATION_INTERFACE_INSTANCE: Guid =
    guid::guid!("d3e4454d-62af-44ec-b851-3170915e5f56");

/// Make protocol version
const fn make_version(major: u16, minor: u16) -> u32 {
    (minor as u32) | ((major as u32) << 16)
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum ProtocolVersion: u32 {
        INVALID = 0,
        RS5 = make_version(1, 0),
        IRON = make_version(3, 0),
        NICKEL_REV2 = make_version(4, 2),
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum MessageVersions: u8 {
        INVALID          = 0,
        HEADER_VERSION_1 = 1,
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum MessageTypes: u8 {
        INVALID            = 0,
        HOST_NOTIFICATION  = 1,
        HOST_REQUEST       = 2,
        HOST_RESPONSE      = 3,
        GUEST_NOTIFICATION = 4,
    }
}

open_enum! {
    /// Guest notification messages.
    ///
    /// These are intended to be "fire-and-forget" messages sent from the Host
    /// to the Guest, without requiring a response.
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum GuestNotifications: u16 {
        INVALID              = 0,
        UPDATE_GENERATION_ID = 1,
        // --- NI ---
        SAVE_GUEST_VTL2_STATE = 2,
        _RESERVED_DO_NOT_USE_3 = 3,
        VPCI_DEVICE_NOTIFICATION = 4,
        MODIFY_VTL2_SETTINGS = 5,
        MODIFY_VTL2_SETTINGS_REV1 = 6,
        // --- GE ---
        BATTERY_STATUS = 7,
        INJECT_DEBUG_INTERRUPT = 8,
        NOTIFY_POST_LIVE_MIGRATION = 9,
    }
}

open_enum! {
    /// Host notification messages.
    ///
    /// These are intended to be "fire-and-forget" messages sent from the Guest
    /// to the Host, without requiring a response.
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum HostNotifications: u16 {
        INVALID   = 0,
        POWER_OFF = 1,
        RESET     = 2,
        EVENT_LOG = 3,
        LOG_TRACE = 4,
        // --- NI ---
        RESTORE_GUEST_VTL2_STATE_COMPLETED = 5,
        MODIFY_VTL2_SETTINGS_COMPLETED     = 6,
        START_VTL0_COMPLETED               = 7,
        VTL_CRASH                          = 8,
        TRIPLE_FAULT                       = 9,
    }
}

open_enum! {
    /// Header ids (Each request has a response of the same ID).
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum HostRequests: u16 {
        INVALID                      = 0,
        VERSION                      = 1,
        TIME                         = 2,
        BIOS_BOOT_FINALIZE           = 3,
        VMGS_GET_DEVICE_INFO         = 4,
        VMGS_READ                    = 5,
        VMGS_WRITE                   = 6,
        VMGS_FLUSH                   = 7,
        IGVM_ATTEST                  = 8,
        // --- RS ---
        GUEST_STATE_PROTECTION_BY_ID = 9,
        VMGS_KEYS_READ               = 10,
        LOG_TRACE                    = 11, // deprecated
        ACTIVITY_TRACE_START         = 12,
        ACTIVITY_TRACE_OP            = 13,
        EVENT_TRACE                  = 14,
        // --- MN ---
        DEVICE_PLATFORM_SETTINGS     = 15, // deprecated in favor of DEVICE_PLATFORM_SETTINGS_V2
        // --- FE ---
        ADVISORY_PLATFORM_SETTINGS   = 16, // deprecated in favor of DEVICE_PLATFORM_SETTINGS_V2
        GUEST_STATE_PROTECTION       = 17,
        // --- NI ---
        DEVICE_PLATFORM_SETTINGS_V2      = 18, // wart: sent in request, but responses comes back as DEVICE_PLATFORM_SETTINGS_V2_REV1
        VPCI_DEVICE_CONTROL              = 19,
        SAVE_GUEST_VTL2_STATE            = 20,
        RESTORE_GUEST_VTL2_STATE         = 21,
        VPCI_DEVICE_BINDING_CHANGE       = 22,
        VGA_PROXY_PCI_READ               = 23,
        VGA_PROXY_PCI_WRITE              = 24,
        _RESERVED_DO_NOT_USE_25          = 25,
        _RESERVED_DO_NOT_USE_26          = 26,
        DEVICE_PLATFORM_SETTINGS_V2_REV1 = 27, // wart: only sent back in *response* to DEVICE_PLATFORM_SETTINGS
        CREATE_RAM_GPA_RANGE             = 28,
        RESET_RAM_GPA_RANGE              = 29,

        // --- Experimental (not yet in Hyper-V) ---
        MAP_FRAMEBUFFER              = 0xFFFF,
        UNMAP_FRAMEBUFFER            = 0xFFFE,
    }
}

pub use header::*;
pub mod header {
    use super::MessageTypes;
    use super::MessageVersions;
    use static_assertions::const_assert_eq;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    use super::GuestNotifications;
    use super::HostNotifications;
    use super::HostRequests;

    /// The raw header pulled off the wire.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
    pub struct HeaderRaw {
        pub message_version: MessageVersions,
        pub message_type: MessageTypes,
        pub message_id: u16,
    }

    // this trait cannot be implemented outside `mod get_protocol::header`, as
    // there are several safety guarantees that must be upheld when specifying
    // the `MessageId` associated type.
    pub trait HeaderMeta: private::Sealed {
        const MESSAGE_TYPE: MessageTypes;
        type MessageId: Copy + IntoBytes + FromBytes + Immutable + KnownLayout + Sized;
    }

    macro_rules! defn_header_meta {
        (
            $(($header_alias:ident => $name:ident, $message_type:ident, $message_id:ident)$(,)*)*
        ) => {
            mod private {
                pub trait Sealed {}
                $(
                    impl Sealed for super::$name {}
                )*
            }

            $(
                #[derive(Copy, Clone, Debug)]
                pub enum $name {}

                impl HeaderMeta for $name {
                    const MESSAGE_TYPE: MessageTypes = MessageTypes::$message_type;
                    type MessageId = $message_id;
                }

                // ensure that all message_ids are u16 sized
                const_assert_eq!(size_of::<u16>(), size_of::<$message_id>());
                // ensure that the resulting header is sized correctly
                const_assert_eq!(4, size_of::<HeaderGeneric<$name>>());

                impl TryFrom<HeaderRaw> for HeaderGeneric<$name> {
                    type Error = ();

                    fn try_from(raw: HeaderRaw) -> Result<HeaderGeneric<$name>, ()> {
                        if raw.message_type != MessageTypes::$message_type {
                            return Err(());
                        }

                        Ok(HeaderGeneric {
                            message_version: raw.message_version,
                            message_type: raw.message_type,
                            message_id: $message_id(raw.message_id),
                        })
                    }
                }

                pub type $header_alias = HeaderGeneric<$name>;
            )*

        };
    }

    defn_header_meta! {
        (HeaderGuestNotification => GuestNotification, GUEST_NOTIFICATION, GuestNotifications),
        (HeaderHostNotification => HostNotification, HOST_NOTIFICATION, HostNotifications),
        (HeaderHostResponse => HostResponse, HOST_RESPONSE, HostRequests),
        (HeaderHostRequest => HostRequest, HOST_REQUEST, HostRequests),
    }

    // TODO: repr(packed) is only needed here to make zerocopy's IntoBytes
    // derive happy. Ideally it wouldn't be needed. We know this type has no
    // padding bytes because of the asserts inside `defn_header_meta!`, and
    // HeaderMeta is sealed, but zerocopy can't see all of that yet.
    #[repr(C, packed)]
    #[derive(Copy, Clone, Debug, FromBytes, Immutable, KnownLayout, PartialEq, IntoBytes)]
    pub struct HeaderGeneric<Meta: HeaderMeta> {
        pub message_version: MessageVersions,
        pub message_type: MessageTypes,
        message_id: Meta::MessageId,
    }

    impl<Meta: HeaderMeta> HeaderGeneric<Meta> {
        pub fn new(message_id: Meta::MessageId) -> Self {
            Self {
                message_version: MessageVersions::HEADER_VERSION_1,
                message_type: Meta::MESSAGE_TYPE,
                message_id,
            }
        }

        // This method is only needed to force a copy of the message_id field to
        // avoid references to potentially unaligned packed fields. message_id
        // will never actually be unaligned in practice, but the compiler can't
        // prove that. This can go away and message_id can be made `pub` when
        // repr(packed) goes away.
        pub fn message_id(&self) -> Meta::MessageId {
            self.message_id
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum LargePayloadState : u32 {
        END = 0,
        MORE = 1,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct PowerOffNotification {
    pub message_header: HeaderHostNotification,
    pub hibernate: ProtocolBool,
    pub _pad: u8,
}

const_assert_eq!(6, size_of::<PowerOffNotification>());

impl PowerOffNotification {
    pub fn new(hibernate: bool) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostNotifications::POWER_OFF),

            hibernate: hibernate.into(),
            _pad: 0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ResetNotification {
    pub message_header: HeaderHostNotification,
}

const_assert_eq!(4, size_of::<ResetNotification>());

impl ResetNotification {
    pub fn new() -> Self {
        Self {
            message_header: HeaderGeneric::new(HostNotifications::RESET),
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EventLogId: u32 {
        INVALID_ID = 0,
        BOOT_SUCCESS = 1,
        BOOT_SUCCESS_SECURE_BOOT_FAILED = 2,
        BOOT_FAILURE = 3,
        BOOT_FAILURE_SECURE_BOOT_FAILED = 4,
        NO_BOOT_DEVICE = 5,
        ATTESTATION_FAILED = 6,
        VMGS_FILE_CLEAR = 7,
        VMGS_INIT_FAILED = 8,
        VMGS_INVALID_FORMAT = 9,
        VMGS_CORRUPT_FORMAT = 10,
        KEY_NOT_RELEASED = 11,
        DEK_DECRYPTION_FAILED = 12,
        WATCHDOG_TIMEOUT_RESET = 13,
        BOOT_ATTEMPT = 14,
        VMGS_ACCESS_FAILED = 15,
        CERTIFICATE_RENEWAL_FAILED = 16,
        TPM_INVALID_STATE = 17,
        TPM_IDENTITY_CHANGE_FAILED = 18,
        WRAPPED_KEY_REQUIRED_BUT_INVALID = 19,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct EventLogNotification {
    pub message_header: HeaderHostNotification,
    pub event_log_id: EventLogId,
}

const_assert_eq!(8, size_of::<EventLogNotification>());

impl EventLogNotification {
    pub fn new(event_log_id: EventLogId) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostNotifications::EVENT_LOG),
            event_log_id,
        }
    }
}

pub const TRACE_MSG_MAX_SIZE: usize = 256;
open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum LogLevel: u32 {
        INVALID = 0,
        CRITICAL = 1,
        ERROR = 2,
        WARNING = 3,
        INFORMATION = 4,
        VERBOSE = 5,
    }
}

impl From<LogLevel> for u8 {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::INVALID => 0,
            LogLevel::CRITICAL => 1,
            LogLevel::ERROR => 2,
            LogLevel::WARNING => 3,
            LogLevel::INFORMATION => 4,
            LogLevel::VERBOSE => 5,
            _ => {
                unreachable!();
            }
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum GuestVtl2SaveRestoreStatus : u16 {
        SUCCESS = 0,
        FAILURE = 1,
        MORE_DATA = 2,
        REQUEST_DATA = 3,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct LogTraceNotification {
    pub message_header: HeaderHostNotification,
    pub level: LogLevel,
    pub message: [u16; TRACE_MSG_MAX_SIZE],
}

const_assert_eq!(520, size_of::<LogTraceNotification>());

pub const VTL_CRASH_PARAMETERS: usize = 5;

/// The transport level VTL crash data to send to the host.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VtlCrashNotification {
    pub message_header: HeaderHostNotification,
    pub vp_index: u32,
    pub last_vtl: u8,
    pub reserved0: u8,
    pub reserved1: u16,
    pub reserved2: u32,
    pub control: u64,
    pub parameters: [u64; VTL_CRASH_PARAMETERS],
}

const_assert_eq!(64, size_of::<VtlCrashNotification>());

impl VtlCrashNotification {
    pub fn new(
        vp_index: u32,
        last_vtl: u8,
        control: u64,
        parameters: [u64; VTL_CRASH_PARAMETERS],
    ) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostNotifications::VTL_CRASH),
            vp_index,
            last_vtl,
            reserved0: 0,
            reserved1: 0,
            reserved2: 0,
            control,
            parameters,
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum TripleFaultType: u32 {
        UNRECOVERABLE_EXCEPTION = 1,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RegisterState {
    pub name: u32,
    pub value: [u8; 16],
}
const_assert_eq!(20, size_of::<RegisterState>());

/// Triple fault notification to send to the host.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TripleFaultNotification {
    pub message_header: HeaderHostNotification,
    pub vp_index: u32,
    pub fault_type: TripleFaultType,
    pub register_count: u32,
}
const_assert_eq!(16, size_of::<TripleFaultNotification>());

impl TripleFaultNotification {
    pub fn new(vp_index: u32, fault_type: TripleFaultType, register_count: u32) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostNotifications::TRIPLE_FAULT),
            vp_index,
            fault_type,
            register_count,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VersionRequest {
    pub message_header: HeaderHostRequest,
    pub version: ProtocolVersion,
}

impl VersionRequest {
    /// Creates new `VersionRequest` message
    pub fn new(version: ProtocolVersion) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VERSION),
            version,
        }
    }
}

const_assert_eq!(8, size_of::<VersionRequest>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VersionResponse {
    pub message_header: HeaderHostResponse,
    pub version_accepted: ProtocolBool,
    pub _pad: u8,
}

impl VersionResponse {
    pub fn new(version_accepted: bool) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VERSION),
            version_accepted: version_accepted.into(),
            _pad: 0,
        }
    }
}

const_assert_eq!(6, size_of::<VersionResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TimeRequest {
    pub message_header: HeaderHostRequest,
}

const_assert_eq!(4, size_of::<TimeRequest>());

impl TimeRequest {
    pub fn new() -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::TIME),
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TimeResponse {
    pub message_header: HeaderHostResponse,
    pub _pad: u32,

    pub vm_reference_time: u64,
    pub utc: i64,
    pub time_zone: i16,
    pub daylight_savings: ProtocolBool,
    pub _pad1: [u8; 5],
}

impl TimeResponse {
    pub fn new(vm_reference_time: u64, utc: i64, time_zone: i16, daylight_savings: bool) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::TIME),
            _pad: 0,

            vm_reference_time,
            utc,
            time_zone,
            daylight_savings: daylight_savings.into(),
            _pad1: [0; 5],
        }
    }
}

const_assert_eq!(32, size_of::<TimeResponse>());

/// Maximum agent data size
pub const IGVM_ATTEST_MSG_REQ_AGENT_DATA_MAX_SIZE: usize = 2048;
/// Maximum attestation report size
pub const IGVM_ATTEST_MSG_REQ_REPORT_MAX_SIZE: usize = 4096;

/// Maximum return pages
pub const IGVM_ATTEST_MSG_MAX_SHARED_GPA: usize = 16;

// Error from the VM worker process in the host when sending an
// attestation request.
pub const IGVM_ATTEST_VMWP_GENERIC_ERROR_CODE: usize = 0xFFFFFFFF;

/// The response payload could be quite large, so pass host
/// previously shared pages to use for response.
/// Use GET response packet to serialize and convey response length.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct IgvmAttestRequest {
    pub message_header: HeaderHostRequest,
    /// Number of GPA
    pub number_gpa: u32,
    /// GPA addresses
    pub shared_gpa: [u64; IGVM_ATTEST_MSG_MAX_SHARED_GPA],
    /// Agent data length
    pub agent_data_length: u32,
    /// Report length
    pub report_length: u32,
    /// Agent data
    pub agent_data: [u8; IGVM_ATTEST_MSG_REQ_AGENT_DATA_MAX_SIZE],
    /// Report
    pub report: [u8; IGVM_ATTEST_MSG_REQ_REPORT_MAX_SIZE],
}

const_assert_eq!(6288, size_of::<IgvmAttestRequest>());

impl IgvmAttestRequest {
    pub fn new(
        shared_gpa: [u64; IGVM_ATTEST_MSG_MAX_SHARED_GPA],
        number_gpa: u32,
        agent_data: [u8; IGVM_ATTEST_MSG_REQ_AGENT_DATA_MAX_SIZE],
        agent_data_length: u32,
        report: [u8; IGVM_ATTEST_MSG_REQ_REPORT_MAX_SIZE],
        report_length: u32,
    ) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::IGVM_ATTEST),
            number_gpa,
            shared_gpa,
            agent_data_length,
            report_length,
            agent_data,
            report,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct IgvmAttestResponse {
    pub message_header: HeaderHostResponse,
    pub length: u32,
}

const_assert_eq!(8, size_of::<IgvmAttestResponse>());

/// This can only be used in PROTOCOL_VERSION_RS5
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct BiosBootFinalizeRequest {
    pub message_header: HeaderHostRequest,
    pub value: u8,
    pub _pad: u8,
}

const_assert_eq!(6, size_of::<BiosBootFinalizeRequest>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct BiosBootFinalizeResponse {
    pub message_header: HeaderHostResponse,
}

impl BiosBootFinalizeResponse {
    pub fn new() -> BiosBootFinalizeResponse {
        BiosBootFinalizeResponse {
            message_header: HeaderGeneric::new(HostRequests::BIOS_BOOT_FINALIZE),
        }
    }
}

const_assert_eq!(4, size_of::<BiosBootFinalizeResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VmgsGetDeviceInfoRequest {
    pub message_header: HeaderHostRequest,
}

impl VmgsGetDeviceInfoRequest {
    pub fn new() -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VMGS_GET_DEVICE_INFO),
        }
    }
}

const_assert_eq!(4, size_of::<VmgsGetDeviceInfoRequest>());

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum VmgsIoStatus: u32 {
        SUCCESS         = 0,
        INVALID_COMMAND = 1,
        DEVICE_ERROR    = 2,
        RETRY           = 3,
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum VmgsWriteFlags: u32 {
        NONE          = 0,
        WRITE_THROUGH = 0x00000001,
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum VmgsReadFlags: u32 {
        NONE = 0,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VmgsGetDeviceInfoResponse {
    pub message_header: HeaderHostResponse,
    pub status: VmgsIoStatus,
    pub capacity: u64, // logical sectors
    pub bytes_per_logical_sector: u16,
    pub bytes_per_physical_sector: u16,
    pub maximum_transfer_size_bytes: u32,
}

impl VmgsGetDeviceInfoResponse {
    pub fn new(
        status: VmgsIoStatus,
        capacity: u64,
        bytes_per_logical_sector: u16,
        bytes_per_physical_sector: u16,
        maximum_transfer_size_bytes: u32,
    ) -> VmgsGetDeviceInfoResponse {
        VmgsGetDeviceInfoResponse {
            message_header: HeaderGeneric::new(HostRequests::VMGS_GET_DEVICE_INFO),

            status,
            capacity,
            bytes_per_logical_sector,
            bytes_per_physical_sector,
            maximum_transfer_size_bytes,
        }
    }
}

const_assert_eq!(24, size_of::<VmgsGetDeviceInfoResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VmgsWriteRequest {
    pub message_header: HeaderHostRequest,
    pub flags: VmgsWriteFlags,
    pub sector_offset: u64, // logical sectors
    pub sector_count: u32,  // logical sectors
    pub _pad: u32,
    // Variable size payload follows
}

const_assert_eq!(24, size_of::<VmgsWriteRequest>());

impl VmgsWriteRequest {
    pub fn new(flags: VmgsWriteFlags, sector_offset: u64, sector_count: u32) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VMGS_WRITE),
            flags,
            sector_offset,
            sector_count,
            _pad: 0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VmgsWriteResponse {
    pub message_header: HeaderHostResponse,
    pub status: VmgsIoStatus,
}

impl VmgsWriteResponse {
    pub fn new(status: VmgsIoStatus) -> VmgsWriteResponse {
        VmgsWriteResponse {
            message_header: HeaderGeneric::new(HostRequests::VMGS_WRITE),
            status,
        }
    }
}

const_assert_eq!(8, size_of::<VmgsWriteResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VmgsReadRequest {
    pub message_header: HeaderHostRequest,
    pub flags: VmgsReadFlags,
    pub sector_offset: u64, // logical sectors
    pub sector_count: u32,  // logical sectors
    pub _pad: u32,
}

const_assert_eq!(24, size_of::<VmgsReadRequest>());

impl VmgsReadRequest {
    pub fn new(flags: VmgsReadFlags, sector_offset: u64, sector_count: u32) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VMGS_READ),
            flags,
            sector_offset,
            sector_count,
            _pad: 0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VmgsReadResponse {
    pub message_header: HeaderHostResponse,
    pub status: VmgsIoStatus,
    // Variable size payload follows
}

impl VmgsReadResponse {
    pub fn new(status: VmgsIoStatus) -> VmgsReadResponse {
        VmgsReadResponse {
            message_header: HeaderGeneric::new(HostRequests::VMGS_READ),
            status,
        }
    }
}

const_assert_eq!(8, size_of::<VmgsReadResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VmgsFlushRequest {
    pub message_header: HeaderHostRequest,
}

impl VmgsFlushRequest {
    pub fn new() -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VMGS_FLUSH),
        }
    }
}

const_assert_eq!(4, size_of::<VmgsFlushRequest>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VmgsFlushResponse {
    pub message_header: HeaderHostResponse,
    pub status: VmgsIoStatus,
}

impl VmgsFlushResponse {
    pub fn new(status: VmgsIoStatus) -> VmgsFlushResponse {
        VmgsFlushResponse {
            message_header: HeaderGeneric::new(HostRequests::VMGS_FLUSH),
            status,
        }
    }
}

const_assert_eq!(8, size_of::<VmgsFlushResponse>());

const VMGS_MAX_IO_MSG_HEADER_SIZE: usize = size_of::<VmgsReadRequest>();
const_assert!(VMGS_MAX_IO_MSG_HEADER_SIZE >= size_of::<VmgsReadResponse>());
const_assert!(VMGS_MAX_IO_MSG_HEADER_SIZE >= size_of::<VmgsWriteRequest>());
const_assert!(VMGS_MAX_IO_MSG_HEADER_SIZE >= size_of::<VmgsWriteResponse>());
const_assert!(VMGS_MAX_IO_MSG_HEADER_SIZE <= MAX_HEADER_SIZE);

pub const MAX_TRANSFER_SIZE: usize = u32::MAX as usize - VMGS_MAX_IO_MSG_HEADER_SIZE;

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum ActivityClassId: u32 {
        INVALID = 0,
        VMGS_INITIALIZE_DEVICE = 1,
        VMGS_INITIALIZE_STORE = 2,
        VMGS_OPEN_STORE = 3,
        VMGS_FORMAT = 4,
        VMGS_SEND_RECEIVE = 5,
        VMGS_DEVICE_READ = 6,
        VMGS_DEVICE_WRITE = 7,
        VMGS_DEVICE_FLUSH = 8,
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum ActivityOpCode: u32 {
        INFO = 0,
        START = 1,
        STOP = 2,
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EventId: u32 {
        INVALID = 0,
        VMGS_INFO = 1,
        VMGS_WARNING = 2,
        VMGS_ERROR = 3,
        VMGS_CRITICAL = 4,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct LogTraceRequest {
    pub message_header: HeaderHostRequest,
    pub level: LogLevel,
    pub message: [u16; TRACE_MSG_MAX_SIZE],
}

const_assert_eq!(520, size_of::<LogTraceRequest>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct LogTraceResponse {
    pub message_header: HeaderHostResponse,
}

const_assert_eq!(4, size_of::<LogTraceResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ActivityTraceStartRequest {
    pub message_header: HeaderHostRequest,
    pub activity_class: ActivityClassId,
    pub related_activity_id: Guid,
    pub size: u32, // Size of payload
                   // Variable size payload follows, maximum TRACE_MSG_MAX_SIZE WCHAR
}

const_assert_eq!(28, size_of::<ActivityTraceStartRequest>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ActivityTraceStartResponse {
    pub message_header: HeaderHostResponse,
    pub activity_id: Guid,
}

const_assert_eq!(20, size_of::<ActivityTraceStartResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ActivityTraceOpRequest {
    pub message_header: HeaderHostRequest,
    pub activity_class: ActivityClassId,
    pub activity_id: Guid,
    pub related_activity_id: Guid,
    pub op_code: ActivityOpCode,
    pub size: u32, // Size of payload
                   // Variable size payload follows, maximum TRACE_MSG_MAX_SIZE_WCHAR
}

const_assert_eq!(48, size_of::<ActivityTraceOpRequest>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ActivityTraceOpResponse {
    pub message_header: HeaderHostResponse,
}

const_assert_eq!(4, size_of::<ActivityTraceOpResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct EventTraceRequest {
    pub message_header: HeaderHostRequest,
    pub event: EventId,
    pub size: u32, // Size of payload
                   // Variable size payload follows, maximum TRACE_MSG_MAX_SIZE WCHAR
}

const_assert_eq!(12, size_of::<EventTraceRequest>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct EventTraceResponse {
    pub message_header: HeaderHostResponse,
}

const_assert_eq!(4, size_of::<EventTraceResponse>());

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum SecureBootTemplateType: u32 {
        SECURE_BOOT_DISABLED = 0,
        MICROSOFT_WINDOWS = 1,
        MICROSOFT_UEFI_CERTIFICATE_AUTHORITY = 2,
        OPEN_SOURCE_SHIELDED_VM = 3,
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum UefiConsoleMode: u8 {
        DEFAULT = 0,
        COM1 = 1,
        COM2 = 2,
        NONE = 3,
    }
}

pub const HCL_DEVICE_PLATFORM_MAX_SMBIOS_LENGTH: usize = 64;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct DevicePlatformSettingsRequestV2 {
    pub message_header: HeaderHostRequest,
}

impl DevicePlatformSettingsRequestV2 {
    pub fn new() -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::DEVICE_PLATFORM_SETTINGS_V2),
        }
    }
}

const_assert_eq!(4, size_of::<DevicePlatformSettingsRequestV2>());

/// This represents a boolean value sent over the protocol as a u8. The only
/// valid values are 0 or 1.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ProtocolBool(pub u8);

impl From<bool> for ProtocolBool {
    fn from(value: bool) -> Self {
        ProtocolBool(if value { 1 } else { 0 })
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct DevicePlatformSettingsResponseV2 {
    pub message_header: HeaderHostResponse,

    pub size: u32,
    // variable length JSON payload
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct DevicePlatformSettingsResponseV2Rev1 {
    pub message_header: HeaderHostResponse,

    pub size: u32,

    pub payload_state: LargePayloadState,
    // variable length JSON payload
}

pub const GSP_CLEARTEXT_MAX: u32 = 32;
pub const GSP_CIPHERTEXT_MAX: u32 = 512;
pub const NUMBER_GSP: u32 = 2;

#[bitfield(u32)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
pub struct GspExtendedStatusFlags {
    pub state_refresh_request: bool,
    pub no_registry_file: bool,
    pub no_rpc_server: bool,
    pub allow_ak_cert_renewal: bool,
    pub requires_rpc_server: bool,

    #[bits(27)]
    _reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GspCleartextContent {
    pub length: u32,
    pub buffer: [u8; GSP_CLEARTEXT_MAX as usize * 2],
}

const_assert_eq!(68, size_of::<GspCleartextContent>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GspCiphertextContent {
    pub length: u32,
    pub buffer: [u8; GSP_CIPHERTEXT_MAX as usize],
}

const_assert_eq!(516, size_of::<GspCiphertextContent>());

/// This can only be used in PROTOCOL_VERSION_RS5
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GuestStateProtectionByIdRequest {
    pub message_header: HeaderHostRequest,
}

const_assert_eq!(4, size_of::<GuestStateProtectionByIdRequest>());

impl GuestStateProtectionByIdRequest {
    pub fn new() -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::GUEST_STATE_PROTECTION_BY_ID),
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GuestStateProtectionByIdResponse {
    pub message_header: HeaderHostResponse,
    pub seed: GspCleartextContent,
    pub extended_status_flags: GspExtendedStatusFlags,
}

const_assert_eq!(76, size_of::<GuestStateProtectionByIdResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GuestStateProtectionRequest {
    pub message_header: HeaderHostRequest,
    pub new_gsp: GspCleartextContent,
    pub encrypted_gsp: [GspCiphertextContent; NUMBER_GSP as usize],
    pub extended_status_support_flags: GspExtendedStatusFlags,
}

const_assert_eq!(1108, size_of::<GuestStateProtectionRequest>());

impl GuestStateProtectionRequest {
    pub fn new(
        buffer: [u8; GSP_CLEARTEXT_MAX as usize * 2],
        encrypted_gsp: [GspCiphertextContent; NUMBER_GSP as usize],
        extended_status_support_flags: GspExtendedStatusFlags,
    ) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::GUEST_STATE_PROTECTION),
            new_gsp: GspCleartextContent {
                length: GSP_CLEARTEXT_MAX,
                buffer,
            },
            encrypted_gsp,
            extended_status_support_flags,
        }
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct GuestStateProtectionResponse {
    pub message_header: HeaderHostResponse,
    pub encrypted_gsp: GspCiphertextContent,
    pub decrypted_gsp: [GspCleartextContent; NUMBER_GSP as usize],
    pub extended_status_flags: GspExtendedStatusFlags,
}

const_assert_eq!(660, size_of::<GuestStateProtectionResponse>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct UpdateGenerationId {
    pub message_header: HeaderGuestNotification,
    pub _pad: u32,
    pub generation_id: [u8; 16],
}

const_assert_eq!(24, size_of::<UpdateGenerationId>());

impl UpdateGenerationId {
    pub fn new(generation_id: [u8; 16]) -> Self {
        Self {
            message_header: HeaderGeneric::new(GuestNotifications::UPDATE_GENERATION_ID),
            _pad: 0,
            generation_id,
        }
    }
}

/// Bitfield describing SaveGuestVtl2StateNotification::capabilities_flags
#[bitfield(u64)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct SaveGuestVtl2StateFlags {
    /// Explicitly allow nvme_keepalive feature when servicing.
    #[bits(1)]
    pub enable_nvme_keepalive: bool,

    /// Explicitly allow mana_keepalive feature when servicing.
    #[bits(1)]
    pub enable_mana_keepalive: bool,

    /// Reserved, must be zero.
    #[bits(62)]
    _rsvd1: u64,
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct SaveGuestVtl2StateNotification {
    pub message_header: HeaderGuestNotification,
    pub correlation_id: Guid,
    pub capabilities_flags: SaveGuestVtl2StateFlags,
    pub timeout_hint_secs: u16,
}

const_assert_eq!(30, size_of::<SaveGuestVtl2StateNotification>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct SaveGuestVtl2StateRequest {
    pub message_header: HeaderHostRequest,
    pub save_status: GuestVtl2SaveRestoreStatus,
    // Variable-length payload follows
}

const_assert_eq!(6, size_of::<SaveGuestVtl2StateRequest>());

impl SaveGuestVtl2StateRequest {
    pub fn new(status: GuestVtl2SaveRestoreStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::SAVE_GUEST_VTL2_STATE),
            save_status: status,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct SaveGuestVtl2StateResponse {
    pub message_header: HeaderHostResponse,
    pub save_status: GuestVtl2SaveRestoreStatus,
}

const_assert_eq!(6, size_of::<SaveGuestVtl2StateResponse>());

impl SaveGuestVtl2StateResponse {
    pub fn new(status: GuestVtl2SaveRestoreStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::SAVE_GUEST_VTL2_STATE),
            save_status: status,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RestoreGuestVtl2StateHostNotification {
    pub message_header: HeaderHostNotification,
    pub status: GuestVtl2SaveRestoreStatus,
}

const_assert_eq!(6, size_of::<RestoreGuestVtl2StateHostNotification>());

impl RestoreGuestVtl2StateHostNotification {
    pub fn new(stat: GuestVtl2SaveRestoreStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(
                HostNotifications::RESTORE_GUEST_VTL2_STATE_COMPLETED,
            ),
            status: stat,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RestoreGuestVtl2StateRequest {
    pub message_header: HeaderHostRequest,
    pub restore_status: GuestVtl2SaveRestoreStatus,
}

const_assert_eq!(6, size_of::<RestoreGuestVtl2StateRequest>());

impl RestoreGuestVtl2StateRequest {
    pub fn new(status: GuestVtl2SaveRestoreStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::RESTORE_GUEST_VTL2_STATE),
            restore_status: status,
        }
    }
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct RestoreGuestVtl2StateResponse {
    pub message_header: HeaderHostResponse,
    pub data_length: u32,
    pub restore_status: GuestVtl2SaveRestoreStatus,
    // Variable-length payload follows
}

const_assert_eq!(10, size_of::<RestoreGuestVtl2StateResponse>());

impl RestoreGuestVtl2StateResponse {
    pub fn new(length: u32, status: GuestVtl2SaveRestoreStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::RESTORE_GUEST_VTL2_STATE),
            data_length: length,
            restore_status: status,
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum VpciDeviceControlCode: u32 {
        UNDEFINED = 0,
        OFFER = 1,
        REVOKE = 2,
        RESET = 3,
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum VpciDeviceControlStatus: u32 {
        SUCCESS = 0,
        INVALID_REQUEST = 1,
        DEVICE_NOT_FOUND = 2,
        INVALID_DEVICE_STATE = 3,
        GENERIC_FAILURE = 4,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VpciDeviceControlRequest {
    pub message_header: HeaderHostRequest,
    pub code: VpciDeviceControlCode,
    pub bus_instance_id: Guid,
}

const_assert_eq!(24, size_of::<VpciDeviceControlRequest>());

impl VpciDeviceControlRequest {
    pub fn new(code: VpciDeviceControlCode, bus_instance_id: Guid) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VPCI_DEVICE_CONTROL),
            code,
            bus_instance_id,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VpciDeviceControlResponse {
    pub message_header: HeaderHostResponse,
    pub status: VpciDeviceControlStatus,
}

const_assert_eq!(8, size_of::<VpciDeviceControlResponse>());

impl VpciDeviceControlResponse {
    pub fn new(status: VpciDeviceControlStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VPCI_DEVICE_CONTROL),
            status,
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum VpciDeviceNotificationCode : u32 {
        UNDEFINED = 0,
        ENUMERATED = 1,
        PREPARE_FOR_REMOVAL = 2,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VpciDeviceNotification {
    pub message_header: HeaderGuestNotification,
    pub bus_instance_id: Guid,
    pub code: VpciDeviceNotificationCode,
}

const_assert_eq!(24, size_of::<VpciDeviceNotification>());

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VpciDeviceBindingChangeRequest {
    pub message_header: HeaderHostRequest,
    pub bus_instance_id: [u8; 16], // Guid
    pub binding_state: u8,
}

const_assert_eq!(21, size_of::<VpciDeviceBindingChangeRequest>());

impl VpciDeviceBindingChangeRequest {
    pub fn new(bus_instance_id: Guid, binding_state: bool) -> Self {
        let mut guid: [u8; 16] = [0; 16];
        guid.copy_from_slice(bus_instance_id.as_bytes());
        Self {
            message_header: HeaderGeneric::new(HostRequests::VPCI_DEVICE_BINDING_CHANGE),
            bus_instance_id: guid,
            binding_state: binding_state as u8,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VpciDeviceBindingChangeResponse {
    pub message_header: HeaderHostResponse,
    pub bus_instance_id: Guid,
    pub status: VpciDeviceControlStatus,
}

const_assert_eq!(24, size_of::<VpciDeviceBindingChangeResponse>());

impl VpciDeviceBindingChangeResponse {
    pub fn new(bus_instance_id: Guid, status: VpciDeviceControlStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VPCI_DEVICE_BINDING_CHANGE),
            bus_instance_id,
            status,
        }
    }
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VgaProxyPciReadRequest {
    pub message_header: HeaderHostRequest,
    pub offset: u16,
}

const_assert_eq!(6, size_of::<VgaProxyPciReadRequest>());

impl VgaProxyPciReadRequest {
    pub fn new(offset: u16) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VGA_PROXY_PCI_READ),
            offset,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VgaProxyPciReadResponse {
    pub message_header: HeaderHostResponse,
    pub value: u32,
}

const_assert_eq!(8, size_of::<VgaProxyPciReadResponse>());

impl VgaProxyPciReadResponse {
    pub fn new(value: u32) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VGA_PROXY_PCI_READ),
            value,
        }
    }
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VgaProxyPciWriteRequest {
    pub message_header: HeaderHostRequest,
    pub value: u32,
    pub offset: u16,
}

const_assert_eq!(10, size_of::<VgaProxyPciWriteRequest>());

impl VgaProxyPciWriteRequest {
    pub fn new(offset: u16, value: u32) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VGA_PROXY_PCI_WRITE),
            offset,
            value,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct VgaProxyPciWriteResponse {
    pub message_header: HeaderHostResponse,
}

const_assert_eq!(4, size_of::<VgaProxyPciWriteResponse>());

impl VgaProxyPciWriteResponse {
    pub fn new() -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::VGA_PROXY_PCI_WRITE),
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum ModifyVtl2SettingsStatus : u32 {
        SUCCESS = 0,
        FAILURE = 1,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ModifyVtl2SettingsNotification {
    pub message_header: HeaderGuestNotification,

    pub size: u32,
    // variable length JSON payload
}

const_assert_eq!(8, size_of::<ModifyVtl2SettingsNotification>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ModifyVtl2SettingsRev1Notification {
    pub message_header: HeaderGuestNotification,

    pub size: u32,

    pub payload_state: LargePayloadState,
    // variable length JSON payload
}

const_assert_eq!(12, size_of::<ModifyVtl2SettingsRev1Notification>());

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ModifyVtl2SettingsCompleteNotification {
    pub message_header: HeaderHostNotification,
    pub modify_status: ModifyVtl2SettingsStatus,
    pub result_document_size: u32,
}

const_assert_eq!(12, size_of::<ModifyVtl2SettingsCompleteNotification>());

impl ModifyVtl2SettingsCompleteNotification {
    pub fn new(status: ModifyVtl2SettingsStatus, result_document_size: u32) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostNotifications::MODIFY_VTL2_SETTINGS_COMPLETED),
            modify_status: status,
            result_document_size,
        }
    }
}

pub const GET_LOG_INTERFACE_GUID: Guid = guid::guid!("AA5DE534-D149-487A-9053-05972BA20A7C");

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum LogType: u8 {
        EVENT = 0,
        SPAN_ENTER = 1,
        SPAN_EXIT = 2,
    }
}

#[bitfield(u16)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct LogFlags {
    pub kmsg: bool,
    #[bits(15)]
    pub mbz0: u16,
}

pub const TRACE_LOGGING_NAME_MAX_SIZE: usize = 128;
pub const TRACE_LOGGING_TARGET_MAX_SIZE: usize = 128;
pub const TRACE_LOGGING_FIELDS_MAX_SIZE: usize = 256;
pub const TRACE_LOGGING_MESSAGE_MAX_SIZE: usize = 4096;
pub const TRACE_LOGGING_NOTIFICATION_MAX_SIZE: usize = TRACE_LOGGING_NAME_MAX_SIZE
    + TRACE_LOGGING_TARGET_MAX_SIZE
    + TRACE_LOGGING_FIELDS_MAX_SIZE
    + TRACE_LOGGING_MESSAGE_MAX_SIZE
    + size_of::<TraceLoggingNotificationHeader>();

#[repr(C)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TraceLoggingBufferOffset {
    pub size: u16,
    pub offset: u16,
}

// The header is followed by a buffer which holds the string fields.
//
//  ---------------------------------------------------------
//  |  Header | Buffer [ name | target | fields | message ] |
//  ---------------------------------------------------------
#[repr(C)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct TraceLoggingNotificationHeader {
    pub log_type: LogType,
    pub level: u8,
    pub flags: LogFlags,
    pub name: TraceLoggingBufferOffset,
    pub target: TraceLoggingBufferOffset,
    pub fields: TraceLoggingBufferOffset,
    pub message: TraceLoggingBufferOffset,
    pub mbz0: u32,
    pub activity_id: Guid,
    pub related_activity_id: Guid,
    pub correlation_id: Guid,
    pub timestamp: u64,
}
const_assert_eq!(80, size_of::<TraceLoggingNotificationHeader>());

/// MAP_FRAMEBUFFER_REQUEST
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MapFramebufferRequest {
    pub message_header: HeaderHostRequest,
    pub gpa: u64,
}

const_assert_eq!(12, size_of::<MapFramebufferRequest>());

impl MapFramebufferRequest {
    pub fn new(gpa: u64) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::MAP_FRAMEBUFFER),
            gpa,
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum MapFramebufferStatus : u32 {
        SUCCESS = 0,
        FAILURE = 1,
    }
}

/// MAP_FRAMEBUFFER_RESPONSE
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MapFramebufferResponse {
    pub message_header: HeaderHostResponse,
    pub status: MapFramebufferStatus,
}

const_assert_eq!(8, size_of::<MapFramebufferResponse>());

impl MapFramebufferResponse {
    pub fn new(status: MapFramebufferStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::MAP_FRAMEBUFFER),
            status,
        }
    }
}

/// UNMAP_FRAMEBUFFER
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct UnmapFramebufferRequest {
    pub message_header: HeaderHostRequest,
}

const_assert_eq!(4, size_of::<UnmapFramebufferRequest>());

impl UnmapFramebufferRequest {
    pub fn new() -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::UNMAP_FRAMEBUFFER),
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum UnmapFramebufferStatus : u32 {
        SUCCESS = 0,
        FAILURE = 1,
    }
}

/// UNMAP_FRAMEBUFFER_RESPONSE
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct UnmapFramebufferResponse {
    pub message_header: HeaderHostResponse,
    pub status: UnmapFramebufferStatus,
}

const_assert_eq!(8, size_of::<UnmapFramebufferResponse>());

impl UnmapFramebufferResponse {
    pub fn new(status: UnmapFramebufferStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::UNMAP_FRAMEBUFFER),
            status,
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum StartVtl0Status : u32 {
        SUCCESS = 0,
        FAILURE = 1,
    }
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct StartVtl0CompleteNotification {
    pub message_header: HeaderHostNotification,
    pub status: StartVtl0Status,
    pub result_document_size: u32,
    // result_document is a variable size raw string because the Error is anyhow::Error
    // TODO: use specific Errors for "start VM"
}

const_assert_eq!(12, size_of::<StartVtl0CompleteNotification>());

impl StartVtl0CompleteNotification {
    pub fn new(status: StartVtl0Status, result_document_size: u32) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostNotifications::START_VTL0_COMPLETED),
            status,
            result_document_size,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct BatteryStatusNotification {
    pub message_header: HeaderGuestNotification,
    pub flags: BatteryStatusFlags,
    pub max_capacity: u32,
    pub remaining_capacity: u32,
    pub rate: u32,
}

#[bitfield(u32)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct BatteryStatusFlags {
    pub ac_online: bool,
    pub battery_present: bool,
    pub charging: bool,
    pub discharging: bool,
    #[bits(28)]
    pub reserved: u32,
}

impl BatteryStatusNotification {
    pub fn new(
        flags: BatteryStatusFlags,
        max_capacity: u32,
        remaining_capacity: u32,
        rate: u32,
    ) -> Self {
        Self {
            message_header: HeaderGeneric::new(GuestNotifications::BATTERY_STATUS),
            flags,
            max_capacity,
            remaining_capacity,
            rate,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct InjectDebugInterruptNotification {
    pub message_header: HeaderGuestNotification,
    pub vtl: u8,
    pub _pad: u8,
}
const_assert_eq!(6, size_of::<InjectDebugInterruptNotification>());

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct PostLiveMigrationNotification {
    pub message_header: HeaderGuestNotification,
}
const_assert_eq!(4, size_of::<PostLiveMigrationNotification>());

#[bitfield(u64)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct CreateRamGpaRangeFlags {
    _reserved1: bool,

    /// Writes are discarded
    pub rom_mb: bool,

    #[bits(62)]
    _reserved: u64,
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct CreateRamGpaRangeRequest {
    pub message_header: HeaderHostRequest,
    pub slot: u32,
    pub gpa_start: u64,
    pub gpa_count: u64,
    pub gpa_offset: u64,
    pub flags: CreateRamGpaRangeFlags,
}

const_assert_eq!(40, size_of::<CreateRamGpaRangeRequest>());

impl CreateRamGpaRangeRequest {
    pub fn new(
        slot: u32,
        gpa_start: u64,
        gpa_count: u64,
        gpa_offset: u64,
        flags: CreateRamGpaRangeFlags,
    ) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::CREATE_RAM_GPA_RANGE),
            slot,
            gpa_start,
            gpa_count,
            gpa_offset,
            flags,
        }
    }
}

open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum CreateRamGpaRangeStatus : u32 {
        SUCCESS = 0,
        /// slot index out of bounds
        SLOT_OUT_OF_BOUNDS = 1,
        /// slot is already occupied. needs to be reset first
        SLOT_OCCUPIED = 2,
        /// invalid flag
        INVALID_FLAG = 3,
        /// invalid GPA
        INVALID_GPA = 4,
        /// call to CreateRamGpaRange failed
        FAILED = 5,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct CreateRamGpaRangeResponse {
    pub message_header: HeaderHostResponse,
    pub status: CreateRamGpaRangeStatus,
}

const_assert_eq!(8, size_of::<CreateRamGpaRangeResponse>());

impl CreateRamGpaRangeResponse {
    pub fn new(status: CreateRamGpaRangeStatus) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::CREATE_RAM_GPA_RANGE),
            status,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ResetRamGpaRangeRequest {
    pub message_header: HeaderHostRequest,
    pub slot: u32,
}

const_assert_eq!(8, size_of::<ResetRamGpaRangeRequest>());

impl ResetRamGpaRangeRequest {
    pub fn new(slot: u32) -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::RESET_RAM_GPA_RANGE),
            slot,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct ResetRamGpaRangeResponse {
    pub message_header: HeaderHostResponse,
}

const_assert_eq!(4, size_of::<ResetRamGpaRangeResponse>());

impl ResetRamGpaRangeResponse {
    pub fn new() -> Self {
        Self {
            message_header: HeaderGeneric::new(HostRequests::RESET_RAM_GPA_RANGE),
        }
    }
}

pub mod test_utilities {
    // These constants are shared across GED and GET testing
    pub const TEST_VMGS_SECTOR_SIZE: u32 = 512;
    pub const TEST_VMGS_CAPACITY: usize = 4194816; // 4 MB
}
