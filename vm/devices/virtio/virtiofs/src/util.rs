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
///
/// Fails with `EINVAL`, as `utimensat` does, when the message sets a time to a
/// value whose nanoseconds are a second or more.
pub fn fuse_set_attr_to_lxutil(
    arg: &fuse_setattr_in,
    thread_uid: lx::uid_t,
) -> lx::Result<lxutil::SetAttributes> {
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
            lxutil::SetTime::Set(fuse_time_to_duration(arg.atime, arg.atimensec)?)
        };
    }

    if arg.valid & FATTR_MTIME != 0 {
        attr.mtime = if arg.valid & FATTR_MTIME_NOW != 0 {
            lxutil::SetTime::Now
        } else {
            lxutil::SetTime::Set(fuse_time_to_duration(arg.mtime, arg.mtimensec)?)
        };
    }

    if arg.valid & FATTR_CTIME != 0 {
        attr.ctime = lxutil::SetTime::Set(fuse_time_to_duration(arg.ctime, arg.ctimensec)?);
    }

    Ok(attr)
}

/// Converts a FUSE time of `seconds` and `nanoseconds` to a `Duration`.
///
/// Fails with `EINVAL`, as `utimensat` does, when `nanoseconds` is a second or
/// more, rather than carrying it into `seconds`, which can overflow.
fn fuse_time_to_duration(seconds: u64, nanoseconds: u32) -> lx::Result<Duration> {
    const NANOSECONDS_PER_SECOND: u32 = 1_000_000_000;
    if nanoseconds >= NANOSECONDS_PER_SECOND {
        return Err(lx::Error::EINVAL);
    }
    Ok(Duration::new(seconds, nanoseconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    /// A `SETATTR` that sets every time that `valid` selects to `seconds` and
    /// `nanoseconds`.
    fn set_times(valid: u32, seconds: u64, nanoseconds: u32) -> fuse_setattr_in {
        fuse_setattr_in {
            valid,
            atime: seconds,
            mtime: seconds,
            ctime: seconds,
            atimensec: nanoseconds,
            mtimensec: nanoseconds,
            ctimensec: nanoseconds,
            ..fuse_setattr_in::new_zeroed()
        }
    }

    /// The time that `attr` sets for the `FATTR_*` time bit `valid`.
    fn set_time(attr: &lxutil::SetAttributes, valid: u32) -> lxutil::SetTime {
        match valid {
            FATTR_ATIME => attr.atime,
            FATTR_MTIME => attr.mtime,
            FATTR_CTIME => attr.ctime,
            _ => unreachable!("{valid:#x} is not a time"),
        }
    }

    /// A time set to a value keeps nanoseconds below one second as they are,
    /// and fails with `EINVAL` for a second or more, as `utimensat` does,
    /// rather than carrying them into its seconds, which can overflow.
    #[test]
    fn set_attr_times_require_nanoseconds_below_one_second() {
        for valid in [FATTR_ATIME, FATTR_MTIME, FATTR_CTIME] {
            for seconds in [0, u64::MAX] {
                for nanoseconds in [0, 999_999_999] {
                    let arg = set_times(valid, seconds, nanoseconds);
                    let attr = fuse_set_attr_to_lxutil(&arg, 0).unwrap();
                    assert!(
                        matches!(
                            set_time(&attr, valid),
                            lxutil::SetTime::Set(time)
                                if (time.as_secs(), time.subsec_nanos()) == (seconds, nanoseconds)
                        ),
                        "valid {valid:#x}, {seconds} s, {nanoseconds} ns"
                    );
                }
                for nanoseconds in [1_000_000_000, u32::MAX] {
                    let arg = set_times(valid, seconds, nanoseconds);
                    assert_eq!(
                        fuse_set_attr_to_lxutil(&arg, 0).err(),
                        Some(lx::Error::EINVAL),
                        "valid {valid:#x}, {seconds} s, {nanoseconds} ns"
                    );
                }
            }
        }
    }

    /// Only a time set to a value takes its seconds and nanoseconds: a time set
    /// to the current time ignores them, as does a time left unchanged.
    #[test]
    fn set_attr_ignores_times_not_set_to_a_value() {
        let valid = FATTR_ATIME | FATTR_ATIME_NOW | FATTR_MTIME | FATTR_MTIME_NOW;
        let attr = fuse_set_attr_to_lxutil(&set_times(valid, u64::MAX, u32::MAX), 0).unwrap();
        assert!(matches!(attr.atime, lxutil::SetTime::Now));
        assert!(matches!(attr.mtime, lxutil::SetTime::Now));
        assert!(matches!(attr.ctime, lxutil::SetTime::Omit));
    }

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
