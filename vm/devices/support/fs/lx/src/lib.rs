// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides constants and types derived from Linux without requiring libc.

#![expect(missing_docs)]

#[macro_use]
mod macros;
mod string;

use std::io;
use thiserror::Error;

pub use string::LxStr;
pub use string::LxString;

#[expect(non_camel_case_types)]
pub type uid_t = u32;
#[expect(non_camel_case_types)]
pub type gid_t = u32;
#[expect(non_camel_case_types)]
pub type mode_t = u32;
#[expect(non_camel_case_types)]
pub type ino_t = u64;
#[expect(non_camel_case_types)]
pub type off_t = i64;
#[expect(non_camel_case_types)]
pub type dev_t = usize;

pub const MODE_INVALID: mode_t = mode_t::MAX;
pub const MODE_VALID_BITS: mode_t = S_IFMT | 0o7777;
pub const UID_INVALID: uid_t = uid_t::MAX;
pub const GID_INVALID: gid_t = gid_t::MAX;

pub const S_IFIFO: u32 = 0x1000;
pub const S_IFCHR: u32 = 0x2000;
pub const S_IFDIR: u32 = 0x4000;
pub const S_IFBLK: u32 = 0x6000;
pub const S_IFREG: u32 = 0x8000;
pub const S_IFLNK: u32 = 0xa000;
pub const S_IFSOCK: u32 = 0xc000;
pub const S_IFMT: u32 = 0xf000;
pub const S_ISUID: u32 = 0o4000;
pub const S_ISGID: u32 = 0o2000;
pub const S_IXGRP: u32 = 0o010;

pub const DT_UNK: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;
pub const DT_WHT: u8 = 14;

lx_errors! {
    EPERM = 1;
    ENOENT = 2;
    ESRCH = 3;
    EINTR = 4;
    EIO = 5;
    ENXIO = 6;
    E2BIG = 7;
    ENOEXEC = 8;
    EBADF = 9;
    ECHILD = 10;
    EAGAIN = 11;
    ENOMEM = 12;
    EACCES = 13;
    EFAULT = 14;
    EBUSY = 16;
    EEXIST = 17;
    EXDEV = 18;
    ENODEV = 19;
    ENOTDIR = 20;
    EISDIR = 21;
    EINVAL = 22;
    ENFILE = 23;
    EMFILE = 24;
    ENOTTY = 25;
    EFBIG = 27;
    ENOSPC = 28;
    ESPIPE = 29;
    EROFS = 30;
    EMLINK = 31;
    EPIPE = 32;
    ERANGE = 34;
    EDEADLK = 35;
    ENAMETOOLONG = 36;
    ENOLCK = 37;
    ENOSYS = 38;
    ENOTEMPTY = 39;
    ELOOP = 40;
    EIDRM = 43;
    ENODATA = 61;
    EPROTO = 71;
    EOVERFLOW = 75;
    EUSERS = 87;
    ENOTSOCK = 88;
    EDESTADDRREQ = 89;
    EMSGSIZE = 90;
    EPROTOTYPE = 91;
    ENOPROTOOPT = 92;
    EPROTONOSUPPORT = 93;
    ESOCKTNOSUPPORT = 94;
    ENOTSUP = 95;
    EAFNOSUPPORT = 97;
    EADDRINUSE = 98;
    EADDRNOTAVAIL = 99;
    ENETUNREACH = 101;
    ECONNABORTED = 103;
    ECONNRESET = 104;
    ENOBUFS = 105;
    EISCONN = 106;
    ENOTCONN = 107;
    ETIMEDOUT = 110;
    ECONNREFUSED = 111;
    EHOSTDOWN = 112;
    EHOSTUNREACH = 113;
    EALREADY = 114;
    EINPROGRESS = 115;
    ENOMEDIUM = 123;
    EMEDIUMTYPE = 124;
    ENOKEY = 126;
}

