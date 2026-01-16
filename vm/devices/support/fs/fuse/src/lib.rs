// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A Rust helper library for creating File-system in User Space (FUSE) daemons, aimed in
//! particular at virtio-fs.

#![expect(missing_docs)]

#[cfg(unix)]
mod conn;
pub mod protocol;
mod reply;
mod request;
mod session;
mod util;

#[cfg(target_os = "linux")]
pub use conn::Connection;
pub use reply::DirEntryWriter;
pub use reply::ReplySender;
pub use request::FuseOperation;
pub use request::Request;
pub use request::RequestReader;
pub use session::Session;
pub use session::SessionInfo;

use lx::LxStr;
use lx::LxString;
use protocol::*;
use std::time::Duration;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Reply data for the `create` operation.
///
/// The `create` operation includes two values in its reply, but fuse.h has no wrapper for the
/// combination of these values as they're just passed as separate arguments to `fuse_reply_create`
/// in libfuse.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CreateOut {
    pub entry: fuse_entry_out,
    pub open: fuse_open_out,
}

/// Trait that FUSE file systems must implement.
///
/// Most operations are loosely based on `fuse_lowlevel_ops` in libfuse, so check the [official
/// libfuse documentation](http://libfuse.github.io/doxygen/index.html) for more information on how
/// these operations should behave.
///
/// For many operations, a reply of `ENOSYS` is taken as permanent failure, preventing the client
/// from ever issuing that operation again.
pub trait Fuse {
    /// Looks up a child of an inode.
    ///
    /// This increases the lookup count of the found entry by one.
    fn lookup(&self, _request: &Request, _name: &LxStr) -> lx::Result<fuse_entry_out> {
        Err(lx::Error::ENOSYS)
    }

    /// Tells the FUSE file system to reduce the lookup count of an inode by the specified amount.
    ///
    /// # Note
    ///
    /// The client is not guaranteed to send a forget message for every inode if the file system
    /// is unmounted.
    fn forget(&self, _node_id: u64, _lookup_count: u64) {
        // No reply is allowed from this message, not even error.
    }

    /// Retrieves the attributes of a file.
    ///
    /// # Note
    ///
    /// If attributes are retrieved through an open file descriptor (i.e. using `fstat`), the
    /// `fh` parameter will be set to the file handle returned by the `open` call.
    fn get_attr(&self, _request: &Request, _flags: u32, _fh: u64) -> lx::Result<fuse_attr_out> {
        Err(lx::Error::ENOSYS)
    }

    /// Changes the attributes of a file.
    fn set_attr(&self, _request: &Request, _arg: &fuse_setattr_in) -> lx::Result<fuse_attr_out> {
        Err(lx::Error::ENOSYS)
    }

    /// Reads the target of a symbolic link.
    fn read_link(&self, _request: &Request) -> lx::Result<LxString> {
        Err(lx::Error::ENOSYS)
    }

    /// Creates a symbolic link as a child of the specified inode.
    fn symlink(
        &self,
        _request: &Request,
        _name: &LxStr,
        _target: &LxStr,
    ) -> lx::Result<fuse_entry_out> {
        Err(lx::Error::ENOSYS)
    }

    /// Creates a regular file, fifo, socket, or character or block device node as a child of
    /// the specified inode.
    fn mknod(
        &self,
        _request: &Request,
        _name: &LxStr,
        _arg: &fuse_mknod_in,
    ) -> lx::Result<fuse_entry_out> {
        Err(lx::Error::ENOSYS)
    }

    /// Creates a directory as a child of the specified inode.
    fn mkdir(
        &self,
        _request: &Request,
        _name: &LxStr,
        _arg: &fuse_mkdir_in,
    ) -> lx::Result<fuse_entry_out> {
        Err(lx::Error::ENOSYS)
    }

    /// Removes a non-directory child from the specified inode.
    fn unlink(&self, _request: &Request, _name: &LxStr) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Removes a directory child from the specified inode.
    fn rmdir(&self, _request: &Request, _name: &LxStr) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Renames a file.
    ///
    /// The file's original parent is the request's inode, while the new parent is indicated using
    /// `new_dir`.
    fn rename(
        &self,
        _request: &Request,
        _name: &LxStr,
        _new_dir: u64,
        _new_name: &LxStr,
        _flags: u32,
    ) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Creates a hard-link to an existing inode, as a child of the specified inode..
    fn link(&self, _request: &Request, _name: &LxStr, _target: u64) -> lx::Result<fuse_entry_out> {
        Err(lx::Error::ENOSYS)
    }

    /// Opens a file.
    ///
    /// If not implemented, this call will succeed, which can be used if the file system doesn't
    /// need any state for open files, since the inode number is also provided to functions
    /// such as `read` and `write`.
    fn open(&self, _request: &Request, _flags: u32) -> lx::Result<fuse_open_out> {
        Err(lx::Error::ENOSYS)
    }

    /// Reads data from an open file.
    fn read(&self, _request: &Request, _arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        Err(lx::Error::ENOSYS)
    }

