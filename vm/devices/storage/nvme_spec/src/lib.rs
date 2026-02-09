// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions from the NVMe specifications:
//!
//! Base 2.0c: <https://nvmexpress.org/wp-content/uploads/NVM-Express-Base-Specification-2.0c-2022.10.04-Ratified.pdf>
//! PCIe transport 1.0c: <https://nvmexpress.org/wp-content/uploads/NVM-Express-PCIe-Transport-Specification-1.0c-2022.10.03-Ratified.pdf>

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![no_std]

pub mod nvm;

use bitfield_struct::bitfield;
use inspect::Inspect;
use mesh::MeshPayload;
use open_enum::open_enum;
use storage_string::AsciiString;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

type U128LE = zerocopy::U128<zerocopy::LE>;

open_enum! {
    pub enum Register: u16 {
        CAP = 0x0,
        VS = 0x8,
        INTMS = 0xc,
        INTMC = 0x10,
        CC = 0x14,
        RESERVED = 0x18,
        CSTS = 0x1c,
        NSSR = 0x20,
        AQA = 0x24,
        ASQ = 0x28,
        ACQ = 0x30,
        CMBLOC = 0x38,
        CMBSZ = 0x3c,
        BPINFO = 0x40,
        BPRSEL = 0x44,
        BPMBL = 0x48,
    }
}

#[derive(Inspect)]
#[bitfield(u64)]
pub struct Cap {
    pub mqes_z: u16,
    pub cqr: bool,
    pub ams_weighted_round_robin_with_urgent: bool,
    pub ams_vendor_specific: bool,
    #[bits(5)]
    pub reserved: u8,
    pub to: u8,
    #[bits(4)]
    pub dstrd: u8,
    pub nssrs: bool,
    pub css_nvm: bool,
    #[bits(5)]
    pub css_reserved: u8,
    pub multiple_io: bool,
    pub admin_only: bool,
    pub bps: bool,
    #[bits(2)]
    pub cps: u8,
    #[bits(4)]
    pub mpsmin: u8,
    #[bits(4)]
    pub mpsmax: u8,
    pub pmrs: bool,
    pub cmbs: bool,
    pub nsss: bool,
    pub crwms: bool,
    pub crims: bool,
    #[bits(3)]
    pub reserved2: u64,
}

#[derive(Inspect)]
#[bitfield(u32)]
pub struct Cc {
    pub en: bool,
    #[bits(3)]
    pub reserved: u8,
    #[bits(3)]
    pub css: u8,
    #[bits(4)]
    pub mps: u8,
    #[bits(3)]
    pub ams: u8,
    #[bits(2)]
    pub shn: u8,
    #[bits(4)]
    pub iosqes: u8,
    #[bits(4)]
    pub iocqes: u8,
    pub crime: bool,
    #[bits(7)]
    pub reserved2: u8,
}

#[derive(Inspect)]
#[bitfield(u32)]
pub struct Csts {
    pub rdy: bool,
    pub cfs: bool,
    #[bits(2)]
    pub shst: u8,
    pub nssro: bool,
    pub pp: bool,
    pub st: bool,
    #[bits(25)]
    pub reserved: u32,
}

#[derive(Inspect)]
#[bitfield(u32)]
pub struct Aqa {
    #[bits(12)]
    pub asqs_z: u16,
    #[bits(4)]
    pub reserved: u8,
    #[bits(12)]
    pub acqs_z: u16,
    #[bits(4)]
    pub reserved2: u8,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    IntoBytes,
    Immutable,
    KnownLayout,
    FromBytes,
    Inspect,
    MeshPayload,
    PartialEq,
)]
pub struct Command {
    pub cdw0: Cdw0,
    pub nsid: u32,
    pub cdw2: u32,
    pub cdw3: u32,
    pub mptr: u64,
    #[inspect(iter_by_index)]
    pub dptr: [u64; 2],
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}
static_assertions::assert_eq_size!(Command, [u8; 64]);

#[derive(Inspect)]
#[bitfield(u32)]
#[derive(PartialEq, IntoBytes, Immutable, KnownLayout, FromBytes, MeshPayload)]
pub struct Cdw0 {
    pub opcode: u8,
    #[bits(2)]
    pub fuse: u8,
    #[bits(4)]
    pub reserved: u8,
    #[bits(2)]
    pub psdt: u8,
    pub cid: u16,
}

