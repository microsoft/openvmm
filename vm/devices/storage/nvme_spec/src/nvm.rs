// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVM command set definitions
//!
//! NVM Command Set 1.0c: <https://nvmexpress.org/wp-content/uploads/NVM-Express-NVM-Command-Set-Specification-1.0c-2022.10.03-Ratified.pdf>

use crate::U128LE;
use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::LE;
use zerocopy::U16;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect, Clone)]
pub struct IdentifyNamespace {
    pub nsze: u64,
    pub ncap: u64,
    pub nuse: u64,
    pub nsfeat: Nsfeat,
    /// Number of LBA formats. Zero based.
    pub nlbaf: u8,
    pub flbas: Flbas,
    pub mc: u8,
    pub dpc: u8,
    pub dps: u8,
    pub nmic: u8,
    pub rescap: ReservationCapabilities,
    pub fpi: u8,
    pub dlfeat: u8,
    pub nawun: u16,
    pub nawupf: u16,
    pub nacwu: u16,
    pub nabsn: u16,
    pub nabo: u16,
    pub nabspf: u16,
    pub noiob: u16,
    #[inspect(display)]
    pub nvmcap: U128LE,
    pub npwg: u16,
    pub npwa: u16,
    pub npdg: u16,
    pub npda: u16,
    pub nows: u16,
    pub mssrl: u16,
    pub mcl: u32,
    pub msrc: u8,
    #[inspect(skip)]
    pub rsvd1: [u8; 11],
    pub anagrpid: u32,
    #[inspect(skip)]
    pub rsvd2: [u8; 3],
    pub nsattr: u8,
    pub nvmsetid: u16,
    pub endgid: u16,
    pub nguid: [u8; 16],
    pub eui64: [u8; 8],
    #[inspect(iter_by_index)]
    pub lbaf: [Lbaf; 16],
    #[inspect(skip)]
    pub rsvd3: [u8; 192],
    #[inspect(skip)]
    pub vs: [u8; 3712],
}

open_enum! {
    pub enum NamespaceIdentifierType: u8 {
        RESERVED = 0x00,
        IEEE = 0x01,
        NSGUID = 0x02,
        NSUUID = 0x03,
        CSI = 0x04,
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
pub struct NamespaceIdentificationDescriptor {
    pub nidt: u8, // NamespaceIdentifierType
    pub nidl: u8,
    pub rsvd: [u8; 2],
    pub nid: [u8; 16],
}

#[derive(Inspect)]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Nsfeat {
    /// Thin provisioning
    pub thinp: bool,
    /// NAWUN, NAWUPF, NACWU are defined.
    pub nsabp: bool,
    /// Namespace supports deallocated or unwritten logical block error.
    pub dae: bool,
    pub uidreuse: bool,
    /// NPWG, NPWA, NPDG, NPDA, and NOWS are defined for this namespace.
    pub optperf: bool,
    #[bits(3)]
    _rsvd: u8,
}

/// LBA format
#[derive(Inspect)]
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Lbaf {
    /// Metadata size
    pub ms: u16,
    /// LBA data size (as power of two)
    pub lbads: u8,
    /// Relative performance
    #[bits(2)]
    pub rp: u8,
    #[bits(6)]
    _rsvd: u8,
}

/// Formatted LBA size
#[derive(Inspect)]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Flbas {
    #[bits(4)]
    pub low_index: u8,
    pub inband_metadata: bool,
    /// High bits of the index. Only valid if NLBAF > 16.
    #[bits(2)]
    pub high_index: u8,
    #[bits(1)]
    _rsvd: u8,
}

#[derive(Inspect)]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReservationCapabilities {
    pub persist_through_power_loss: bool,
    pub write_exclusive: bool,
    pub exclusive_access: bool,
    pub write_exclusive_registrants_only: bool,
    pub exclusive_access_registrants_only: bool,
    pub write_exclusive_all_registrants: bool,
    pub exclusive_access_all_registrants: bool,
    _rsvd: bool,
}

open_enum! {
    pub enum NvmOpcode: u8 {
        FLUSH = 0x00,
        WRITE = 0x01,
        READ = 0x02,
        /// Dataset management.
        DSM = 0x09,

        RESERVATION_REGISTER = 0xd,
        RESERVATION_REPORT = 0xe,
        RESERVATION_ACQUIRE = 0x11,
        RESERVATION_RELEASE = 0x15,
    }
}

#[bitfield(u32)]
pub struct Cdw10ReadWrite {
    /// Starting LBA, low 32 bits.
    pub sbla_low: u32,
}

#[bitfield(u32)]
pub struct Cdw11ReadWrite {
    /// Starting LBA, high 32 bits.
    pub sbla_high: u32,
}

