// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protocol definitions for the VSS (Volume Shadow Service) IC.

#![allow(missing_docs)]

use crate::Version;
use guid::Guid;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub const VSS_VERSION_WIN8: Version = Version::new(4, 0);
pub const VSS_VERSION_WINBLUE: Version = Version::new(5, 0);
pub const VSS_VERSION_THRESHOLD: Version = Version::new(6, 0);
pub const VSS_VERSION_THRESHOLD_UR1: Version = Version::new(7, 0);

pub const MAX_VHD_COUNT: usize = 260;

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VssMessage {
    pub header: VssHeader,
    pub reserved: [u8; 7],
    pub data: [u8; 24],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VssHeader {
    pub operation: Operation,
    pub reserved: [u8; 7],
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum Operation: u8 {
        CREATE = 0,
        DELETE = 1,
        CHECK_HOT_BACKUP = 2,
        GET_DIRECT_MAPPED_DEVICES_INFO = 3,
        // Messages below this are only valid for message version >= 4.0
        BACKUP_COMPLETE = 4,
        // Messages below this are only valid for message version >= 5.0
        FREEZE_APPLICATIONS = 5,
        THAW_APPLICATIONS = 6,
        AUTO_RECOVER = 7,
        // Messages below this are only valid for message version >= 6.0
        QUERY_GUEST_CLUSTER_INFORMATION = 8,
    }
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageCheckHotBackup {
    pub flags: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageCreate {
    pub snapshot_set_id: Guid,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageCreateV2 {
    pub snapshot_set_id: Guid,
    pub backup_type: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageDelete {
    pub snapshot_set_id: Guid,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageDirectMappedDevicesInfo {
    pub flags: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageCheckHotBackupComplete {
    pub flags: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageThawApplications {
    pub flags: u32,
}

/// For freeze and autorecover.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message2 {
    pub header: VssHeader,
    pub backup_type: u32,
    pub flags: u32,
    pub lun_count: u32,
    pub luns: [LunInfo; MAX_VHD_COUNT],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message2Ex {
    pub header: VssHeader,
    pub backup_type: u32,
    pub flags: u32,
    pub lun_count: u32,
    pub luns: [LunInfo; MAX_VHD_COUNT],
    pub shadow_luns: [LunInfo; MAX_VHD_COUNT],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message3 {
    pub header: VssHeader,
    pub cluster_id: Guid,
    pub cluster_size: u32,
    pub lun_count: u32,
    pub shared_luns: [LunInfo; MAX_VHD_COUNT],
    pub shared_lun_status: [u32; MAX_VHD_COUNT],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message3Ex {
    pub header: VssHeader,
    pub cluster_id: Guid,
    pub cluster_size: u32,
    pub lun_count: u32,
    pub shared_luns: [LunInfo; MAX_VHD_COUNT],
    pub shared_lun_status: [u32; MAX_VHD_COUNT],
    pub last_move_time: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct LunInfo {
    pub bus_type: u8,
    pub reserved: [u8; 3],
    pub controller: Guid,
    pub port: u8,
    pub target: u8,
    pub lun: u8,
    pub reserved2: u8,
}