#[repr(C)]
pub struct Opcode(pub u8);

impl Opcode {
    pub fn transfer_controller_to_host(&self) -> bool {
        self.0 & 0b10 != 0
    }

    pub fn transfer_host_to_controller(&self) -> bool {
        self.0 & 0b01 != 0
    }
}

open_enum! {
    pub enum AdminOpcode: u8 {
        DELETE_IO_SUBMISSION_QUEUE = 0x00,
        CREATE_IO_SUBMISSION_QUEUE = 0x01,
        GET_LOG_PAGE = 0x02,
        DELETE_IO_COMPLETION_QUEUE = 0x04,
        CREATE_IO_COMPLETION_QUEUE = 0x05,
        IDENTIFY = 0x06,
        ABORT = 0x08,
        SET_FEATURES = 0x09,
        GET_FEATURES = 0x0a,
        ASYNCHRONOUS_EVENT_REQUEST = 0x0c,
        NAMESPACE_MANAGEMENT = 0x0d,
        FIRMWARE_COMMIT = 0x10,
        FIRMWARE_IMAGE_DOWNLOAD = 0x11,
        DEVICE_SELF_TEST = 0x14,
        NAMESPACE_ATTACHMENT = 0x15,
        KEEP_ALIVE = 0x18,
        DIRECTIVE_SEND = 0x19,
        DIRECTIVE_RECEIVE = 0x1a,
        VIRTUALIZATION_MANAGEMENT = 0x1c,
        NV_ME_MI_SEND = 0x1d,
        NV_ME_MI_RECEIVE = 0x1e,
        CAPACITY_MANAGEMENT = 0x20,
        LOCKDOWN = 0x24,
        DOORBELL_BUFFER_CONFIG = 0x7c,
        FABRICS_COMMANDS = 0x7f,
        FORMAT_NVM = 0x80,
        SECURITY_SEND = 0x81,
        SECURITY_RECEIVE = 0x82,
        SANITIZE = 0x84,
        GET_LBA_STATUS = 0x86,
    }
}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, MeshPayload)]
pub struct Completion {
    pub dw0: u32,
    pub dw1: u32,
    pub sqhd: u16,
    pub sqid: u16,
    pub cid: u16,
    pub status: CompletionStatus,
}
static_assertions::assert_eq_size!(Completion, [u8; 16]);

#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, MeshPayload)]
pub struct CompletionStatus {
    pub phase: bool,
    /// 8 bits of status code followed by 3 bits of the status code type.
    #[bits(11)]
    pub status: u16,
    #[bits(2)]
    pub crd: u8,
    pub more: bool,
    pub dnr: bool,
}

open_enum! {
    #[derive(Default)]
    pub enum StatusCodeType: u8 {
        GENERIC = 0,
        COMMAND_SPECIFIC = 1,
        MEDIA_ERROR = 2,
        PATH_RELATED = 3,
        VENDOR_SPECIFIC = 7,
    }
}

