// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the kernel interface of FUSE.
//!
//! This was derived from the official fuse.h from the Linux kernel sources. It represents
//! FUSE protocol version 7.31.
//!
//! For more details, see fuse.h.
#![allow(non_camel_case_types)]
#![expect(unused_parens)]

use bitfield_struct::bitfield;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/*
 * Version negotiation:
 *
 * Both the kernel and userspace send the version they support in the
 * INIT request and reply respectively.
 *
 * If the major versions match then both shall use the smallest
 * of the two minor versions for communication.
 *
 * If the kernel supports a larger major version, then userspace shall
 * reply with the major version it supports, ignore the rest of the
 * INIT message and expect a new INIT message from the kernel with a
 * matching major version.
 *
 * If the library supports a larger major version, then it shall fall
 * back to the major protocol version sent by the kernel for
 * communication and reply with that major version (and an arbitrary
 * supported minor version).
 */

/** Version number of this interface */
pub const FUSE_KERNEL_VERSION: u32 = 7;

/** Minor version number of this interface */
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 31;

/** The node ID of the root inode */
pub const FUSE_ROOT_ID: u64 = 1;

/* Make sure all structures are padded to 64bit boundary, so 32bit
userspace works under 64bit kernels */

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_attr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_sx_time {
    pub sec: i64,
    pub nsec: u32,
    pub _rsvd: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_statx {
    pub mask: u32,
    pub blksize: u32,
    pub attributes: u64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub mode: u16,
    pub _rsvd1: u16,
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub attributes_mask: u64,
    pub atime: fuse_sx_time,
    pub btime: fuse_sx_time,
    pub ctime: fuse_sx_time,
    pub mtime: fuse_sx_time,
    pub rdev_major: u32,
    pub rdev_minor: u32,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub _rsvd2: [u64; 14],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_kstatfs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
    pub padding: u32,
    pub spare: [u32; 6],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_file_lock {
    pub start: u64,
    pub end: u64,
    pub lock_type: u32,
    pub pid: u32, /* tgid */
}

/**
 * Bitmasks for fuse_setattr_in.valid
 */
pub const FATTR_MODE: u32 = (1 << 0);
pub const FATTR_UID: u32 = (1 << 1);
pub const FATTR_GID: u32 = (1 << 2);
pub const FATTR_SIZE: u32 = (1 << 3);
pub const FATTR_ATIME: u32 = (1 << 4);
pub const FATTR_MTIME: u32 = (1 << 5);
pub const FATTR_FH: u32 = (1 << 6);
pub const FATTR_ATIME_NOW: u32 = (1 << 7);
pub const FATTR_MTIME_NOW: u32 = (1 << 8);
pub const FATTR_LOCKOWNER: u32 = (1 << 9);
pub const FATTR_CTIME: u32 = (1 << 10);
pub const FATTR_KILL_SUIDGID: u32 = (1 << 11);

/**
 * Flags returned by the OPEN request
 *
 * FOPEN_DIRECT_IO: bypass page cache for this open file
 * FOPEN_KEEP_CACHE: don't invalidate the data cache on open
 * FOPEN_NONSEEKABLE: the file is not seekable
 * FOPEN_CACHE_DIR: allow caching this directory
 * FOPEN_STREAM: the file is stream-like (no file position at all)
 */
pub const FOPEN_DIRECT_IO: u32 = (1 << 0);
pub const FOPEN_KEEP_CACHE: u32 = (1 << 1);
pub const FOPEN_NONSEEKABLE: u32 = (1 << 2);
pub const FOPEN_CACHE_DIR: u32 = (1 << 3);
pub const FOPEN_STREAM: u32 = (1 << 4);

/**
 * INIT request/reply flags
 *
 * FUSE_ASYNC_READ: asynchronous read requests
 * FUSE_POSIX_LOCKS: remote locking for POSIX file locks
 * FUSE_FILE_OPS: kernel sends file handle for fstat, etc... (not yet supported)
 * FUSE_ATOMIC_O_TRUNC: handles the O_TRUNC open flag in the filesystem
 * FUSE_EXPORT_SUPPORT: filesystem handles lookups of "." and ".."
 * FUSE_BIG_WRITES: filesystem can handle write size larger than 4kB
 * FUSE_DONT_MASK: don't apply umask to file mode on create operations
 * FUSE_SPLICE_WRITE: kernel supports splice write on the device
 * FUSE_SPLICE_MOVE: kernel supports splice move on the device
 * FUSE_SPLICE_READ: kernel supports splice read on the device
 * FUSE_FLOCK_LOCKS: remote locking for BSD style file locks
 * FUSE_HAS_IOCTL_DIR: kernel supports ioctl on directories
 * FUSE_AUTO_INVAL_DATA: automatically invalidate cached pages
 * FUSE_DO_READDIRPLUS: do READDIRPLUS (READDIR+LOOKUP in one)
 * FUSE_READDIRPLUS_AUTO: adaptive readdirplus
 * FUSE_ASYNC_DIO: asynchronous direct I/O submission
 * FUSE_WRITEBACK_CACHE: use writeback cache for buffered writes
 * FUSE_NO_OPEN_SUPPORT: kernel supports zero-message opens
 * FUSE_PARALLEL_DIROPS: allow parallel lookups and readdir
 * FUSE_HANDLE_KILLPRIV: fs handles killing suid/sgid/cap on write/chown/trunc
 * FUSE_POSIX_ACL: filesystem supports posix acls
 * FUSE_ABORT_ERROR: reading the device after abort returns ECONNABORTED
 * FUSE_MAX_PAGES: init_out.max_pages contains the max number of req pages
 * FUSE_CACHE_SYMLINKS: cache READLINK responses
 * FUSE_NO_OPENDIR_SUPPORT: kernel supports zero-message opendir
 * FUSE_EXPLICIT_INVAL_DATA: only invalidate cached pages on explicit request
 * FUSE_MAP_ALIGNMENT: map_alignment field is valid
 */
pub const FUSE_ASYNC_READ: u32 = (1 << 0);
pub const FUSE_POSIX_LOCKS: u32 = (1 << 1);
pub const FUSE_FILE_OPS: u32 = (1 << 2);
pub const FUSE_ATOMIC_O_TRUNC: u32 = (1 << 3);
pub const FUSE_EXPORT_SUPPORT: u32 = (1 << 4);
pub const FUSE_BIG_WRITES: u32 = (1 << 5);
pub const FUSE_DONT_MASK: u32 = (1 << 6);
pub const FUSE_SPLICE_WRITE: u32 = (1 << 7);
pub const FUSE_SPLICE_MOVE: u32 = (1 << 8);
pub const FUSE_SPLICE_READ: u32 = (1 << 9);
pub const FUSE_FLOCK_LOCKS: u32 = (1 << 10);
pub const FUSE_HAS_IOCTL_DIR: u32 = (1 << 11);
pub const FUSE_AUTO_INVAL_DATA: u32 = (1 << 12);
pub const FUSE_DO_READDIRPLUS: u32 = (1 << 13);
pub const FUSE_READDIRPLUS_AUTO: u32 = (1 << 14);
pub const FUSE_ASYNC_DIO: u32 = (1 << 15);
pub const FUSE_WRITEBACK_CACHE: u32 = (1 << 16);
pub const FUSE_NO_OPEN_SUPPORT: u32 = (1 << 17);
pub const FUSE_PARALLEL_DIROPS: u32 = (1 << 18);
pub const FUSE_HANDLE_KILLPRIV: u32 = (1 << 19);
pub const FUSE_POSIX_ACL: u32 = (1 << 20);
pub const FUSE_ABORT_ERROR: u32 = (1 << 21);
pub const FUSE_MAX_PAGES: u32 = (1 << 22);
pub const FUSE_CACHE_SYMLINKS: u32 = (1 << 23);
pub const FUSE_NO_OPENDIR_SUPPORT: u32 = (1 << 24);
pub const FUSE_EXPLICIT_INVAL_DATA: u32 = (1 << 25);
pub const FUSE_MAP_ALIGNMENT: u32 = (1 << 26);
pub const FUSE_SUBMOUNTS: u32 = (1 << 27);
pub const FUSE_HANDLE_KILLPRIV_V2: u32 = (1 << 28);

/**
 * CUSE INIT request/reply flags
 *
 * CUSE_UNRESTRICTED_IOCTL:  use unrestricted ioctl
 */
pub const CUSE_UNRESTRICTED_IOCTL: u32 = (1 << 0);

/**
 * Release flags
 */
pub const FUSE_RELEASE_FLUSH: u32 = (1 << 0);
pub const FUSE_RELEASE_FLOCK_UNLOCK: u32 = (1 << 1);

/**
 * Getattr flags
 */
pub const FUSE_GETATTR_FH: u32 = (1 << 0);

/**
 * Lock flags
 */
pub const FUSE_LK_FLOCK: u32 = (1 << 0);

/**
 * WRITE flags
 *
 * FUSE_WRITE_CACHE: delayed write from page cache, file handle is guessed
 * FUSE_WRITE_LOCKOWNER: lock_owner field is valid
 * FUSE_WRITE_KILL_PRIV: kill suid and sgid bits
 */
pub const FUSE_WRITE_CACHE: u32 = (1 << 0);
pub const FUSE_WRITE_LOCKOWNER: u32 = (1 << 1);
pub const FUSE_WRITE_KILL_PRIV: u32 = (1 << 2);

/**
 * Read flags
 */
pub const FUSE_READ_LOCKOWNER: u32 = (1 << 1);

/**
 * Ioctl flags
 *
 * FUSE_IOCTL_COMPAT: 32bit compat ioctl on 64bit machine
 * FUSE_IOCTL_UNRESTRICTED: not restricted to well-formed ioctls, retry allowed
 * FUSE_IOCTL_RETRY: retry with new iovecs
 * FUSE_IOCTL_32BIT: 32bit ioctl
 * FUSE_IOCTL_DIR: is a directory
 * FUSE_IOCTL_COMPAT_X32: x32 compat ioctl on 64bit machine (64bit time_t)
 *
 * FUSE_IOCTL_MAX_IOV: maximum of in_iovecs + out_iovecs
 */
pub const FUSE_IOCTL_COMPAT: u32 = (1 << 0);
pub const FUSE_IOCTL_UNRESTRICTED: u32 = (1 << 1);
pub const FUSE_IOCTL_RETRY: u32 = (1 << 2);
pub const FUSE_IOCTL_32BIT: u32 = (1 << 3);
pub const FUSE_IOCTL_DIR: u32 = (1 << 4);
pub const FUSE_IOCTL_COMPAT_X32: u32 = (1 << 5);

pub const FUSE_IOCTL_MAX_IOV: u32 = 256;

/**
 * Poll flags
 *
 * FUSE_POLL_SCHEDULE_NOTIFY: request poll notify
 */
pub const FUSE_POLL_SCHEDULE_NOTIFY: u32 = (1 << 0);

/**
 * Fsync flags
 *
 * FUSE_FSYNC_FDATASYNC: Sync data only, not metadata
 */
pub const FUSE_FSYNC_FDATASYNC: u32 = (1 << 0);

pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_FORGET: u32 = 2; /* no reply */
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_SETATTR: u32 = 4;
pub const FUSE_READLINK: u32 = 5;
pub const FUSE_SYMLINK: u32 = 6;
pub const FUSE_MKNOD: u32 = 8;
pub const FUSE_MKDIR: u32 = 9;
pub const FUSE_UNLINK: u32 = 10;
pub const FUSE_RMDIR: u32 = 11;
pub const FUSE_RENAME: u32 = 12;
pub const FUSE_LINK: u32 = 13;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_WRITE: u32 = 16;
pub const FUSE_STATFS: u32 = 17;
pub const FUSE_RELEASE: u32 = 18;
pub const FUSE_FSYNC: u32 = 20;
pub const FUSE_SETXATTR: u32 = 21;
pub const FUSE_GETXATTR: u32 = 22;
pub const FUSE_LISTXATTR: u32 = 23;
pub const FUSE_REMOVEXATTR: u32 = 24;
pub const FUSE_FLUSH: u32 = 25;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_OPENDIR: u32 = 27;
pub const FUSE_READDIR: u32 = 28;
pub const FUSE_RELEASEDIR: u32 = 29;
pub const FUSE_FSYNCDIR: u32 = 30;
pub const FUSE_GETLK: u32 = 31;
pub const FUSE_SETLK: u32 = 32;
pub const FUSE_SETLKW: u32 = 33;
pub const FUSE_ACCESS: u32 = 34;
pub const FUSE_CREATE: u32 = 35;
pub const FUSE_INTERRUPT: u32 = 36;
pub const FUSE_BMAP: u32 = 37;
pub const FUSE_DESTROY: u32 = 38;
pub const FUSE_IOCTL: u32 = 39;
pub const FUSE_POLL: u32 = 40;
pub const FUSE_NOTIFY_REPLY: u32 = 41;
pub const FUSE_BATCH_FORGET: u32 = 42;
pub const FUSE_FALLOCATE: u32 = 43;
pub const FUSE_READDIRPLUS: u32 = 44;
pub const FUSE_RENAME2: u32 = 45;
pub const FUSE_LSEEK: u32 = 46;
pub const FUSE_COPY_FILE_RANGE: u32 = 47;
pub const FUSE_SETUPMAPPING: u32 = 48;
pub const FUSE_REMOVEMAPPING: u32 = 49;
pub const FUSE_SYNCFS: u32 = 50;
pub const FUSE_TMPFILE: u32 = 51;
pub const FUSE_STATX: u32 = 52;
/* Special Android FUSE operation */
pub const FUSE_CANONICAL_PATH: u32 = 2016;

/* CUSE specific operations */
pub const CUSE_INIT: u32 = 4096;

/* Reserved opcodes: helpful to detect structure endian-ness */
pub const CUSE_INIT_BSWAP_RESERVED: u32 = 1048576; /* CUSE_INIT << 8 */
pub const FUSE_INIT_BSWAP_RESERVED: u32 = 436207616; /* FUSE_INIT << 24 */

pub const FUSE_NOTIFY_POLL: u32 = 1;
pub const FUSE_NOTIFY_INVAL_INODE: u32 = 2;
pub const FUSE_NOTIFY_INVAL_ENTRY: u32 = 3;
pub const FUSE_NOTIFY_STORE: u32 = 4;
pub const FUSE_NOTIFY_RETRIEVE: u32 = 5;
pub const FUSE_NOTIFY_DELETE: u32 = 6;
pub const FUSE_NOTIFY_CODE_MAX: u32 = 7;

/* The read buffer is required to be at least 8k, but may be much larger */
pub const FUSE_MIN_READ_BUFFER: u32 = 8192;

pub const FUSE_COMPAT_ENTRY_OUT_SIZE: u32 = 120;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_entry_out {
    pub nodeid: u64, /* Inode ID */
    pub generation: u64, /* Inode generation: nodeid:gen must
                     be unique for the fs's lifetime */
    pub entry_valid: u64, /* Cache timeout for the name */
    pub attr_valid: u64,  /* Cache timeout for the attributes */
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: fuse_attr,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_forget_in {
    pub nlookup: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_forget_one {
    pub nodeid: u64,
    pub nlookup: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_batch_forget_in {
    pub count: u32,
    pub dummy: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_getattr_in {
    pub getattr_flags: u32,
    pub dummy: u32,
    pub fh: u64,
}

pub const FUSE_COMPAT_ATTR_OUT_SIZE: u32 = 96;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_attr_out {
    pub attr_valid: u64, /* Cache timeout for the attributes */
    pub attr_valid_nsec: u32,
    pub dummy: u32,
    pub attr: fuse_attr,
}

pub const FUSE_COMPAT_MKNOD_IN_SIZE: u32 = 8;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_mknod_in {
    pub mode: u32,
    pub rdev: u32,
    pub umask: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_mkdir_in {
    pub mode: u32,
    pub umask: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_rename_in {
    pub newdir: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_rename2_in {
    pub newdir: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_link_in {
    pub oldnodeid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_setattr_in {
    pub valid: u32,
    pub padding: u32,
    pub fh: u64,
    pub size: u64,
    pub lock_owner: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub unused4: u32,
    pub uid: u32,
    pub gid: u32,
    pub unused5: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_open_in {
    pub flags: u32,
    pub unused: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_create_in {
    pub flags: u32,
    pub mode: u32,
    pub umask: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_open_out {
    pub fh: u64,
    pub open_flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_release_in {
    pub fh: u64,
    pub flags: u32,
    pub release_flags: u32,
    pub lock_owner: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_flush_in {
    pub fh: u64,
    pub unused: u32,
    pub padding: u32,
    pub lock_owner: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_read_in {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub read_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

pub const FUSE_COMPAT_WRITE_IN_SIZE: u32 = 24;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_write_in {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub write_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_write_out {
    pub size: u32,
    pub padding: u32,
}

pub const FUSE_COMPAT_STATFS_SIZE: u32 = 48;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_statfs_out {
    pub st: fuse_kstatfs,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_fsync_in {
    pub fh: u64,
    pub fsync_flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_setxattr_in {
    pub size: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_getxattr_in {
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_getxattr_out {
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_lk_in {
    pub fh: u64,
    pub owner: u64,
    pub lk: fuse_file_lock,
    pub lk_flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_lk_out {
    pub lk: fuse_file_lock,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_access_in {
    pub mask: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_init_in {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
}

pub const FUSE_COMPAT_INIT_OUT_SIZE: u32 = 8;
pub const FUSE_COMPAT_22_INIT_OUT_SIZE: u32 = 24;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_init_out {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
    pub max_pages: u16,
    pub map_alignment: u16,
    pub unused: [u32; 8],
}

pub const CUSE_INIT_INFO_MAX: u32 = 4096;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct cuse_init_in {
    pub major: u32,
    pub minor: u32,
    pub unused: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct cuse_init_out {
    pub major: u32,
    pub minor: u32,
    pub unused: u32,
    pub flags: u32,
    pub max_read: u32,
    pub max_write: u32,
    pub dev_major: u32, /* chardev major */
    pub dev_minor: u32, /* chardev minor */
    pub spare: [u32; 10],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_interrupt_in {
    pub unique: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_bmap_in {
    pub block: u64,
    pub blocksize: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_bmap_out {
    pub block: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_ioctl_in {
    pub fh: u64,
    pub flags: u32,
    pub cmd: u32,
    pub arg: u64,
    pub in_size: u32,
    pub out_size: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_ioctl_iovec {
    pub base: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_ioctl_out {
    pub result: i32,
    pub flags: u32,
    pub in_iovs: u32,
    pub out_iovs: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_poll_in {
    pub fh: u64,
    pub kh: u64,
    pub flags: u32,
    pub events: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_poll_out {
    pub revents: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_notify_poll_wakeup_out {
    pub kh: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_fallocate_in {
    pub fh: u64,
    pub offset: u64,
    pub length: u64,
    pub mode: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_in_header {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_out_header {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_dirent {
    pub ino: u64,
    pub off: u64,
    pub namelen: u32,
    pub file_type: u32,
    // char name[];
}

pub fn fuse_dirent_align(x: usize) -> usize {
    (((x) + size_of::<u64>() - 1) & !(size_of::<u64>() - 1))
}
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_direntplus {
    pub entry_out: fuse_entry_out,
    pub dirent: fuse_dirent,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_notify_inval_inode_out {
    pub ino: u64,
    pub off: i64,
    pub len: i64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_notify_inval_entry_out {
    pub parent: u64,
    pub namelen: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_notify_delete_out {
    pub parent: u64,
    pub child: u64,
    pub namelen: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_notify_store_out {
    pub nodeid: u64,
    pub offset: u64,
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_notify_retrieve_out {
    pub notify_unique: u64,
    pub nodeid: u64,
    pub offset: u64,
    pub size: u32,
    pub padding: u32,
}

/* Matches the size of fuse_write_in */
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_notify_retrieve_in {
    pub dummy1: u64,
    pub offset: u64,
    pub size: u32,
    pub dummy2: u32,
    pub dummy3: u64,
    pub dummy4: u64,
}

/* Device ioctls: */
//  const FUSE_DEV_IOC_CLONE: u32 = _IOR(229, 0, u32);

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_lseek_in {
    pub fh: u64,
    pub offset: u64,
    pub whence: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_lseek_out {
    pub offset: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_copy_file_range_in {
    pub fh_in: u64,
    pub off_in: u64,
    pub nodeid_out: u64,
    pub fh_out: u64,
    pub off_out: u64,
    pub len: u64,
    pub flags: u64,
}

pub const FUSE_SETUPMAPPING_FLAG_WRITE: u64 = (1 << 0);

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_setupmapping_in {
    /* An already open handle */
    pub fh: u64,
    /* Offset into the file to start the mapping */
    pub foffset: u64,
    /* Length of mapping required */
    pub len: u64,
    /* Flags, FUSE_SETUPMAPPING_FLAG_* */
    pub flags: u64,
    /* memory offset in to dax window */
    pub moffset: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_removemapping_in {
    /* number of fuse_removemapping_one follows */
    pub count: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_removemapping_one {
    /* Offset into the dax to start the unmapping */
    pub moffset: u64,
    /* Length of mapping required */
    pub len: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_syncfs_in {
    pub padding: u64,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct StatxFlags {
    #[bits(13)]
    _rsvd1: u32,
    pub force_sync: bool,
    pub dont_sync: bool,
    #[bits(17)]
    _rsvd2: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_statx_in {
    pub getattr_flags: u32,
    _rsvd: u32,
    pub fh: u64,
    pub flags: StatxFlags,
    pub mask: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct fuse_statx_out {
    pub attr_valid: u64, /* Cache timeout for the attributes */
    pub attr_valid_nsec: u32,
    pub flags: StatxFlags,
    pub _rsvd: [u64; 2],
    pub statx: fuse_statx,
}