    /// Writes data to an open file.
    fn write(&self, _request: &Request, _arg: &fuse_write_in, _data: &[u8]) -> lx::Result<usize> {
        Err(lx::Error::ENOSYS)
    }

    /// Retrieves the attributes of the file system.
    fn statfs(&self, _request: &Request) -> lx::Result<fuse_kstatfs> {
        Err(lx::Error::ENOSYS)
    }

    /// Closes an open file.
    ///
    /// If not implemented, this call will succeed. Won't be called if `open`
    /// returned `ENOSYS` (the default).
    fn release(&self, _request: &Request, _arg: &fuse_release_in) -> lx::Result<()> {
        Ok(())
    }

    /// Synchronize file contents.
    fn fsync(&self, _request: &Request, _fh: u64, _flags: u32) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Add or change an extended attribute on an inode.
    fn set_xattr(
        &self,
        _request: &Request,
        _name: &LxStr,
        _value: &[u8],
        _flags: u32,
    ) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Retrieve an extended attribute on an inode.
    fn get_xattr(&self, _request: &Request, _name: &LxStr, _size: u32) -> lx::Result<Vec<u8>> {
        Err(lx::Error::ENOSYS)
    }

    /// Retrieve the size of an extended attribute on an inode.
    fn get_xattr_size(&self, _request: &Request, _name: &LxStr) -> lx::Result<u32> {
        Err(lx::Error::ENOSYS)
    }

    /// List all extended attributes on an inode.
    fn list_xattr(&self, _request: &Request, _size: u32) -> lx::Result<Vec<u8>> {
        Err(lx::Error::ENOSYS)
    }

    /// Retrieve the size of the list of extended attributes on an inode.
    fn list_xattr_size(&self, _request: &Request) -> lx::Result<u32> {
        Err(lx::Error::ENOSYS)
    }

    /// Remove an extended attribute from an inode.
    fn remove_xattr(&self, _request: &Request, _name: &LxStr) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Called on each `close()` of a file descriptor for an opened file.
    ///
    /// This is called for every file descriptor, so may be called more than once.
    ///
    /// Use `release` to know when the last file descriptor was closed.
    fn flush(&self, _request: &Request, _arg: &fuse_flush_in) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Negotiate file system parameters with the client.
    fn init(&self, _info: &mut SessionInfo) {}

    /// Opens a directory.
    ///
    /// If not implemented, this call will succeed, which can be used if the file system doesn't
    /// need any state for open files, since the inode number is also provided to functions
    /// such as `read_dir`.
    fn open_dir(&self, _request: &Request, _flags: u32) -> lx::Result<fuse_open_out> {
        Err(lx::Error::ENOSYS)
    }

    /// Reads the contents of a directory.
    ///
    /// Use `DirEntryWriter` to create a buffer containing directory entries.
    fn read_dir(&self, _request: &Request, _arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        Err(lx::Error::ENOSYS)
    }

    /// Closes a directory.
    ///
    /// If not implemented, this call will succeed. Won't be called if `opendir`
    /// returned ENOSYS (the default).
    fn release_dir(&self, _request: &Request, _arg: &fuse_release_in) -> lx::Result<()> {
        Ok(())
    }

    /// Synchronize directory contents.
    fn fsync_dir(&self, _request: &Request, _fh: u64, _flags: u32) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Test for a POSIX file lock.
    fn get_lock(&self, _request: &Request, _arg: &fuse_lk_in) -> lx::Result<fuse_file_lock> {
        Err(lx::Error::ENOSYS)
    }

    /// Acquire, modify or release a POSIX file lock.
    ///
    /// If not implemented, the client still allows for local file locking.
    fn set_lock(&self, _request: &Request, _arg: &fuse_lk_in, _sleep: bool) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Check file access permissions.
    fn access(&self, _request: &Request, _mask: u32) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Create and open a file.
    ///
    /// If not implemented, the client will use `mknod` followed by `open`.
    fn create(
        &self,
        _request: &Request,
        _name: &LxStr,
        _arg: &fuse_create_in,
    ) -> lx::Result<CreateOut> {
        Err(lx::Error::ENOSYS)
    }

    /// Map a file block index to a device block index.
    ///
    /// This method is only relevant for file systems mounted using `fuseblk`.
    fn block_map(&self, _request: &Request, _block: u64, _block_size: u32) -> lx::Result<u64> {
        Err(lx::Error::ENOSYS)
    }

    /// Clean up the file system.
    ///
    /// For regular FUSE, the client only calls this for file systems mounted using `fuseblk`, but
    /// for other file systems `Connection` will call it when the `/dev/fuse` connection is closed.
    ///
    /// For virtio-fs, the client will call this when the file system is unmounted. After receiving
    /// destroy, another `init` call can be received if the file system is mounted again.
    fn destroy(&self) {}

    /// Submit an ioctl.
    ///
    /// # Note
    ///
    /// This is a somewhat limited subset of the ioctl functionality of libfuse; the additional
    /// functionality seems to only apply to CUSE, however.
    fn ioctl(
        &self,
        _request: &Request,
        _arg: &fuse_ioctl_in,
        _data: &[u8],
    ) -> lx::Result<(i32, Vec<u8>)> {
        Err(lx::Error::ENOSYS)
    }