open_enum! {
    #[derive(Default)]
    pub enum Status: u16 {
        SUCCESS = 0x00,
        INVALID_COMMAND_OPCODE = 0x01,
        INVALID_FIELD_IN_COMMAND = 0x02,
        COMMAND_ID_CONFLICT = 0x03,
        DATA_TRANSFER_ERROR = 0x04,
        COMMANDS_ABORTED_DUE_TO_POWER_LOSS_NOTIFICATION = 0x05,
        INTERNAL_ERROR = 0x06,
        COMMAND_ABORT_REQUESTED = 0x07,
        COMMAND_ABORTED_DUE_TO_SQ_DELETION = 0x08,
        COMMAND_ABORTED_DUE_TO_FAILED_FUSED_COMMAND = 0x09,
        COMMAND_ABORTED_DUE_TO_MISSING_FUSED_COMMAND = 0x0a,
        INVALID_NAMESPACE_OR_FORMAT = 0x0b,
        COMMAND_SEQUENCE_ERROR = 0x0c,
        INVALID_SGL_SEGMENT_DESCRIPTOR = 0x0d,
        INVALID_NUMBER_OF_SGL_DESCRIPTORS = 0x0e,
        DATA_SGL_LENGTH_INVALID = 0x0f,
        METADATA_SGL_LENGTH_INVALID = 0x10,
        SGL_DESCRIPTOR_TYPE_INVALID = 0x11,
        INVALID_USE_OF_CONTROLLER_MEMORY_BUFFER = 0x12,
        PRP_OFFSET_INVALID = 0x13,
        ATOMIC_WRITE_UNIT_EXCEEDED = 0x14,
        OPERATION_DENIED = 0x15,
        SGL_OFFSET_INVALID = 0x16,
        RESERVED = 0x17,
        HOST_IDENTIFIER_INCONSISTENT_FORMAT = 0x18,
        KEEP_ALIVE_TIMER_EXPIRED = 0x19,
        KEEP_ALIVE_TIMEOUT_INVALID = 0x1a,
        COMMAND_ABORTED_DUE_TO_PREEMPT_AND_ABORT = 0x1b,
        SANITIZE_FAILED = 0x1c,
        SANITIZE_IN_PROGRESS = 0x1d,
        SGL_DATA_BLOCK_GRANULARITY_INVALID = 0x1e,
        COMMAND_NOT_SUPPORTED_FOR_QUEUE_IN_CMB = 0x1f,
        NAMESPACE_IS_WRITE_PROTECTED = 0x20,
        COMMAND_INTERRUPTED = 0x21,
        TRANSIENT_TRANSPORT_ERROR = 0x22,
        COMMAND_PROHIBITED_BY_COMMAND_AND_FEATURE_LOCKDOWN = 0x23,
        ADMIN_COMMAND_MEDIA_NOT_READY = 0x24,

        LBA_OUT_OF_RANGE = 0x80,
        CAPACITY_EXCEEDED = 0x81,
        NAMESPACE_NOT_READY = 0x82,
        RESERVATION_CONFLICT = 0x83,
        FORMAT_IN_PROGRESS = 0x84,

        COMPLETION_QUEUE_INVALID = 0x100,
        INVALID_QUEUE_IDENTIFIER = 0x101,
        INVALID_QUEUE_SIZE = 0x102,
        ABORT_COMMAND_LIMIT_EXCEEDED = 0x103,
        RESERVED2 = 0x104,
        ASYNCHRONOUS_EVENT_REQUEST_LIMIT_EXCEEDED = 0x105,
        INVALID_FIRMWARE_SLOT = 0x106,
        INVALID_FIRMWARE_IMAGE = 0x107,
        INVALID_INTERRUPT_VECTOR = 0x108,
        INVALID_LOG_PAGE = 0x109,
        INVALID_FORMAT = 0x10a,
        FIRMWARE_ACTIVATION_REQUIRES_CONVENTIONAL_RESET = 0x10b,
        INVALID_QUEUE_DELETION = 0x10c,
        FEATURE_IDENTIFIER_NOT_SAVEABLE = 0x10d,
        FEATURE_NOT_CHANGEABLE = 0x10e,
        FEATURE_NOT_NAMESPACE_SPECIFIC = 0x10f,
        FIRMWARE_ACTIVATION_REQUIRES_NVM_SUBSYSTEM_RESET = 0x110,
        FIRMWARE_ACTIVATION_REQUIRES_CONTROLLER_LEVEL_RESET = 0x111,
        FIRMWARE_ACTIVATION_REQUIRES_MAXIMUM_TIME_VIOLATION = 0x112,
        FIRMWARE_ACTIVATION_PROHIBITED = 0x113,
        OVERLAPPING_RANGE = 0x114,
        NAMESPACE_INSUFFICIENT_CAPACITY = 0x115,
        NAMESPACE_IDENTIFIER_UNAVAILABLE = 0x116,
        RESERVED3 = 0x117,
        NAMESPACE_ALREADY_ATTACHED = 0x118,
        NAMESPACE_IS_PRIVATE = 0x119,
        NAMESPACE_NOT_ATTACHED = 0x11a,
        THIN_PROVISIONING_NOT_SUPPORTED = 0x11b,
        CONTROLLER_LIST_INVALID = 0x11c,
        DEVICE_SELF_TEST_IN_PROGRESS = 0x11d,
        BOOT_PARTITION_WRITE_PROHIBITED = 0x11e,
        INVALID_CONTROLLER_IDENTIFIER = 0x11f,
        INVALID_SECONDARY_CONTROLLER_STATE = 0x120,
        INVALID_NUMBER_OF_CONTROLLER_RESOURCES = 0x121,
        INVALID_RESOURCE_IDENTIFIER = 0x122,
        SANITIZE_PROHIBITED_WHILE_PERSISTENT_MEMORY_REGION_IS_ENABLED = 0x123,
        ANA_GROUP_IDENTIFIER_INVALID = 0x124,
        ANA_ATTACH_FAILED = 0x125,
        INSUFFICIENT_CAPACITY = 0x126,
        NAMESPACE_ATTACHMENT_LIMIT_EXCEEDED = 0x127,
        PROHIBITION_OF_COMMAND_EXECUTION_NOT_SUPPORTED = 0x128,
        IO_COMMAND_SET_NOT_SUPPORTED = 0x129,
        IO_COMMAND_SET_NOT_ENABLED = 0x12a,
        IO_COMMAND_SET_COMBINATION_REJECTED = 0x12b,
        INVALID_IO_COMMAND_SET = 0x12c,
        IDENTIFIER_UNAVAILABLE = 0x12d,

        CONFLICTING_ATTRIBUTES = 0x180,         // Dataset Management, Read, Write
        INVALID_PROTECTION_INFORMATION = 0x181,         // Compare, Read, Write, Write Zeroes
        ATTEMPTED_WRITE_TO_READ_ONLY_RANGE = 0x182,         // Dataset Management, Write, Write Uncorrectable, Write Zeroes
        COMMAND_SIZE_LIMIT_EXCEEDED = 0x183,         // Dataset Management

        MEDIA_WRITE_FAULT                             = 0x280,
        MEDIA_UNRECOVERED_READ_ERROR                  = 0x281,
        MEDIA_END_TO_END_GUARD_CHECK_ERROR            = 0x282,
        MEDIA_END_TO_END_APPLICATION_TAG_CHECK_ERROR  = 0x283,
        MEDIA_END_TO_END_REFERENCE_TAG_CHECK_ERROR    = 0x284,
        MEDIA_COMPARE_FAILURE                         = 0x285,
        MEDIA_ACCESS_DENIED                           = 0x286,
        MEDIA_DEALLOCATED_OR_UNWRITTEN_LOGICAL_BLOCK  = 0x287,
    }
}

