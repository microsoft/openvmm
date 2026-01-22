// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[macro_use]
mod macros;

use crate::protocol::*;
use std::io;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Define an enum for all operations and their arguments.
fuse_operations! {
    FUSE_LOOKUP Lookup name:name;
    FUSE_FORGET Forget arg:fuse_forget_in;
    FUSE_GETATTR GetAttr arg:fuse_getattr_in;
    FUSE_SETATTR SetAttr arg:fuse_setattr_in;
    FUSE_READLINK ReadLink;
    FUSE_SYMLINK Symlink name:name target:str;
    FUSE_MKNOD MkNod arg:fuse_mknod_in name:name;
    FUSE_MKDIR MkDir arg:fuse_mkdir_in name:name;
    FUSE_UNLINK Unlink name:name;
    FUSE_RMDIR RmDir name:name;
    FUSE_RENAME Rename arg:fuse_rename_in name:name new_name:name;
    FUSE_LINK Link arg:fuse_link_in name:name;
    FUSE_OPEN Open arg:fuse_open_in;
    FUSE_READ Read arg:fuse_read_in;
    FUSE_WRITE Write arg:fuse_write_in data:[u8; arg.size];
    FUSE_STATFS StatFs;
    FUSE_RELEASE Release arg:fuse_release_in;
    FUSE_FSYNC FSync arg:fuse_fsync_in;
    FUSE_SETXATTR SetXAttr arg:fuse_setxattr_in name:str value:[u8; arg.size];
    FUSE_GETXATTR GetXAttr arg:fuse_getxattr_in name:str;
    FUSE_LISTXATTR ListXAttr arg:fuse_getxattr_in;
    FUSE_REMOVEXATTR RemoveXAttr name:str;
    FUSE_FLUSH Flush arg:fuse_flush_in;
    FUSE_INIT Init arg:fuse_init_in;
    FUSE_OPENDIR OpenDir arg:fuse_open_in;
    FUSE_READDIR ReadDir arg:fuse_read_in;
    FUSE_RELEASEDIR ReleaseDir arg:fuse_release_in;
    FUSE_FSYNCDIR FSyncDir arg:fuse_fsync_in;
    FUSE_GETLK GetLock arg:fuse_lk_in;
    FUSE_SETLK SetLock arg:fuse_lk_in;
    FUSE_SETLKW SetLockSleep arg:fuse_lk_in;
    FUSE_ACCESS Access arg:fuse_access_in;
    FUSE_CREATE Create arg:fuse_create_in name:str;
    FUSE_INTERRUPT Interrupt arg:fuse_interrupt_in;
    FUSE_BMAP BMap arg:fuse_bmap_in;
    FUSE_DESTROY Destroy;
    FUSE_IOCTL Ioctl arg:fuse_ioctl_in data:[u8; arg.in_size];
    FUSE_POLL Poll arg:fuse_poll_in;
    FUSE_NOTIFY_REPLY NotifyReply arg:fuse_notify_retrieve_in data:[u8];
    FUSE_BATCH_FORGET BatchForget arg:fuse_batch_forget_in nodes:[u8];
    FUSE_FALLOCATE FAllocate arg:fuse_fallocate_in;
    FUSE_READDIRPLUS ReadDirPlus arg:fuse_read_in;
    FUSE_RENAME2 Rename2 arg:fuse_rename2_in name:name new_name:name;
    FUSE_LSEEK LSeek arg:fuse_lseek_in;
    FUSE_COPY_FILE_RANGE CopyFileRange arg:fuse_copy_file_range_in;
    FUSE_SETUPMAPPING SetupMapping arg:fuse_setupmapping_in;
    FUSE_REMOVEMAPPING RemoveMapping arg:fuse_removemapping_in mappings:[u8];
    FUSE_SYNCFS SyncFs _arg:fuse_syncfs_in;
    FUSE_STATX StatX arg:fuse_statx_in;
    FUSE_CANONICAL_PATH CanonicalPath;
}

/// A request received from the FUSE kernel module.
pub struct Request {
    header: fuse_in_header,
    operation: FuseOperation,
}

