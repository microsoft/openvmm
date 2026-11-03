// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(clippy::field_reassign_with_default)] // protocol code benefits from imperative field assignment

use fuse::protocol::*;
use std::time::Duration;

/// Validates FUSE OPEN/OPENDIR flags and consumes guest-only Linux flags.
pub fn open_flags(flags: u32) -> lx::Result<i32> {
    if flags & (lx::O_CREAT | lx::O_EXCL) as u32 != 0 {
        return Err(lx::Error::EINVAL);
    }
    create_flags(flags)
}

/// Validates FUSE CREATE flags and consumes guest-only Linux flags.
pub fn create_flags(flags: u32) -> lx::Result<i32> {
    let guest_only = lx::__FMODE_EXEC | lx::O_LARGEFILE | lx::O_NOFOLLOW;
    let supported = (lx::O_ACCESS_MASK
        | lx::O_CREAT
        | lx::O_EXCL
        | lx::O_TRUNC
        | lx::O_APPEND
        | lx::O_NONBLOCK
        | lx::O_DSYNC
        | lx::O_DIRECT
        | lx::O_DIRECTORY
        | lx::O_NOATIME
        | lx::O_SYNC
        | guest_only) as u32;
    if flags & !supported != 0 {
        return Err(lx::Error::EINVAL);
    }
    // Guest-only intent and flags do not select host-open policy.
    Ok(flags as i32 & !guest_only)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn fuse_flags_only_consume_guest_flags() {
        let guest_flags = lx::__FMODE_EXEC | lx::O_LARGEFILE | lx::O_NOFOLLOW;
        for flags in [
            lx::O_RDWR | lx::O_APPEND | lx::O_TRUNC | lx::O_DIRECT,
            lx::O_DIRECTORY | lx::O_SYNC,
            lx::O_NOACCESS | lx::O_DSYNC | lx::O_NOATIME | lx::O_NONBLOCK,
            lx::O_SYNC & !lx::O_DSYNC,
        ] {
            assert_eq!(open_flags((flags | guest_flags) as u32).unwrap(), flags);
        }
    }

    #[test]
    fn fuse_flags_reject_unsupported_bits() {
        let guest_flags = (lx::__FMODE_EXEC | lx::O_LARGEFILE | lx::O_NOFOLLOW) as u32;
        for unsupported in [
            lx::O_NOCTTY as u32,
            lx::O_CLOEXEC as u32,
            lx::O_PATH as u32,
            0x2000,
            0x400000,
            1 << 31,
        ] {
            let flags = unsupported | guest_flags | (lx::O_WRONLY | lx::O_TRUNC) as u32;
            assert_eq!(open_flags(flags).unwrap_err(), lx::Error::EINVAL);
            assert_eq!(create_flags(flags).unwrap_err(), lx::Error::EINVAL);
        }
    }

    #[test]
    fn fuse_creation_flags_require_create_request() {
        for flags in [lx::O_CREAT, lx::O_EXCL, lx::O_CREAT | lx::O_EXCL] {
            let flags = flags | lx::O_RDWR | lx::O_TRUNC;
            assert_eq!(open_flags(flags as u32).unwrap_err(), lx::Error::EINVAL);
            assert_eq!(
                create_flags((flags | lx::__FMODE_EXEC | lx::O_LARGEFILE) as u32).unwrap(),
                flags
            );
        }
    }
}