impl Status {
    pub fn status_code(&self) -> u8 {
        self.0 as u8
    }

    pub fn status_code_type(&self) -> StatusCodeType {
        StatusCodeType((self.0 >> 8) as u8)
    }
}

// Identify
#[bitfield(u32)]
pub struct Cdw10Identify {
    pub cns: u8,
    pub reserved: u8,
    pub cntid: u16,
}

open_enum! {
    pub enum Cns: u8 {
        NAMESPACE = 0x0,
        CONTROLLER = 0x1,
        ACTIVE_NAMESPACES = 0x2,
        DESCRIPTOR_NAMESPACE = 0x3,
        NVM_SET = 0x4,
        SPECIFIC_NAMESPACE_IO_COMMAND_SET = 0x5,
        SPECIFIC_CONTROLLER_IO_COMMAND_SET = 0x6,
        ACTIVE_NAMESPACE_LIST_IO_COMMAND_SET = 0x7,
        ALLOCATED_NAMESPACE_LIST = 0x10,
        ALLOCATED_NAMESPACE = 0x11,
        CONTROLLER_LIST_OF_NSID = 0x12,
        CONTROLLER_LIST_OF_NVM_SUBSYSTEM = 0x13,
        PRIMARY_CONTROLLER_CAPABILITIES = 0x14,
        SECONDARY_CONTROLLER_LIST = 0x15,
        NAMESPACE_GRANULARITY_LIST = 0x16,
        UUID_LIST = 0x17,
        DOMAIN_LIST = 0x18,
        ENDURANCE_GROUP_LIST = 0x19,
        ALLOCATED_NAMESPACE_LIST_IO_COMMAND_SET = 0x1a,
        ALLOCATED_NAMESPACE_IO_COMMAND_SET = 0x1b,
        IO_COMMAND_SET = 0x1c,
    }
}

