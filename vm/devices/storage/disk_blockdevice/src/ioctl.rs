// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux IOCTLs for block devices.

use crate::ReservationType;
use nix::ioctl_none_bad;
use nix::ioctl_read;
use nix::ioctl_read_bad;
use nix::ioctl_write_int_bad;
use nix::ioctl_write_ptr;
use nix::ioctl_write_ptr_bad;
use nix::request_code_none;
use open_enum::open_enum;
use std::fs;
use std::os::unix::prelude::*;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Linux block device IOCTLs.
const BLK_IOC_MAGIC: u8 = 0x12;
// #define BLKSSZGET  _IO(0x12,104)
ioctl_read_bad!(
    blk_get_sector_size_ioctl,
    request_code_none!(BLK_IOC_MAGIC, 104),
    u32
);
// #define BLKGETSIZE64 _IOR(0x12,114,size_t)
ioctl_read!(blk_get_device_size_in_bytes_ioctl, BLK_IOC_MAGIC, 114, u64);
// #define BLKDISCARD _IO(0x12,119)
ioctl_write_ptr_bad!(
    blk_discard_ioctl,
    request_code_none!(BLK_IOC_MAGIC, 119),
    [u64; 2]
);
// #define BLKPBSZGET _IO(0x12,123)
ioctl_read_bad!(
    blk_get_physical_sector_size_ioctl,
    request_code_none!(BLK_IOC_MAGIC, 123),
    u32
);

ioctl_none_bad!(blk_eject_ioctl, 0x5309);

ioctl_write_int_bad!(blk_lockdoor_ioctl, 0x5329);

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    enum BlkStatus: u8 {
        BLK_STS_OK = 0,
        BLK_STS_NOTSUPP = 1,
        BLK_STS_TIMEOUT = 2,
        BLK_STS_NOSPC = 3,
        BLK_STS_TRANSPORT = 4,
        BLK_STS_TARGET = 5,
        BLK_STS_NEXUS = 6, // nvme/scsi reservation conflicts and general nexue failures
        BLK_STS_MEDIUM = 7,
        BLK_STS_PROTECTION = 8,
        BLK_STS_RESOURCE = 9,
        BLK_STS_IOERR = 10,
        BLK_STS_AGAIN = 12,
        BLK_STS_DEV_RESOURCE = 13,
        BLK_STS_ZONE_RESOURCE = 14,
        BLK_STS_ZONE_OPEN_RESOURCE = 15,
        BLK_STS_ZONE_ACTIVE_RESOURCE = 16,
        BLK_STS_OFFLINE = 17,
        BLK_STS_RSV_CONFLICT = 18, // reservation conflict for pr_ops
    }
}

pub fn discard(file: &fs::File, start: u64, len: u64) -> std::io::Result<()> {
    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is legal to call on any valid FD.
    //         More documentation on this specific ioctl can be found in fs.h.
    unsafe { blk_discard_ioctl(file.as_raw_fd(), &[start, len])? };
    Ok(())
}

/// Eject the underlying block device.
pub fn eject(file: &fs::File) -> std::io::Result<()> {
    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is legal to call on any valid FD representing a CDROM.
    //         More documentation on this specific ioctl can be found in cdrom.h.
    unsafe { blk_eject_ioctl(file.as_raw_fd())? };
    Ok(())
}

/// Sets the SCSI DVD Prevent bit, enabling or disabling change media requests.
pub fn lockdoor(file: &fs::File, locked: bool) -> std::io::Result<()> {
    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is legal to call on any valid FD representing a CDROM.
    //         More documentation on this specific ioctl can be found in cdrom.h.
    unsafe { blk_lockdoor_ioctl(file.as_raw_fd(), locked as libc::c_int)? };
    Ok(())
}

/// Queries total size of a block device.
pub fn query_block_device_size_in_bytes(file: &fs::File) -> std::io::Result<u64> {
    let mut size_in_bytes = 0u64;

    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is legal to call on any valid FD.
    //         More documentation on this specific ioctl can be found in fs.h.
    unsafe { blk_get_device_size_in_bytes_ioctl(file.as_raw_fd(), &mut size_in_bytes)? };

    tracing::debug!(size_in_bytes, "query_block_device_size_in_bytes");

    Ok(size_in_bytes)
}

pub const PR_FL_IGNORE_KEY: u32 = 1 << 0;

