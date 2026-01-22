// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(clippy::field_reassign_with_default)] // protocol code benefits from imperative field assignment

use fuse::protocol::*;
use std::time::Duration;

/// Convert a Linux stat struct to FUSE attributes.
pub fn stat_to_fuse_attr(stat: &lx::Stat) -> fuse_attr {
    fuse_attr {
        ino: stat.inode_nr,
        size: stat.file_size,
        blocks: stat.block_count,
        atime: stat.access_time.seconds as u64,
        mtime: stat.write_time.seconds as u64,
        ctime: stat.change_time.seconds as u64,
        atimensec: stat.access_time.nanoseconds as u32,
        mtimensec: stat.write_time.nanoseconds as u32,
        ctimensec: stat.change_time.nanoseconds as u32,
        mode: stat.mode,
        // This is `usize` on x64 and `u32` on arm64, avoid a warning.
        nlink: stat.link_count as _,
        uid: stat.uid,
        gid: stat.gid,
        rdev: stat.device_nr_special as u32,
        // This is `usize` on x64 and `u32` on arm64, avoid a warning.
        blksize: stat.block_size as _,
        padding: 0,
    }
}

pub fn statex_timestamp_to_fuse_sx_time(ts: &lx::StatExTimestamp) -> fuse_sx_time {
    fuse_sx_time {
        sec: ts.seconds,
        nsec: ts.nanoseconds,
        _rsvd: 0,
    }
}

/// Convert a Linux stat struct to FUSE extended attributes.
pub fn statx_to_fuse_statx(stat: &lx::StatEx) -> fuse_statx {
    fuse_statx {
        mask: stat.mask.into_bits(),
        blksize: stat.block_size,
        attributes: stat.attributes.into_bits(),
        nlink: stat.link_count,
        uid: stat.uid,
        gid: stat.gid,
        mode: stat.mode,
        ino: stat.inode_id,
        size: stat.file_size,
        blocks: stat.block_count,
        attributes_mask: stat.attributes_mask.into_bits(),
        atime: statex_timestamp_to_fuse_sx_time(&stat.access_time),
        btime: statex_timestamp_to_fuse_sx_time(&stat.creation_time),
        mtime: statex_timestamp_to_fuse_sx_time(&stat.write_time),
        ctime: statex_timestamp_to_fuse_sx_time(&stat.change_time),
        rdev_major: stat.rdev_major,
        rdev_minor: stat.rdev_minor,
        dev_major: stat.dev_major,
        dev_minor: stat.dev_minor,
        _rsvd1: 0,
        _rsvd2: [0; 14],
    }
}

/// Convert a FUSE setattr message to a lxutil `SetAttributes` struct.
pub fn fuse_set_attr_to_lxutil(
    arg: &fuse_setattr_in,
    thread_uid: lx::uid_t,
) -> lxutil::SetAttributes {
    let mut attr = lxutil::SetAttributes::default();
    attr.thread_uid = thread_uid;
    if arg.valid & FATTR_MODE != 0 {
        attr.mode = Some(arg.mode);
    }

    if arg.valid & FATTR_UID != 0 {
        attr.uid = Some(arg.uid);
    }

    if arg.valid & FATTR_GID != 0 {
        attr.gid = Some(arg.gid);
    }

    if arg.valid & FATTR_SIZE != 0 {
        attr.size = Some(arg.size as i64);
    }

    if arg.valid & FATTR_ATIME != 0 {
        attr.atime = if arg.valid & FATTR_ATIME_NOW != 0 {
            lxutil::SetTime::Now
        } else {
            lxutil::SetTime::Set(Duration::new(arg.atime, arg.atimensec))
        };
    }

    if arg.valid & FATTR_MTIME != 0 {
        attr.mtime = if arg.valid & FATTR_MTIME_NOW != 0 {
            lxutil::SetTime::Now
        } else {
            lxutil::SetTime::Set(Duration::new(arg.mtime, arg.mtimensec))
        };
    }

    if arg.valid & FATTR_CTIME != 0 {
        attr.ctime = lxutil::SetTime::Set(Duration::new(arg.ctime, arg.ctimensec));
    }

    attr
}