#[derive(Inspect)]
#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct OptionalAdminCommandSupport {
    pub security_send_security_receive: bool,
    pub format_nvm: bool,
    pub firmware_activate_firmware_download: bool,
    pub ns_management: bool,
    pub self_test: bool,
    pub directives: bool,
    pub nvme_mi_send_nvme_mi_receive: bool,
    pub virtualization_management: bool,
    pub doorbell_buffer_config: bool,
    pub get_lba_status: bool,
    pub command_feature_lockdown: bool,
    #[bits(5)]
    pub rsvd: u16,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect, Clone)]
pub struct IdentifyController {
    pub vid: u16,
    pub ssvid: u16,
    pub sn: AsciiString<20>,
    pub mn: AsciiString<40>,
    pub fr: AsciiString<8>,
    pub rab: u8,
    pub ieee: [u8; 3],
    pub cmic: u8,
    /// Maximum data transfer size (in minimum page size units, as power of
    /// two).
    pub mdts: u8,
    pub cntlid: u16,
    pub ver: u32,
    pub rtd3r: u32,
    pub rtd3e: u32,
    pub oaes: Oaes,
    pub ctratt: u32,
    pub rrls: u16,
    #[inspect(skip)]
    pub rsvd1: [u8; 9],
    pub cntrltype: ControllerType,
    pub fguid: [u8; 16],
    pub crdt1: u16,
    pub crdt2: u16,
    pub crdt3: u16,
    #[inspect(skip)]
    pub rsvd2: [u8; 106],
    #[inspect(skip)]
    pub rsvd3: [u8; 13],
    pub nvmsr: u8,
    pub vwci: u8,
    pub mec: u8,
    pub oacs: OptionalAdminCommandSupport,
    pub acl: u8,
    pub aerl: u8,
    pub frmw: FirmwareUpdates,
    pub lpa: u8,
    pub elpe: u8,
    pub npss: u8,
    pub avscc: u8,
    pub apsta: u8,
    pub wctemp: u16,
    pub cctemp: u16,
    pub mtfa: u16,
    pub hmpre: u32,
    pub hmmin: u32,
    #[inspect(display)]
    pub tnvmcap: U128LE,
    #[inspect(display)]
    pub unvmcap: U128LE,
    pub rpmbs: u32,
    pub edstt: u16,
    pub dsto: u8,
    pub fwug: u8,
    pub kas: u16,
    pub hctma: u16,
    pub mntmt: u16,
    pub mxtmt: u16,
    pub sanicap: u32,
    pub hmminds: u32,
    pub hmmaxd: u16,
    pub nsetidmax: u16,
    pub endgidmax: u16,
    pub anatt: u8,
    pub anacap: u8,
    pub anagrpmax: u32,
    pub nanagrpid: u32,
    pub pels: u32,
    pub domain_id: u16,
    #[inspect(skip)]
    pub rsvd4: [u8; 10],
    #[inspect(display)]
    pub megcap: U128LE,
    #[inspect(skip)]
    pub rsvd5: [u8; 128],
    pub sqes: QueueEntrySize,
    pub cqes: QueueEntrySize,
    pub maxcmd: u16,
    pub nn: u32,
    pub oncs: Oncs,
    pub fuses: u16,
    pub fna: u8,
    pub vwc: VolatileWriteCache,
    pub awun: u16,
    pub awupf: u16,
    pub icsvscc: u8,
    pub nwpc: u8,
    pub acwu: u16,
    pub copy_descriptor_fmt: u16,
    pub sgls: u32,
    pub mnan: u32,
    #[inspect(display)]
    pub maxdna: U128LE,
    pub maxcna: u32,
    #[inspect(skip)]
    pub rsvd6: [u8; 204],
    #[inspect(with = "|x| core::str::from_utf8(x).map(|s| s.trim_end_matches('\0')).ok()")]
    pub subnqn: [u8; 256],
    #[inspect(skip)]
    pub rsvd7: [u8; 768],
    pub ioccsz: u32,
    pub iorcsz: u32,
    pub icdoff: u16,
    pub fcatt: u8,
    pub msdbd: u8,
    pub ofcs: u16,
    #[inspect(skip)]
    pub rsvd8: [u8; 242],
    #[inspect(skip)]
    pub power: [u8; 1024],
    #[inspect(skip)]
    pub vendor: [u8; 1024],
}