#[bitfield(u32)]
pub struct Cdw12ReadWrite {
    /// Number of logical blocks. Zero-based.
    pub nlb_z: u16,
    #[bits(4)]
    _rsvd: u8,
    /// Directive type (write only).
    #[bits(4)]
    pub dtype: u8,
    /// Storage tag check.
    pub stc: bool,
    _rsvd2: bool,
    /// Protection information
    #[bits(4)]
    pub prinfo: u8,
    /// Force unit access
    pub fua: bool,
    /// Limited retry
    pub lr: bool,
}

#[bitfield(u32)]
pub struct Cdw10Dsm {
    /// Number of ranges. Zero-based.
    pub nr_z: u8,
    #[bits(24)]
    _rsvd: u32,
}

#[bitfield(u32)]
pub struct Cdw11Dsm {
    /// Integral dataset for read.
    pub idr: bool,
    /// Integral dataset for write.
    pub idw: bool,
    /// Deallocate.
    pub ad: bool,
    #[bits(29)]
    _rsvd: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DsmRange {
    pub context_attributes: u32,
    pub lba_count: u32,
    pub starting_lba: u64,
}

#[bitfield(u32)]
pub struct Cdw10ReservationRegister {
    /// Reservation register action
    #[bits(3)]
    pub rrega: u8,
    /// Ignore existing key
    pub iekey: bool,
    #[bits(26)]
    _rsvd: u32,
    /// Change "persist through power loss" state
    #[bits(2)]
    pub cptpl: u8,
}

open_enum! {
    pub enum ReservationRegisterAction: u8 {
        REGISTER = 0,
        UNREGISTER = 1,
        REPLACE = 2,
    }
}

open_enum! {
    pub enum ChangePersistThroughPowerLoss: u8 {
        NO_CHANGE = 0,
        CLEAR = 2,
        SET = 3,
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReservationRegister {
    /// Current reservation key
    pub crkey: u64,
    /// New reservation key
    pub nrkey: u64,
}

#[bitfield(u32)]
pub struct Cdw10ReservationAcquire {
    /// Reservation acquire action
    #[bits(3)]
    pub racqa: u8,
    /// Ignore existing key (obsolete)
    pub iekey: bool,
    #[bits(4)]
    _rsvd: u32,
    pub rtype: u8,
    _rsvd2: u16,
}

open_enum! {
    pub enum ReservationAcquireAction: u8 {
        ACQUIRE = 0,
        PREEMPT = 1,
        PREEMPT_AND_ABORT = 2,
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum ReservationType: u8 {
        WRITE_EXCLUSIVE = 1,
        EXCLUSIVE_ACCESS = 2,
        WRITE_EXCLUSIVE_REGISTRANTS_ONLY = 3,
        EXCLUSIVE_ACCESS_REGISTRANTS_ONLY = 4,
        WRITE_EXCLUSIVE_ALL_REGISTRANTS = 5,
        EXCLUSIVE_ACCESS_ALL_REGISTRANTS = 6,
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReservationAcquire {
    /// Current reservation key
    pub crkey: u64,
    /// Preempt reservation key
    pub prkey: u64,
}

#[bitfield(u32)]
pub struct Cdw10ReservationRelease {
    /// Reservation release action
    #[bits(3)]
    pub rrela: u8,
    /// Ignore existing key (obsolete)
    pub iekey: bool,
    #[bits(4)]
    _rsvd: u32,
    pub rtype: u8,
    _rsvd2: u16,
}

open_enum! {
    pub enum ReservationReleaseAction: u8 {
        RELEASE = 0,
        CLEAR = 1,
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReservationRelease {
    /// Current reservation key
    pub crkey: u64,
}

#[bitfield(u32)]
pub struct Cdw10ReservationReport {
    pub numd_z: u32,
}

#[bitfield(u32)]
pub struct Cdw11ReservationReport {
    /// Extended data structure
    pub eds: bool,
    #[bits(31)]
    _rsvd: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReservationReport {
    /// Generation
    pub generation: u32,
    /// Reservation type
    pub rtype: ReservationType,
    /// Number of registered controllers
    pub regctl: U16<LE>,
    pub reserved: [u8; 2],
    /// Persist through power loss state
    pub ptpls: u8,
    pub reserved2: [u8; 14],
    // Followed by `[RegisteredController; _]`.
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReservationReportExtended {
    pub report: ReservationReport,
    pub reserved: [u8; 40],
    // Followed by `[RegisteredControllerExtended; _]`.
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct RegisteredController {
    /// Controller ID
    pub cntlid: u16,
    /// Reservation status
    pub rcsts: ReservationStatus,
    pub reserved: [u8; 5],
    /// Host ID
    pub hostid: [u8; 8],
    /// Reservation key
    pub rkey: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct RegisteredControllerExtended {
    /// Controller ID
    pub cntlid: u16,
    /// Reservation status
    pub rcsts: ReservationStatus,
    pub reserved: [u8; 5],
    /// Reservation key
    pub rkey: u64,
    /// Host ID
    pub hostid: [u8; 16],
    pub reserved2: [u8; 32],
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReservationStatus {
    pub holds_reservation: bool,
    #[bits(7)]
    _rsvd: u8,
}