impl Request {
    /// Create a new request from the specified data.
    pub fn new(mut reader: impl RequestReader) -> lx::Result<Self> {
        let header: fuse_in_header = reader.read_type()?;
        let operation = Self::read_operation(&header, reader);
        Ok(Self { header, operation })
    }

    /// Gets the FUSE opcode for this request.
    pub fn opcode(&self) -> u32 {
        self.header.opcode
    }

    /// Gets the unique identifier of this request.
    pub fn unique(&self) -> u64 {
        self.header.unique
    }

    /// Gets the FUSE node ID of the inode that this request is for.
    pub fn node_id(&self) -> u64 {
        self.header.nodeid
    }

    /// Gets the user ID of the user that issued this request.
    pub fn uid(&self) -> lx::uid_t {
        self.header.uid
    }

    /// Gets the group ID of the user that issued this request.
    pub fn gid(&self) -> lx::gid_t {
        self.header.gid
    }

    /// Gets the process ID of the process that issued this request.
    pub fn pid(&self) -> u32 {
        self.header.pid
    }

    /// Gets the operation that this request should perform.
    pub fn operation(&self) -> &FuseOperation {
        &self.operation
    }

    /// Log the request.
    pub fn log(&self) {
        tracing::trace!(
            unique = self.unique(),
            node_id = self.node_id(),
            uid = self.uid(),
            gid = self.gid(),
            pid = self.pid(),
            operation = ?self.operation,
            "Request",
        );
    }

    fn read_operation(header: &fuse_in_header, reader: impl RequestReader) -> FuseOperation {
        if header.len as usize > reader.remaining_len() + size_of_val(header) {
            tracing::error!(
                opcode = header.opcode,
                unique = header.unique,
                header_len = header.len,
                len = reader.remaining_len() + size_of_val(header),
                "Invalid message length",
            );
            return FuseOperation::Invalid;
        }

        match FuseOperation::read(header.opcode, reader) {
            Ok(operation) => operation,
            Err(e) => {
                tracing::error!(
                    opcode = header.opcode,
                    unique = header.unique,
                    error = &e as &dyn std::error::Error,
                    "Invalid message payload",
                );
                FuseOperation::Error(e)
            }
        }
    }
}

/// Helpers to parse FUSE messages.
pub trait RequestReader: io::Read {
    /// Read until a matching byte is found.
    ///
    /// This should advance the read position beyond the matching byte, and return the data up to
    /// (but not including) the matching byte.
    ///
    /// This is used to read NULL-terminated strings.
    fn read_until(&mut self, byte: u8) -> lx::Result<Vec<u8>>;

    /// Gets the remaining, unread length of the input data.
    fn remaining_len(&self) -> usize;

    /// Consume the next `count` bytes.
    fn read_count(&mut self, count: usize) -> lx::Result<Box<[u8]>> {
        let mut buffer = vec![0u8; count];
        self.read_exact(&mut buffer)?;
        Ok(buffer.into_boxed_slice())
    }

    /// Read all the remaining data.
    fn read_all(&mut self) -> lx::Result<Box<[u8]>> {
        self.read_count(self.remaining_len())
    }

    /// Read a struct of type `T`.
    fn read_type<T: IntoBytes + FromBytes + Immutable + KnownLayout>(&mut self) -> lx::Result<T> {
        let mut value: T = T::new_zeroed();
        self.read_exact(value.as_mut_bytes())?;
        Ok(value)
    }

    /// Read a NULL-terminated string
    fn string(&mut self) -> lx::Result<lx::LxString> {
        let buffer = self.read_until(b'\0')?;
        Ok(lx::LxString::from_vec(buffer))
    }

    /// Maximum length of a file name component (NAME_MAX on Linux).
    const NAME_MAX: usize = 255;

    /// Read a NULL-terminated string and ensure it's a valid path name component.
    fn name(&mut self) -> lx::Result<lx::LxString> {
        let name = self.string()?;
        if name.is_empty() || name == "." || name == ".." || name.as_bytes().contains(&b'/') {
            return Err(lx::Error::EINVAL);
        }
        if name.len() > Self::NAME_MAX {
            return Err(lx::Error::ENAMETOOLONG);
        }

        Ok(name)
    }
}