pub const O_RDONLY: i32 = 0x000000;
pub const O_WRONLY: i32 = 0x000001;
pub const O_RDWR: i32 = 0x000002;
pub const O_NOACCESS: i32 = 0x000003;
pub const O_CREAT: i32 = 0x000040;
pub const O_EXCL: i32 = 0x000080;
pub const O_TRUNC: i32 = 0x000200;
pub const O_APPEND: i32 = 0x000400;

// xtask-fmt allow-target-arch sys-crate
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub const O_DIRECTORY: i32 = 0x010000;
// xtask-fmt allow-target-arch sys-crate
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub const O_NOFOLLOW: i32 = 0x020000;

// xtask-fmt allow-target-arch sys-crate
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
pub const O_DIRECTORY: i32 = 0x004000;
// xtask-fmt allow-target-arch sys-crate
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
pub const O_NOFOLLOW: i32 = 0x008000;

pub const O_NOATIME: i32 = 0x040000;

pub const O_ACCESS_MASK: i32 = 0x000003;

pub const AT_REMOVEDIR: i32 = 0x200;

pub const XATTR_CREATE: i32 = 0x1;
pub const XATTR_REPLACE: i32 = 0x2;

/// Wraps a Linux error code in a strongly-typed struct.
#[derive(Copy, Clone, Error, Eq, PartialEq)]
#[error("{err} ({0})", err = str_error(*.0))]
pub struct Error(i32);

impl From<io::Error> for Error {
    // Map IO errors to the appropriate Linux error code.
    fn from(error: io::Error) -> Self {
        let e = match error.kind() {
            io::ErrorKind::NotFound => ENOENT,
            io::ErrorKind::PermissionDenied => EACCES,
            io::ErrorKind::ConnectionRefused => ECONNREFUSED,
            io::ErrorKind::ConnectionReset => ECONNRESET,
            io::ErrorKind::ConnectionAborted => ECONNABORTED,
            io::ErrorKind::NotConnected => ENOTCONN,
            io::ErrorKind::AddrInUse => EADDRINUSE,
            io::ErrorKind::AddrNotAvailable => EADDRNOTAVAIL,
            io::ErrorKind::BrokenPipe => EPIPE,
            io::ErrorKind::AlreadyExists => EEXIST,
            io::ErrorKind::WouldBlock => EAGAIN,
            io::ErrorKind::TimedOut => ETIMEDOUT,
            io::ErrorKind::Interrupted => EINTR,
            _ => EINVAL,
        };

        Error(e)
    }
}

impl Error {
    /// Creates an `Error` from the last operating system error.
    #[cfg(target_os = "linux")]
    pub fn last_os_error() -> Self {
        Self(io::Error::last_os_error().raw_os_error().unwrap())
    }

    /// Creates an `Error` from an existing Linux error code.
    pub fn from_lx(error: i32) -> Self {
        Self(error)
    }

    /// Returns the error code value.
    pub fn value(&self) -> i32 {
        self.0
    }
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Error({} ({}))", str_error(self.0), self.0))
    }
}

/// A specialized `Result` type for operations that return Linux error codes.
pub type Result<T> = std::result::Result<T, Error>;

pub const UTIME_NOW: usize = (1 << 30) - 1;
pub const UTIME_OMIT: usize = (1 << 30) - 2;

/// A Linux `timespec` structure.
///
/// This is similar to `Duration` but matches the memory layout of `timespec`.
#[repr(C)]
#[derive(Debug, Eq, PartialEq)]
pub struct Timespec {
    pub seconds: usize,
    pub nanoseconds: usize,
}

impl Timespec {
    /// Creates a `Timespec` with the value UTIME_OMIT.
    pub fn omit() -> Self {
        Self {
            seconds: 0,
            nanoseconds: UTIME_OMIT,
        }
    }

    /// Creates a `Timespec` with the value UTIME_NOW.
    pub fn now() -> Self {
        Self {
            seconds: 0,
            nanoseconds: UTIME_NOW,
        }
    }
}