pub const PR_WRITE_EXCLUSIVE: u32 = 1;
pub const PR_EXCLUSIVE_ACCESS: u32 = 2;
pub const PR_WRITE_EXCLUSIVE_REG_ONLY: u32 = 3;
pub const PR_EXCLUSIVE_ACCESS_REG_ONLY: u32 = 4;
pub const PR_WRITE_EXCLUSIVE_ALL_REGS: u32 = 5;
pub const PR_EXCLUSIVE_ACCESS_ALL_REGS: u32 = 6;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PrReservation {
    pub reservation_key: u64,
    pub reservation_type: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PrRegistration {
    pub reservation_key: u64,
    pub service_action_reservation_key: u64,
    pub flags: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PrPreempt {
    pub reservation_key: u64,
    pub service_action_reservation_key: u64,
    pub reservation_type: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PrClear {
    pub reservation_key: u64,
    pub flags: u32,
    pub pad: u32,
}

const PR_IOC_MAGIC: u8 = b'p';
ioctl_write_ptr!(pr_register_ioctl, PR_IOC_MAGIC, 200, PrRegistration);
ioctl_write_ptr!(pr_reserve_ioctl, PR_IOC_MAGIC, 201, PrReservation);
ioctl_write_ptr!(pr_release_ioctl, PR_IOC_MAGIC, 202, PrReservation);
ioctl_write_ptr!(pr_preempt_ioctl, PR_IOC_MAGIC, 203, PrPreempt);
ioctl_write_ptr!(pr_preempt_abort_ioctl, PR_IOC_MAGIC, 204, PrPreempt);
ioctl_write_ptr!(pr_clear_ioctl, PR_IOC_MAGIC, 205, PrClear);

/// Issues a reservation register.
///
/// Returns a backend-specific status (NVMe or SCSI status).
pub fn pr_register(
    file: &fs::File,
    reservation_key: u64,
    service_action_reservation_key: u64,
    flags: u32,
) -> std::io::Result<i32> {
    let data = PrRegistration {
        reservation_key,
        service_action_reservation_key,
        flags,
        pad: 0,
    };

    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is legal to call on any valid FD.
    //         More documentation on this specific ioctl can be found in pr.h.
    let result = unsafe { pr_register_ioctl(file.as_raw_fd(), &data)? };
    Ok(result)
}

/// Issues a reservation reserve.
///
/// Returns a backend-specific status (NVMe or SCSI status).
pub fn pr_reserve(
    file: &fs::File,
    reservation_type: ReservationType,
    reservation_key: u64,
) -> std::io::Result<i32> {
    let data = PrReservation {
        reservation_key,
        reservation_type: linux_pr_type(reservation_type),
        flags: 0,
    };

    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is legal to call on any valid FD.
    //         More documentation on this specific ioctl can be found in pr.h.
    let result = unsafe { pr_reserve_ioctl(file.as_raw_fd(), &data)? };
    Ok(result)
}

/// Issues a reservation release.
///
/// Returns a backend-specific status (NVMe or SCSI status).
pub fn pr_release(
    file: &fs::File,
    reservation_type: ReservationType,
    reservation_key: u64,
) -> std::io::Result<i32> {
    let data = PrReservation {
        reservation_key,
        reservation_type: linux_pr_type(reservation_type),
        flags: 0,
    };

    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is legal to call on any valid FD.
    //         More documentation on this specific ioctl can be found in pr.h.
    let result = unsafe { pr_release_ioctl(file.as_raw_fd(), &data)? };
    Ok(result)
}

/// Issues a reservation preempty.
///
/// Returns a backend-specific status (NVMe or SCSI status).
pub fn pr_preempt(
    file: &fs::File,
    reservation_type: ReservationType,
    reservation_key: u64,
    service_action_reservation_key: u64,
    abort: bool,
) -> std::io::Result<i32> {
    let data = PrPreempt {
        reservation_key,
        service_action_reservation_key,
        reservation_type: linux_pr_type(reservation_type),
        flags: 0,
    };

    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is legal to call on any valid FD.
    //         More documentation on this specific ioctl can be found in pr.h.
    let result = unsafe {
        if abort {
            pr_preempt_abort_ioctl(file.as_raw_fd(), &data)?
        } else {
            pr_preempt_ioctl(file.as_raw_fd(), &data)?
        }
    };
    Ok(result)
}

/// Issues a reservation clear.
///
/// Returns a backend-specific status (NVMe or SCSI status).
pub fn pr_clear(file: &fs::File, reservation_key: u64) -> std::io::Result<i32> {
    let data = PrClear {
        reservation_key,
        flags: 0,
        pad: 0,
    };

    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is legal to call on any valid FD.
    //         More documentation on this specific ioctl can be found in pr.h.
    let result = unsafe { pr_clear_ioctl(file.as_raw_fd(), &data)? };
    Ok(result)
}

fn linux_pr_type(reservation_type: ReservationType) -> u32 {
    match reservation_type {
        ReservationType::WriteExclusive => PR_WRITE_EXCLUSIVE,
        ReservationType::ExclusiveAccess => PR_EXCLUSIVE_ACCESS,
        ReservationType::WriteExclusiveRegistrantsOnly => PR_WRITE_EXCLUSIVE_REG_ONLY,
        ReservationType::ExclusiveAccessRegistrantsOnly => PR_EXCLUSIVE_ACCESS_REG_ONLY,
        ReservationType::WriteExclusiveAllRegistrants => PR_WRITE_EXCLUSIVE_ALL_REGS,
        ReservationType::ExclusiveAccessAllRegistrants => PR_EXCLUSIVE_ACCESS_ALL_REGS,
    }
}