impl RequestReader for &[u8] {
    fn read_until(&mut self, byte: u8) -> lx::Result<Vec<u8>> {
        let length = self
            .iter()
            .position(|&c| c == byte)
            .ok_or(lx::Error::EINVAL)?;

        let result = Vec::from(&self[..length]);
        *self = &self[length + 1..];
        Ok(result)
    }

    fn remaining_len(&self) -> usize {
        self.len()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn parse_init() {
        let request = Request::new(FUSE_INIT_REQUEST).unwrap();
        check_header(&request, 1, FUSE_INIT, 0);
        if let FuseOperation::Init { arg } = request.operation {
            assert_eq!(arg.major, 7);
            assert_eq!(arg.minor, 27);
            assert_eq!(arg.max_readahead, 131072);
            assert_eq!(arg.flags, 0x3FFFFB);
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_get_attr() {
        let request = Request::new(FUSE_GETATTR_REQUEST).unwrap();
        check_header(&request, 2, FUSE_GETATTR, 1);
        if let FuseOperation::GetAttr { arg } = request.operation {
            assert_eq!(arg.fh, 0);
            assert_eq!(arg.getattr_flags, 0);
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_statx() {
        let request = Request::new(FUSE_STATX_REQUEST).unwrap();
        check_header(&request, 2, FUSE_STATX, 1);
        if let FuseOperation::StatX { arg } = request.operation {
            assert_eq!(arg.fh, 0);
            assert_eq!(arg.getattr_flags, 0);
            let mask = lx::StatExMask::new()
                .with_file_type(true)
                .with_mode(true)
                .with_nlink(true)
                .with_uid(true)
                .with_gid(true)
                .with_atime(true)
                .with_mtime(true)
                .with_ctime(true)
                .with_ino(true)
                .with_size(true)
                .with_blocks(true)
                .with_btime(true);
            assert_eq!(arg.mask, mask.into_bits());
            let flags = StatxFlags::new().with_dont_sync(true);
            assert_eq!(arg.flags.into_bits(), flags.into_bits());
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_lookup() {
        let request = Request::new(FUSE_LOOKUP_REQUEST).unwrap();
        check_header(&request, 3, FUSE_LOOKUP, 1);
        if let FuseOperation::Lookup { name } = request.operation {
            assert_eq!(name, "hello");
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_open() {
        let request = Request::new(FUSE_OPEN_REQUEST).unwrap();
        check_header(&request, 4, FUSE_OPEN, 2);
        if let FuseOperation::Open { arg } = request.operation {
            assert_eq!(arg.flags, 0x8000);
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_read() {
        let request = Request::new(FUSE_READ_REQUEST).unwrap();
        check_header(&request, 5, FUSE_READ, 2);
        if let FuseOperation::Read { arg } = request.operation {
            assert_eq!(arg.fh, 1);
            assert_eq!(arg.offset, 0);
            assert_eq!(arg.size, 4096);
            assert_eq!(arg.read_flags, 0);
            assert_eq!(arg.lock_owner, 0);
            assert_eq!(arg.flags, 0x8000);
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_flush() {
        let request = Request::new(FUSE_FLUSH_REQUEST).unwrap();
        check_header(&request, 7, FUSE_FLUSH, 2);
        if let FuseOperation::Flush { arg } = request.operation {
            assert_eq!(arg.fh, 1);
            // This was copied from a real fuse request; I have no idea why it sends this number
            // for lock owner especially since locks were not being used.
            assert_eq!(arg.lock_owner, 13021892616250331871);
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_release() {
        let request = Request::new(FUSE_RELEASE_REQUEST).unwrap();
        check_header(&request, 8, FUSE_RELEASE, 2);
        if let FuseOperation::Release { arg } = request.operation {
            assert_eq!(arg.fh, 1);
            assert_eq!(arg.flags, 0x8000);
            assert_eq!(arg.release_flags, 0);
            assert_eq!(arg.lock_owner, 0);
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_opendir() {
        let request = Request::new(FUSE_OPENDIR_REQUEST).unwrap();
        check_header(&request, 9, FUSE_OPENDIR, 1);
        if let FuseOperation::OpenDir { arg } = request.operation {
            assert_eq!(arg.flags, 0x18800);
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_readdir() {
        let request = Request::new(FUSE_READDIR_REQUEST).unwrap();
        check_header(&request, 11, FUSE_READDIR, 1);
        if let FuseOperation::ReadDir { arg } = request.operation {
            assert_eq!(arg.fh, 0);
            assert_eq!(arg.offset, 3);
            assert_eq!(arg.size, 4096);
            assert_eq!(arg.read_flags, 0);
            assert_eq!(arg.lock_owner, 0);
            assert_eq!(arg.flags, 0x18800);
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    #[test]
    fn parse_releasedir() {
        let request = Request::new(FUSE_RELEASEDIR_REQUEST).unwrap();
        check_header(&request, 12, FUSE_RELEASEDIR, 1);
        if let FuseOperation::ReleaseDir { arg } = request.operation {
            assert_eq!(arg.fh, 0);
            assert_eq!(arg.flags, 0x18800);
            assert_eq!(arg.release_flags, 0);
            assert_eq!(arg.lock_owner, 0);
        } else {
            panic!("Incorrect operation {:?}", request.operation);
        }
    }

    fn check_header(request: &Request, unique: u64, opcode: u32, ino: u64) {
        assert_eq!(request.unique(), unique);
        assert_eq!(request.opcode(), opcode);
        assert_eq!(request.node_id(), ino);
        assert_eq!(request.uid(), 0);
        assert_eq!(request.gid(), 0);
        assert_eq!(
            request.pid(),
            if opcode == FUSE_INIT || opcode == FUSE_RELEASE || opcode == FUSE_RELEASEDIR {
                0
            } else {
                971
            }
        );
    }

    pub const FUSE_INIT_REQUEST: &[u8] = &[
        56, 0, 0, 0, 26, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 27, 0, 0, 0, 0, 0, 2, 0, 251, 255, 63, 0,
    ];

    pub const FUSE_GETATTR_REQUEST: &[u8] = &[
        56, 0, 0, 0, 3, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 203, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    pub const FUSE_LOOKUP_REQUEST: &[u8] = &[
        46, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 203, 3, 0, 0, 0, 0, 0, 0, 104, 101, 108, 108, 111, 0,
    ];

    const FUSE_OPEN_REQUEST: &[u8] = &[
        48, 0, 0, 0, 14, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 203, 3, 0, 0, 0, 0, 0, 0, 0, 128, 0, 0, 0, 0, 0, 0,
    ];

    const FUSE_READ_REQUEST: &[u8] = &[
        80, 0, 0, 0, 15, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 203, 3, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 0, 0, 0, 0, 0, 0,
    ];

    const FUSE_FLUSH_REQUEST: &[u8] = &[
        64, 0, 0, 0, 25, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 203, 3, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 223, 18,
        226, 110, 87, 14, 183, 180,
    ];

    const FUSE_RELEASE_REQUEST: &[u8] = &[
        64, 0, 0, 0, 18, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0,
    ];

    const FUSE_OPENDIR_REQUEST: &[u8] = &[
        48, 0, 0, 0, 27, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 203, 3, 0, 0, 0, 0, 0, 0, 0, 136, 1, 0, 0, 0, 0, 0,
    ];

    const FUSE_READDIR_REQUEST: &[u8] = &[
        80, 0, 0, 0, 28, 0, 0, 0, 11, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 203, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 16,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 136, 1, 0, 0, 0, 0, 0,
    ];

    const FUSE_RELEASEDIR_REQUEST: &[u8] = &[
        64, 0, 0, 0, 29, 0, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 136, 1, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0,
    ];

    const FUSE_STATX_REQUEST: &[u8] = &[
        64, 0, 0, 0, 52, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 203, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 64, 0,
        0, 255, 15, 0, 0,
    ];

    #[test]
    fn name_too_long_returns_enametoolong() {
        // Create a name with 256 characters (exceeds NAME_MAX of 255) + null terminator
        let mut data = vec![b'a'; 256];
        data.push(0); // null terminator

        let mut reader: &[u8] = &data;
        let result = reader.name();

        assert_eq!(result, Err(lx::Error::ENAMETOOLONG));
    }
}