const _: () = assert!(size_of::<IdentifyController>() == 4096);

#[derive(Inspect)]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueueEntrySize {
    #[bits(4)]
    pub min: u8,
    #[bits(4)]
    pub max: u8,
}

#[derive(Inspect)]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FirmwareUpdates {
    pub ffsro: bool,
    #[bits(3)]
    pub nofs: u8,
    pub fawr: bool,
    pub smud: bool,
    #[bits(2)]
    pub rsvd: u8,
}

/// Optional asynchronous events supported
#[derive(Inspect)]
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Oaes {
    _rsvd: u8,
    pub namespace_attribute: bool,
    pub firmware_activation: bool,
    _rsvd2: bool,
    pub asymmetric_namespace_access_change: bool,
    pub predictable_latency_event_aggregate_log_change: bool,
    pub lba_status_information: bool,
    pub endurance_group_event_aggregate_log_page_change: bool,
    pub normal_nvm_subsystem_shutdown: bool,
    _rsvd3: u16,
}

/// Optional NVM command support
#[derive(Inspect)]
#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Oncs {
    pub compare: bool,
    pub write_uncorrectable: bool,
    pub dataset_management: bool,
    pub write_zeroes: bool,
    pub save: bool,
    pub reservations: bool,
    pub timestamp: bool,
    pub verify: bool,
    pub copy: bool,
    #[bits(7)]
    _rsvd: u16,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
    #[inspect(debug)]
    pub enum ControllerType: u8 {
        RESERVED = 0,
        IO_CONTROLLER = 1,
        DISCOVERY_CONTROLLER = 2,
        ADMINISTRATIVE_CONTROLLER = 3,
    }
}

#[derive(Inspect)]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VolatileWriteCache {
    pub present: bool,
    #[bits(2)]
    pub broadcast_flush_behavior: u8,
    #[bits(5)]
    _rsvd: u8,
}

open_enum! {
    pub enum BroadcastFlushBehavior: u8 {
        NOT_INDICATED = 0,
        NOT_SUPPORTED = 2,
        SUPPORTED = 3,
    }
}

#[bitfield(u32)]
pub struct Cdw10SetFeatures {
    pub fid: u8,
    #[bits(23)]
    _rsvd: u32,
    pub save: bool,
}

#[bitfield(u32)]
pub struct Cdw10GetFeatures {
    pub fid: u8,
    #[bits(3)]
    pub sel: u8,
    #[bits(21)]
    _rsvd: u32,
}

open_enum! {
    pub enum Feature: u8 {
        ARBITRATION = 0x01,
        POWER_MANAGEMENT = 0x02,
        LBA_RANGE_TYPE = 0x03,
        TEMPERATURE_THRESHOLD = 0x04,
        ERROR_RECOVERY = 0x05,
        VOLATILE_WRITE_CACHE = 0x06,
        NUMBER_OF_QUEUES = 0x07,
        INTERRUPT_COALESCING = 0x08,
        INTERRUPT_VECTOR_CONFIG = 0x09,
        WRITE_ATOMICITY = 0x0a,
        ASYNC_EVENT_CONFIG = 0x0b,
        AUTONOMOUS_POWER_STATE_TRANSITION = 0x0c,
        HOST_MEMORY_BUFFER = 0x0d,
        TIMESTAMP = 0x0e,
        KEEP_ALIVE = 0x0f,
        HOST_CONTROLLED_THERMAL_MANAGEMENT = 0x10,
        NONOPERATIONAL_POWER_STATE = 0x11,
        READ_RECOVERY_LEVEL_CONFIG = 0x12,
        PREDICTABLE_LATENCY_MODE_CONFIG = 0x13,
        PREDICTABLE_LATENCY_MODE_WINDOW = 0x14,
        LBA_STATUS_INFORMATION_REPORT_INTERVAL = 0x15,
        HOST_BEHAVIOR_SUPPORT = 0x16,
        SANITIZE_CONFIG = 0x17,
        ENDURANCE_GROUP_EVENT_CONFIG = 0x18,
        IO_COMMAND_SET_PROFILE = 0x19,
        ENHANCED_CONTROLLER_METADATA = 0x7d,
        CONTROLLER_METADATA = 0x7e,
        NAMESPACE_METADATA = 0x7f,
        NVM_SOFTWARE_PROGRESS_MARKER = 0x80,
        NVM_HOST_IDENTIFIER = 0x81,
        NVM_RESERVATION_NOTIFICATION_MASK = 0x82,
        NVM_RESERVATION_PERSISTENCE = 0x83,
        NVM_NAMESPACE_WRITE_PROTECTION_CONFIG = 0x84,
    }
}