impl From<&std::time::Duration> for Timespec {
    fn from(time: &std::time::Duration) -> Self {
        Self {
            seconds: time.as_secs() as usize,
            nanoseconds: time.subsec_nanos() as usize,
        }
    }
}

/// A Linux `stat` structure.
#[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
#[repr(C)]
#[derive(Debug, Eq, PartialEq)]
pub struct Stat {
    pub device_nr: u64,
    pub inode_nr: ino_t,
    pub link_count: usize,
    pub mode: mode_t,
    pub uid: uid_t,
    pub gid: gid_t,
    pub pad0: u32,
    pub device_nr_special: u64,
    pub file_size: u64,
    pub block_size: isize,
    pub block_count: u64,
    pub access_time: Timespec,
    pub write_time: Timespec,
    pub change_time: Timespec,
    pub pad1: [isize; 3],
}

/// A Linux `stat` structure.
#[cfg(target_arch = "aarch64")] // xtask-fmt allow-target-arch sys-crate
#[repr(C)]
#[derive(Debug, Eq, PartialEq)]
pub struct Stat {
    pub device_nr: u64,
    pub inode_nr: ino_t,
    pub mode: mode_t,
    pub link_count: u32,
    pub uid: uid_t,
    pub gid: gid_t,
    pub device_nr_special: u64,
    pub pad0: u32,
    pub file_size: u64,
    pub block_size: u32,
    pub pad2: u32,
    pub block_count: u64,
    pub access_time: Timespec,
    pub write_time: Timespec,
    pub change_time: Timespec,
    pub unused: [u32; 2],
}

#[repr(C)]
#[derive(Debug, Eq, PartialEq)]
pub struct StatFs {
    pub fs_type: usize,
    pub block_size: usize,
    pub block_count: u64,
    pub free_block_count: u64,
    pub available_block_count: u64,
    pub file_count: u64,
    pub available_file_count: u64,
    pub file_system_id: [u8; 8],
    pub maximum_file_name_length: usize,
    pub file_record_size: usize,
    pub flags: usize,
    pub spare: [usize; 4],
}

/// A directory entry returned by `LxFile::read_dir`.
#[derive(Debug)]
pub struct DirEntry {
    pub name: LxString,
    pub inode_nr: ino_t,
    pub offset: off_t,
    pub file_type: u8,
}

pub fn s_isreg(mode: mode_t) -> bool {
    mode & S_IFMT == S_IFREG
}

pub fn s_isdir(mode: mode_t) -> bool {
    mode & S_IFMT == S_IFDIR
}

pub fn s_ischr(mode: mode_t) -> bool {
    mode & S_IFMT == S_IFCHR
}

pub fn s_isblk(mode: mode_t) -> bool {
    mode & S_IFMT == S_IFBLK
}

pub fn s_isfifo(mode: mode_t) -> bool {
    mode & S_IFMT == S_IFIFO
}

pub fn s_issock(mode: mode_t) -> bool {
    mode & S_IFMT == S_IFSOCK
}

pub fn s_islnk(mode: mode_t) -> bool {
    mode & S_IFMT == S_IFLNK
}

pub fn major32(dev: dev_t) -> u32 {
    ((dev & 0xfff00) >> 8) as u32
}

pub fn make_major32(major: u32) -> dev_t {
    ((major as dev_t) & 0xfff) << 8
}

pub fn major64(dev: dev_t) -> u32 {
    (((dev >> 32) & 0xfffff000) | major32(dev) as dev_t) as u32
}

pub fn make_major64(major: u32) -> dev_t {
    (((major as dev_t) & !0xfff) << 32) | make_major32(major)
}

pub fn minor(dev: dev_t) -> u32 {
    (((dev >> 12) & 0xffffff00) | (dev & 0xff)) as u32
}

pub fn make_minor(minor: u32) -> dev_t {
    ((minor as dev_t & 0xffffff00) << 12) | (minor as dev_t & 0xff)
}

pub fn make_dev(major: u32, minor: u32) -> dev_t {
    make_major64(major) | make_minor(minor)
}