    /// Allocate requested space.
    fn fallocate(&self, _request: &Request, _arg: &fuse_fallocate_in) -> lx::Result<()> {
        Err(lx::Error::ENOSYS)
    }

    /// Reads the contents of a directory, and performs a lookup on each entry.
    ///
    /// This function increases the lookup count of each entry in the directory by one.
    ///
    /// If you implement this, you must set `FUSE_DO_READDIRPLUS` in `init`. If you implement both
    /// read_dir_plus and read_dir, also set `FUSE_READDIRPLUS_AUTO`.
    fn read_dir_plus(&self, _request: &Request, _arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        Err(lx::Error::ENOSYS)
    }

    /// Find data holes in a sparse file.
    fn lseek(&self, _request: &Request, _fh: u64, _offset: u64, _whence: u32) -> lx::Result<u64> {
        Err(lx::Error::ENOSYS)
    }

    /// Copy data from one file to another without needing to send data through the FUSE kernel
    /// module.
    fn copy_file_range(
        &self,
        _request: &Request,
        _arg: &fuse_copy_file_range_in,
    ) -> lx::Result<usize> {
        Err(lx::Error::ENOSYS)
    }

    /// Create a DAX memory mapping.
    fn setup_mapping(
        &self,
        request: &Request,
        mapper: &dyn Mapper,
        arg: &fuse_setupmapping_in,
    ) -> lx::Result<()> {
        let _ = (request, mapper, arg);
        Err(lx::Error::ENOSYS)
    }

    /// Remove a DAX memory mapping.
    fn remove_mapping(
        &self,
        request: &Request,
        mapper: &dyn Mapper,
        moffset: u64,
        len: u64,
    ) -> lx::Result<()> {
        let _ = (request, mapper, moffset, len);
        Err(lx::Error::ENOSYS)
    }

    /// Retrieves the statx details of a file.
    ///
    /// # Note
    ///
    /// If information is retrieved through an open file descriptor (i.e. using `fstat`), the
    /// `fh` parameter will be set to the file handle returned by the `open` call.
    fn get_statx(
        &self,
        _request: &Request,
        _fh: u64,
        _getattr_flags: u32,
        _flags: StatxFlags,
        _mask: lx::StatExMask,
    ) -> lx::Result<fuse_statx_out> {
        Err(lx::Error::ENOSYS)
    }
}

#[cfg(windows)]
pub type FileRef<'a> = std::os::windows::io::BorrowedHandle<'a>;
#[cfg(unix)]
pub type FileRef<'a> = std::os::unix::io::BorrowedFd<'a>;

/// Trait for mapping files into a shared memory region.
///
/// This is used to support DAX with virtio-fs.
pub trait Mapper {
    /// Map memory into the region at `offset`.
    fn map(
        &self,
        offset: u64,
        file: FileRef<'_>,
        file_offset: u64,
        len: u64,
        writable: bool,
    ) -> lx::Result<()>;

    /// Unmaps any memory in the given range.
    fn unmap(&self, offset: u64, len: u64) -> lx::Result<()>;

    /// Clears any mappings in the range.
    fn clear(&self);
}

impl fuse_entry_out {
    /// Create a new `fuse_entry_out`.
    pub fn new(node_id: u64, entry_valid: Duration, attr_valid: Duration, attr: fuse_attr) -> Self {
        Self {
            nodeid: node_id,
            generation: 0,
            entry_valid: entry_valid.as_secs(),
            entry_valid_nsec: entry_valid.subsec_nanos(),
            attr_valid: attr_valid.as_secs(),
            attr_valid_nsec: attr_valid.subsec_nanos(),
            attr,
        }
    }
}

impl fuse_attr_out {
    /// Create a new `fuse_attr_out`.
    pub fn new(valid: Duration, attr: fuse_attr) -> Self {
        Self {
            attr_valid: valid.as_secs(),
            attr_valid_nsec: valid.subsec_nanos(),
            dummy: 0,
            attr,
        }
    }
}

impl fuse_statx_out {
    /// Create a new `fuse_statx_out`.
    pub fn new(valid: Duration, flags: StatxFlags, statx: fuse_statx) -> Self {
        Self {
            attr_valid: valid.as_secs(),
            attr_valid_nsec: valid.subsec_nanos(),
            flags,
            statx,
            _rsvd: [0; 2],
        }
    }
}

impl fuse_open_out {
    /// Create a new `fuse_open_out`.
    pub fn new(fh: u64, open_flags: u32) -> Self {
        Self {
            fh,
            open_flags,
            padding: 0,
        }
    }
}

impl fuse_kstatfs {
    /// Create a new `fuse_kstatfs`.
    pub fn new(
        blocks: u64,
        bfree: u64,
        bavail: u64,
        files: u64,
        ffree: u64,
        bsize: u32,
        namelen: u32,
        frsize: u32,
    ) -> Self {
        Self {
            blocks,
            bfree,
            bavail,
            files,
            ffree,
            bsize,
            namelen,
            frsize,
            padding: 0,
            spare: Default::default(),
        }
    }
}