#[bitfield(u32)]
pub struct Cdw11FeatureNumberOfQueues {
    pub nsq_z: u16,
    pub ncq_z: u16,
}

#[bitfield(u32)]
pub struct Cdw11FeatureVolatileWriteCache {
    pub wce: bool,
    #[bits(31)]
    _rsvd: u32,
}

#[bitfield(u32)]
pub struct Cdw11FeatureReservationPersistence {
    /// Persist through power loss
    pub ptpl: bool,
    #[bits(31)]
    _rsvd: u32,
}

#[bitfield(u32)]
pub struct Cdw10CreateIoQueue {
    pub qid: u16,
    pub qsize_z: u16,
}

#[bitfield(u32)]
pub struct Cdw11CreateIoCompletionQueue {
    pub pc: bool,
    pub ien: bool,
    #[bits(14)]
    pub rsvd: u16,
    pub iv: u16,
}

#[bitfield(u32)]
pub struct Cdw11CreateIoSubmissionQueue {
    pub pc: bool,
    #[bits(2)]
    pub qprio: u8,
    #[bits(13)]
    pub rsvd: u16,
    pub cqid: u16,
}

#[bitfield(u32)]
pub struct Cdw10DeleteIoQueue {
    pub qid: u16,
    pub rsvd: u16,
}

#[bitfield(u32)]
pub struct Cdw10GetLogPage {
    /// Log page identifier
    pub lid: u8,
    #[bits(7)]
    pub lsp: u8,
    /// Retain asynchronous event
    pub rae: bool,
    pub numdl_z: u16,
}

#[bitfield(u32)]
pub struct Cdw11GetLogPage {
    pub numdu: u16,
    pub lsi: u16,
}

open_enum! {
    pub enum LogPageIdentifier: u8 {
        SUPPORTED_LOG_PAGES = 0,
        ERROR_INFORMATION = 1,
        HEALTH_INFORMATION = 2,
        FIRMWARE_SLOT_INFORMATION = 3,
        CHANGED_NAMESPACE_LIST = 4,
    }
}

#[bitfield(u32)]
pub struct AsynchronousEventRequestDw0 {
    #[bits(3)]
    pub event_type: u8,
    #[bits(5)]
    _rsvd: u8,
    pub information: u8,
    pub log_page_identifier: u8,
    _rsvd2: u8,
}

open_enum! {
    pub enum AsynchronousEventType: u8 {
        ERROR_STATUS = 0b000,
        HEALTH_STATUS = 0b001,
        NOTICE = 0b010,
        IMMEDIATE = 0b011,
        IO_COMMAND_SPECIFIC = 0b110,
        VENDOR_SPECIFIC = 0b111,
    }
}

open_enum! {
    pub enum AsynchronousEventInformationNotice: u8 {
        NAMESPACE_ATTRIBUTE_CHANGED = 0,
        FIRMWARE_ACTIVATION_STARTING = 1,
        TELEMETRY_LOG_CHANGED = 2,
        ASYMMETRIC_NAMESPACE_ACCESS_CHANGE = 3,
        PREDICTABLE_LATENCY_EVENT_AGGREGATE_LOG_CHANGE = 4,
        LBA_STATUS_INFORMATION_ALERT = 5,
        ENDURANCE_GROUP_EVENT_AGGREGATE_LOG_PAGE_CHANGE = 6,
    }
}
