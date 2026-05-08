// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Fuse;
use super::Mapper;
use super::protocol::*;
use super::reply::ReplySender;
use super::request::FuseOperation;
use super::request::Request;
use super::request::RequestReader;
use parking_lot::RwLock;
use std::io;
use std::sync::atomic;
use thiserror::Error;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

// These are the flags that libfuse enables by default when calling init.
const DEFAULT_FLAGS: u32 = FUSE_ASYNC_READ
    | FUSE_PARALLEL_DIROPS
    | FUSE_AUTO_INVAL_DATA
    | FUSE_HANDLE_KILLPRIV
    | FUSE_ASYNC_DIO
    | FUSE_ATOMIC_O_TRUNC
    | FUSE_BIG_WRITES
    | FUSE_INIT_EXT;

// Default flags2 to negotiate when FUSE_INIT_EXT is supported.
// Individual filesystem implementations can set additional flags2 in their
// init callback (e.g. FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2 for virtiofs).
const DEFAULT_FLAGS2: u32 = 0;

const DEFAULT_MAX_PAGES: u32 = 256;

// Page size is currently hardcoded. While it could be determined from the OS, in the case of
// virtio-fs it's not clear whether the host's or guest's page size should be used, if there's
// a difference.
const PAGE_SIZE: u32 = 4096;

/// A FUSE session for a file system.
///
/// Handles negotiation and dispatching requests to the file system.
pub struct Session {
    fs: Box<dyn Fuse + Send + Sync>,
    // Initialized provides a quick way to check if FUSE_INIT is expected without having to take
    // a lock, since operations mostly don't need to access the SessionInfo.
    initialized: atomic::AtomicBool,
    info: RwLock<SessionInfo>,
}

impl Session {
    /// Create a new `Session`.
    pub fn new<T>(fs: T) -> Self
    where
        T: 'static + Fuse + Send + Sync,
    {
        Self {
            fs: Box::new(fs),
            initialized: atomic::AtomicBool::new(false),
            info: RwLock::new(SessionInfo::default()),
        }
    }

    /// Indicates whether the session has received an init request.
    ///
    /// Also returns `false` after the session received a destroy request.
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(atomic::Ordering::Acquire)
    }

    /// Dispatch a FUSE request to the file system.
    pub fn dispatch(
        &self,
        request: Request,
        sender: &mut impl ReplySender,
        mapper: Option<&dyn Mapper>,
    ) {
        let unique = request.unique();
        let result = if self.is_initialized() {
            self.dispatch_helper(request, sender, mapper)
        } else {
            self.dispatch_init(request, sender)
        };

        match result {
            Err(OperationError::FsError(e)) => {
                if let Err(e) = sender.send_error(unique, e.value()) {
                    tracing::error!(
                        unique,
                        error = &e as &dyn std::error::Error,
                        "Failed to send reply",
                    );
                }
            }
            Err(OperationError::SendError(e)) => {
                if e.kind() == io::ErrorKind::NotFound {
                    tracing::trace!(unique, "Request was interrupted.");
                } else {
                    tracing::error!(
                        unique,
                        error = &e as &dyn std::error::Error,
                        "Failed to send reply",
                    );
                }
            }
            Ok(_) => (),
        }
    }

    /// End the session.
    ///
    /// This puts the session in a state where it can accept another FUSE_INIT message. This allows
    /// a virtiofs file system to be remounted after unmount.
    ///
    /// This invokes the file system's destroy callback if it hadn't been called already.
    pub fn destroy(&self) {
        if self.initialized.swap(false, atomic::Ordering::AcqRel) {
            self.fs.destroy();
        }
    }

    /// Perform the actual dispatch. This allows the caller to send an error reply if any operation
    /// encounters an error.
    fn dispatch_helper(
        &self,
        request: Request,
        sender: &mut impl ReplySender,
        mapper: Option<&dyn Mapper>,
    ) -> Result<(), OperationError> {
        request.log();

        match request.operation() {
            FuseOperation::Invalid => {
                // This indicates the header could be parsed but the rest of the request could not,
                // so send an error reply.
                return Err(lx::Error::EIO.into());
            }
            FuseOperation::Error(e) => {
                // This indicates the request was parsed but contained invalid data (e.g., a name
                // that was too long). Return the specific error.
                return Err((*e).into());
            }
            FuseOperation::Lookup { name } => {
                let out = self.fs.lookup(&request, name)?;
                sender.send_arg(request.unique(), out)?;
            }
            FuseOperation::Forget { arg } => {
                self.fs.forget(request.node_id(), arg.nlookup);
            }
            FuseOperation::GetAttr { arg } => {
                let out = self.fs.get_attr(&request, arg.getattr_flags, arg.fh)?;
                sender.send_arg(request.unique(), out)?;
            }
            FuseOperation::SetAttr { arg } => {
                let out = self.fs.set_attr(&request, arg)?;
                sender.send_arg(request.unique(), out)?;
            }
            FuseOperation::ReadLink {} => {
                let out = self.fs.read_link(&request)?;
                sender.send_string(request.unique(), out)?;
            }
            FuseOperation::Symlink { name, target } => {
                let out = self.fs.symlink(&request, name, target)?;
                sender.send_arg(request.unique(), out)?;
            }
            FuseOperation::MkNod { arg, name } => {
                let out = self.fs.mknod(&request, name, arg)?;
                sender.send_arg(request.unique(), out)?;
            }
            FuseOperation::MkDir { arg, name } => {
                let out = self.fs.mkdir(&request, name, arg)?;
                sender.send_arg(request.unique(), out)?;
            }
            FuseOperation::Unlink { name } => {
                self.fs.unlink(&request, name)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::RmDir { name } => {
                self.fs.rmdir(&request, name)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::Rename {
                arg,
                name,
                new_name,
            } => {
                self.fs.rename(&request, name, arg.newdir, new_name, 0)?;

                sender.send_empty(request.unique())?;
            }
            FuseOperation::Link { arg, name } => {
                let out = self.fs.link(&request, name, arg.oldnodeid)?;
                sender.send_arg(request.unique(), out)?;
            }
            FuseOperation::Open { arg } => {
                let out = self.fs.open(&request, arg.flags)?;
                self.send_release_if_interrupted(&request, sender, out.fh, arg.flags, out, false)?;
            }
            FuseOperation::Read { arg } => {
                let out = self.fs.read(&request, arg)?;
                Self::send_max_size(sender, request.unique(), &out, arg.size)?;
            }
            FuseOperation::Write { arg, data } => {
                let out = self.fs.write(&request, arg, data)?;
                sender.send_arg(
                    request.unique(),
                    fuse_write_out {
                        size: out.try_into().unwrap(),
                        padding: 0,
                    },
                )?;
            }
            FuseOperation::StatFs {} => {
                let out = self.fs.statfs(&request)?;
                sender.send_arg(request.unique(), fuse_statfs_out { st: out })?;
            }
            FuseOperation::Release { arg } => {
                self.fs.release(&request, arg)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::FSync { arg } => {
                self.fs.fsync(&request, arg.fh, arg.fsync_flags)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::SetXAttr { arg, name, value } => {
                self.fs.set_xattr(&request, name, value, arg.flags)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::GetXAttr { arg, name } => {
                if arg.size == 0 {
                    let out = self.fs.get_xattr_size(&request, name)?;
                    sender.send_arg(
                        request.unique(),
                        fuse_getxattr_out {
                            size: out,
                            padding: 0,
                        },
                    )?;
                } else {
                    let out = self.fs.get_xattr(&request, name, arg.size)?;
                    Self::send_max_size(sender, request.unique(), &out, arg.size)?;
                }
            }
            FuseOperation::ListXAttr { arg } => {
                if arg.size == 0 {
                    let out = self.fs.list_xattr_size(&request)?;
                    sender.send_arg(
                        request.unique(),
                        fuse_getxattr_out {
                            size: out,
                            padding: 0,
                        },
                    )?;
                } else {
                    let out = self.fs.list_xattr(&request, arg.size)?;
                    Self::send_max_size(sender, request.unique(), &out, arg.size)?;
                }
            }
            FuseOperation::RemoveXAttr { name } => {
                self.fs.remove_xattr(&request, name)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::Flush { arg } => {
                self.fs.flush(&request, arg)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::Init { arg: _ } => {
                tracing::warn!("Duplicate init message.");
                return Err(lx::Error::EIO.into());
            }
            FuseOperation::OpenDir { arg } => {
                let out = self.fs.open_dir(&request, arg.flags)?;
                self.send_release_if_interrupted(&request, sender, out.fh, arg.flags, out, true)?;
            }
            FuseOperation::ReadDir { arg } => {
                let out = self.fs.read_dir(&request, arg)?;
                Self::send_max_size(sender, request.unique(), &out, arg.size)?;
            }
            FuseOperation::ReleaseDir { arg } => {
                self.fs.release_dir(&request, arg)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::FSyncDir { arg } => {
                self.fs.fsync_dir(&request, arg.fh, arg.fsync_flags)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::GetLock { arg } => {
                let out = self.fs.get_lock(&request, arg)?;
                sender.send_arg(request.unique(), out)?;
            }
            FuseOperation::SetLock { arg } => {
                self.fs.set_lock(&request, arg, false)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::SetLockSleep { arg } => {
                self.fs.set_lock(&request, arg, true)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::Access { arg } => {
                self.fs.access(&request, arg.mask)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::Create { arg, name } => {
                let out = self.fs.create(&request, name, arg)?;
                self.send_release_if_interrupted(
                    &request,
                    sender,
                    out.open.fh,
                    arg.flags,
                    out,
                    false,
                )?;
            }
            FuseOperation::Interrupt { arg: _ } => {
                // Interrupt is potentially complicated, and none of the sample file systems seem
                // to use it, so it's left as TODO for now.
                tracing::warn!("FUSE_INTERRUPT not supported.");
                return Err(lx::Error::ENOSYS.into());
            }
            FuseOperation::BMap { arg } => {
                let out = self.fs.block_map(&request, arg.block, arg.blocksize)?;
                sender.send_arg(request.unique(), fuse_bmap_out { block: out })?;
            }
            FuseOperation::Destroy {} => {
                if let Some(mapper) = mapper {
                    mapper.clear();
                }
                self.destroy();
                sender.send_empty(request.unique())?;
            }
            FuseOperation::Ioctl { arg, data } => {
                let out = self.fs.ioctl(&request, arg, data)?;
                if out.1.len() > arg.out_size as usize {
                    return Err(lx::Error::EINVAL.into());
                }

                // As far as I can tell, the fields other than result are only used for CUSE.
                sender.send_arg_data(
                    request.unique(),
                    fuse_ioctl_out {
                        result: out.0,
                        flags: 0,
                        in_iovs: 0,
                        out_iovs: 0,
                    },
                    data,
                )?;
            }
            FuseOperation::Poll { arg: _ } => {
                // Poll is not currently needed, and complicated to support. It appears to have some
                // way of registering for later notifications, but I can't figure out how that
                // works without libfuse source.
                tracing::warn!("FUSE_POLL not supported.");
                return Err(lx::Error::ENOSYS.into());
            }
            FuseOperation::NotifyReply { arg: _, data: _ } => {
                // Not sure what this is. It has something to do with poll, I think.
                tracing::warn!("FUSE_NOTIFY_REPLY not supported.");
                return Err(lx::Error::ENOSYS.into());
            }
            FuseOperation::BatchForget { arg, nodes } => {
                self.batch_forget(arg.count, nodes);
            }
            FuseOperation::FAllocate { arg } => {
                self.fs.fallocate(&request, arg)?;
                sender.send_empty(request.unique())?;
            }
            FuseOperation::ReadDirPlus { arg } => {
                let out = self.fs.read_dir_plus(&request, arg)?;
                Self::send_max_size(sender, request.unique(), &out, arg.size)?;
            }
            FuseOperation::Rename2 {
                arg,
                name,
                new_name,
            } => {
                self.fs
                    .rename(&request, name, arg.newdir, new_name, arg.flags)?;

                sender.send_empty(request.unique())?;
            }
            FuseOperation::LSeek { arg } => {
                let out = self.fs.lseek(&request, arg.fh, arg.offset, arg.whence)?;
                sender.send_arg(request.unique(), fuse_lseek_out { offset: out })?;
            }
            FuseOperation::CopyFileRange { arg } => {
                let out = self.fs.copy_file_range(&request, arg)?;
                sender.send_arg(
                    request.unique(),
                    fuse_write_out {
                        size: out.try_into().unwrap(),
                        padding: 0,
                    },
                )?;
            }
            FuseOperation::SetupMapping { arg } => {
                if let Some(mapper) = mapper {
                    self.fs.setup_mapping(&request, mapper, arg)?;
                    sender.send_empty(request.unique())?;
                } else {
                    return Err(lx::Error::ENOSYS.into());
                }
            }
            FuseOperation::RemoveMapping { arg, mappings } => {
                if let Some(mapper) = mapper {
                    self.remove_mapping(&request, mapper, arg.count, mappings)?;
                    sender.send_empty(request.unique())?;
                } else {
                    return Err(lx::Error::ENOSYS.into());
                }
            }
            FuseOperation::SyncFs { _arg } => {
                // Rely on host file system to sync data
                sender.send_empty(request.unique())?;
            }
            FuseOperation::StatX { arg } => {
                let out = self.fs.get_statx(
                    &request,
                    arg.fh,
                    arg.getattr_flags,
                    arg.flags,
                    arg.mask.into(),
                )?;
                sender.send_arg(request.unique(), out)?;
            }
            FuseOperation::CanonicalPath {} => {
                // Android-specific opcode used to return a guest accessible
                // path to the file location being proxied by the fuse
                // implementation.
                tracing::trace!("Unsupported opcode FUSE_CANONICAL_PATH");
                sender.send_error(request.unique(), lx::Error::ENOSYS.value())?;
            }
        }

        Ok(())
    }

    /// Dispatch the init message.
    fn dispatch_init(
        &self,
        request: Request,
        sender: &mut impl ReplySender,
    ) -> Result<(), OperationError> {
        request.log();
        let init: &fuse_init_in = if let FuseOperation::Init { arg } = request.operation() {
            arg
        } else {
            tracing::error!(opcode = request.opcode(), "Expected FUSE_INIT");
            return Err(lx::Error::EIO.into());
        };

        let mut info = self.info.write();
        if self.is_initialized() {
            tracing::error!("Racy FUSE_INIT requests.");
            return Err(lx::Error::EIO.into());
        }

        let mut out = fuse_init_out::new_zeroed();
        out.major = FUSE_KERNEL_VERSION;
        out.minor = FUSE_KERNEL_MINOR_VERSION;

        // According to the docs, if the kernel reports a higher version, the response should have
        // only the desired version set and the kernel will resend FUSE_INIT with that version.
        if init.major > FUSE_KERNEL_VERSION {
            sender.send_arg(request.unique(), out)?;
            return Ok(());
        }

        // Don't bother supporting old versions. Version 7.27 is what kernel 4.19 uses, and can
        // be supported without needing to change the daemon's behavior for compatibility.
        if init.major < FUSE_KERNEL_VERSION || init.minor < 27 {
            tracing::error!(
                major = init.major,
                minor = init.minor,
                "Got unsupported kernel version",
            );
            return Err(lx::Error::EIO.into());
        }

        // Prepare the session info and call the file system to negotiate.
        info.major = init.major;
        info.minor = init.minor;
        info.max_readahead = init.max_readahead;
        info.capable = init.flags;
        info.want = DEFAULT_FLAGS & init.flags;
        info.want2 = 0;
        info.capable2 = 0;
        // Negotiate flags2 when the kernel supports extended init.
        if init.flags & FUSE_INIT_EXT != 0 {
            info.capable2 = init.flags2;
            info.want2 = DEFAULT_FLAGS2 & init.flags2;
        }
        info.time_gran = 1;
        info.max_write = DEFAULT_MAX_PAGES * PAGE_SIZE;
        self.fs.init(&mut info);

        assert!(info.want & !info.capable == 0);
        // If the filesystem cleared FUSE_INIT_EXT from want, force want2 to
        // zero so we never reply with flags2 the kernel won't expect.
        if info.want & FUSE_INIT_EXT == 0 {
            info.want2 = 0;
        }
        assert!(info.want2 & !info.capable2 == 0);

        // Report the negotiated values back to the client.
        // TODO: Set map_alignment for DAX.
        out.max_readahead = info.max_readahead;
        out.flags = info.want;
        out.max_background = info.max_background;
        out.congestion_threshold = info.congestion_threshold;
        out.max_write = info.max_write;
        out.time_gran = info.time_gran;
        out.max_pages = ((info.max_write - 1) / PAGE_SIZE - 1).try_into().unwrap();
        // Only report flags2 when extended init was negotiated.
        if info.want & FUSE_INIT_EXT != 0 {
            out.flags2 = info.want2;
        }

        sender.send_arg(request.unique(), out)?;

        // Indicate other requests can be received now.
        self.initialized.store(true, atomic::Ordering::Release);
        Ok(())
    }

    /// Send a reply and call the release method if the reply was interrupted.
    fn send_release_if_interrupted<
        TArg: zerocopy::IntoBytes + std::fmt::Debug + Immutable + KnownLayout,
    >(
        &self,
        request: &Request,
        sender: &mut impl ReplySender,
        fh: u64,
        flags: u32,
        arg: TArg,
        dir: bool,
    ) -> lx::Result<()> {
        if let Err(e) = sender.send_arg(request.unique(), arg) {
            // ENOENT means the request was interrupted, and the kernel will not call
            // release, so do it now.
            if e.kind() == io::ErrorKind::NotFound {
                let arg = fuse_release_in {
                    fh,
                    flags,
                    release_flags: 0,
                    lock_owner: 0,
                };

                if dir {
                    self.fs.release_dir(request, &arg)?;
                } else {
                    self.fs.release(request, &arg)?;
                }
            } else {
                return Err(e.into());
            }
        }

        Ok(())
    }

    /// Send a reply, validating it doesn't exceed the requested size.
    ///
    /// If it exceeds the maximum size, this causes a panic because that's a bug in the file system.
    fn send_max_size(
        sender: &mut impl ReplySender,
        unique: u64,
        data: &[u8],
        max_size: u32,
    ) -> Result<(), OperationError> {
        assert!(data.len() <= max_size as usize);
        sender.send_data(unique, data)?;
        Ok(())
    }

    /// Process `FUSE_BATCH_FORGET` by repeatedly calling `forget`.
    fn batch_forget(&self, count: u32, mut nodes: &[u8]) {
        for _ in 0..count {
            let forget: fuse_forget_one = match nodes.read_type() {
                Ok(f) => f,
                Err(_) => break,
            };

            self.fs.forget(forget.nodeid, forget.nlookup);
        }
    }

    /// Remove multiple DAX mappings.
    fn remove_mapping(
        &self,
        request: &Request,
        mapper: &dyn Mapper,
        count: u32,
        mut mappings: &[u8],
    ) -> lx::Result<()> {
        for _ in 0..count {
            let mapping: fuse_removemapping_one = mappings.read_type()?;
            self.fs
                .remove_mapping(request, mapper, mapping.moffset, mapping.len)?;
        }

        Ok(())
    }
}

/// Provides information about a session. Public fields may be modified during `init`.
#[derive(Default)]
pub struct SessionInfo {
    major: u32,
    minor: u32,
    pub max_readahead: u32,
    capable: u32,
    capable2: u32,
    pub want: u32,
    /// Extended flags (flags2) to negotiate when FUSE_INIT_EXT is active.
    pub want2: u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
}

impl SessionInfo {
    pub fn major(&self) -> u32 {
        self.major
    }

    pub fn minor(&self) -> u32 {
        self.minor
    }

    pub fn capable(&self) -> u32 {
        self.capable
    }

    pub fn capable2(&self) -> u32 {
        self.capable2
    }
}

#[derive(Debug, Error)]
enum OperationError {
    #[error("File system error")]
    FsError(#[from] lx::Error),
    #[error("Send error")]
    SendError(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::tests::*;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use zerocopy::FromBytes;
    use zerocopy::IntoBytes;

    #[test]
    fn dispatch_error_name_too_long() {
        let fs = TestFs::default();
        let session = Session::new(fs);

        // Initialize the session first
        let mut init_sender = MockSender::default();
        session.dispatch(
            Request::new(FUSE_INIT_REQUEST).unwrap(),
            &mut init_sender,
            None,
        );
        assert!(session.is_initialized());

        // Create a LOOKUP request with a name that's too long (256 bytes, exceeds NAME_MAX of 255)
        let mut error_sender = ErrorCheckingSender::default();
        let lookup_data = make_lookup_name_too_long();
        let request = Request::new(lookup_data.as_slice()).unwrap();

        // Verify the operation is Error(ENAMETOOLONG)
        assert!(
            matches!(request.operation(), FuseOperation::Error(e) if *e == lx::Error::ENAMETOOLONG)
        );

        session.dispatch(request, &mut error_sender, None);

        // Verify that an error reply was sent with ENAMETOOLONG (36)
        assert_eq!(
            error_sender.last_error,
            Some(lx::Error::ENAMETOOLONG.value())
        );
    }

    #[test]
    fn dispatch() {
        let mut sender = MockSender::default();
        let fs = TestFs::default();
        let state = fs.state.clone();
        let session = Session::new(fs);
        assert!(!session.is_initialized());
        let request = Request::new(FUSE_INIT_REQUEST).unwrap();
        session.dispatch(request, &mut sender, None);
        assert_eq!(state.lock().called, INIT_CALLED);
        assert!(session.is_initialized());
        session.dispatch(
            Request::new(FUSE_GETATTR_REQUEST).unwrap(),
            &mut sender,
            None,
        );
        assert_eq!(state.lock().called, INIT_CALLED | GETATTR_CALLED);

        session.dispatch(
            Request::new(FUSE_LOOKUP_REQUEST).unwrap(),
            &mut sender,
            None,
        );
        assert_eq!(
            state.lock().called,
            INIT_CALLED | GETATTR_CALLED | LOOKUP_CALLED
        );
    }

    #[derive(Default)]
    struct State {
        called: u32,
    }

    #[derive(Default)]
    struct TestFs {
        state: Arc<Mutex<State>>,
    }

    impl Fuse for TestFs {
        fn init(&self, info: &mut SessionInfo) {
            assert_eq!(self.state.lock().called & INIT_CALLED, 0);
            assert_eq!(info.major(), 7);
            assert_eq!(info.minor(), 27);
            assert_eq!(info.capable(), 0x3FFFFB);
            assert_eq!(info.want, 0xC9029);
            assert_eq!(info.max_readahead, 131072);
            assert_eq!(info.max_background, 0);
            assert_eq!(info.max_write, 1048576);
            assert_eq!(info.congestion_threshold, 0);
            assert_eq!(info.time_gran, 1);
            self.state.lock().called |= INIT_CALLED;
        }

        fn get_attr(&self, request: &Request, flags: u32, fh: u64) -> lx::Result<fuse_attr_out> {
            assert_eq!(self.state.lock().called & GETATTR_CALLED, 0);
            assert_eq!(request.node_id(), 1);
            assert_eq!(flags, 0);
            assert_eq!(fh, 0);
            let mut attr = fuse_attr_out::new_zeroed();
            attr.attr.ino = 1;
            attr.attr.mode = lx::S_IFDIR | 0o755;
            attr.attr.nlink = 2;
            attr.attr_valid = 1;
            self.state.lock().called |= GETATTR_CALLED;
            Ok(attr)
        }

        fn lookup(&self, request: &Request, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
            assert_eq!(self.state.lock().called & LOOKUP_CALLED, 0);
            assert_eq!(request.node_id(), 1);
            assert_eq!(name, "hello");
            self.state.lock().called |= LOOKUP_CALLED;
            let mut attr = fuse_attr::new_zeroed();
            attr.ino = 2;
            attr.mode = lx::S_IFREG | 0o644;
            attr.nlink = 1;
            attr.size = 13;
            Ok(fuse_entry_out {
                nodeid: 2,
                generation: 0,
                entry_valid: 1,
                entry_valid_nsec: 0,
                attr_valid: 1,
                attr_valid_nsec: 0,
                attr,
            })
        }
    }

    #[derive(Default)]
    struct MockSender {
        state: u32,
    }

    impl ReplySender for MockSender {
        fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
            let flat: Vec<u8> = bufs.iter().flat_map(|s| s.iter()).copied().collect();
            match self.state {
                0 => assert_eq!(flat, INIT_REPLY),
                1 => assert_eq!(flat, GETATTR_REPLY),
                2 => assert_eq!(flat, LOOKUP_REPLY),
                _ => panic!("Unexpected send."),
            }

            self.state += 1;
            Ok(())
        }
    }

    const INIT_CALLED: u32 = 0x1;
    const GETATTR_CALLED: u32 = 0x2;
    const LOOKUP_CALLED: u32 = 0x4;

    const INIT_REPLY: &[u8] = &[
        80, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 39, 0, 0, 0, 0, 0, 2, 0, 41,
        144, 12, 0, 0, 0, 0, 0, 0, 0, 16, 0, 1, 0, 0, 0, 254, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    const GETATTR_REPLY: &[u8] = &[
        120, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 237, 65, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0,
    ];

    const LOOKUP_REPLY: &[u8] = &[
        144, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0,
        0, 0, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 164, 129, 0,
        0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    /// A ReplySender that tracks error responses for testing
    #[derive(Default)]
    struct ErrorCheckingSender {
        last_error: Option<i32>,
    }

    impl ReplySender for ErrorCheckingSender {
        fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
            // Parse the fuse_out_header to check for errors
            let flat: Vec<u8> = bufs.iter().flat_map(|s| s.iter()).copied().collect();
            if flat.len() >= 16 {
                // fuse_out_header: len (4), error (4), unique (8)
                let error = i32::from_ne_bytes([flat[4], flat[5], flat[6], flat[7]]);
                if error != 0 {
                    self.last_error = Some(-error); // Error is stored as negative in header
                }
            }
            Ok(())
        }
    }

    /// A ReplySender that captures the raw response bytes for inspection.
    #[derive(Default)]
    struct CapturingSender {
        data: Vec<u8>,
    }

    impl ReplySender for CapturingSender {
        fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
            self.data = bufs.iter().flat_map(|s| s.iter()).copied().collect();
            Ok(())
        }
    }

    impl CapturingSender {
        /// Parse the captured reply as a fuse_out_header + fuse_init_out.
        fn parse_init_reply(&self) -> (fuse_out_header, fuse_init_out) {
            let hdr = fuse_out_header::read_from_prefix(&self.data).unwrap().0;
            let body = fuse_init_out::read_from_prefix(&self.data[size_of::<fuse_out_header>()..])
                .unwrap()
                .0;
            (hdr, body)
        }
    }

    /// Build a FUSE_INIT request with the given version and flags.
    fn make_init_request(
        major: u32,
        minor: u32,
        max_readahead: u32,
        flags: u32,
        flags2: u32,
    ) -> Vec<u8> {
        let header = fuse_in_header {
            len: (size_of::<fuse_in_header>() + size_of::<fuse_init_in>()) as u32,
            opcode: FUSE_INIT,
            unique: 1,
            nodeid: 0,
            uid: 0,
            gid: 0,
            pid: 0,
            padding: 0,
        };
        let init = fuse_init_in {
            major,
            minor,
            max_readahead,
            flags,
            flags2,
            unused: [0; 11],
        };
        let mut data = Vec::new();
        data.extend_from_slice(header.as_bytes());
        data.extend_from_slice(init.as_bytes());
        data
    }

    /// A minimal Fuse implementation that records the SessionInfo seen during init
    /// and optionally requests FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2.
    #[derive(Default)]
    struct InitCapturingFs {
        info: Arc<Mutex<Option<(u32, u32, u32)>>>, // (want, want2, capable)
        request_direct_io_mmap: bool,
    }

    impl Fuse for InitCapturingFs {
        fn init(&self, info: &mut SessionInfo) {
            if self.request_direct_io_mmap && info.capable2() & FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2 != 0
            {
                info.want2 |= FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2;
            }
            *self.info.lock() = Some((info.want, info.want2, info.capable()));
        }
    }

    #[test]
    fn init_with_ext_negotiates_flags2_and_direct_io_allow_mmap() {
        // Kernel advertises FUSE_INIT_EXT among its capabilities.
        let flags = 0x003FFFFB | FUSE_INIT_EXT;
        let request_data = make_init_request(7, 39, 131072, flags, FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2);

        let fs = InitCapturingFs {
            request_direct_io_mmap: true,
            ..Default::default()
        };
        let info_ref = fs.info.clone();
        let session = Session::new(fs);

        let mut sender = CapturingSender::default();
        session.dispatch(
            Request::new(request_data.as_slice()).unwrap(),
            &mut sender,
            None,
        );

        assert!(session.is_initialized());

        // The filesystem should see FUSE_INIT_EXT in the negotiated flags and
        // FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2 in want2.
        let info = info_ref.lock();
        let &(want, want2, _capable) = info
            .as_ref()
            .expect("filesystem init info should be captured after initialization");
        assert_ne!(
            want & FUSE_INIT_EXT,
            0,
            "FUSE_INIT_EXT should be negotiated"
        );
        assert_ne!(
            want2 & FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
            0,
            "FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2 should be in want2"
        );

        // The reply must carry both flags and flags2.
        let (_hdr, init_out) = sender.parse_init_reply();
        assert_ne!(
            init_out.flags & FUSE_INIT_EXT,
            0,
            "Reply flags must include FUSE_INIT_EXT"
        );
        assert_ne!(
            init_out.flags2 & FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
            0,
            "Reply flags2 must include FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2"
        );
    }

    #[test]
    fn init_without_ext_does_not_negotiate_flags2() {
        // Kernel does NOT advertise FUSE_INIT_EXT.
        let flags = 0x003FFFFB; // same as FUSE_INIT_REQUEST, no FUSE_INIT_EXT
        let request_data = make_init_request(7, 27, 131072, flags, 0);

        let fs = InitCapturingFs::default();
        let info_ref = fs.info.clone();
        let session = Session::new(fs);

        let mut sender = CapturingSender::default();
        session.dispatch(
            Request::new(request_data.as_slice()).unwrap(),
            &mut sender,
            None,
        );

        assert!(session.is_initialized());

        // Without FUSE_INIT_EXT the daemon must not request any flags2.
        let info = info_ref.lock();
        let &(_want, want2, _capable) = info
            .as_ref()
            .expect("filesystem init info should be captured after initialization");
        assert_eq!(want2, 0, "want2 must be zero without FUSE_INIT_EXT");

        let (_hdr, init_out) = sender.parse_init_reply();
        assert_eq!(
            init_out.flags & FUSE_INIT_EXT,
            0,
            "Reply flags must NOT include FUSE_INIT_EXT"
        );
        assert_eq!(init_out.flags2, 0, "Reply flags2 must be zero");
    }

    #[test]
    fn init_ext_without_direct_io_mmap_results_in_zero_flags2() {
        // Kernel supports FUSE_INIT_EXT but does NOT advertise
        // FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2 in flags2.
        let flags = 0x003FFFFB | FUSE_INIT_EXT;
        let request_data = make_init_request(7, 39, 131072, flags, 0);

        let fs = InitCapturingFs::default();
        let info_ref = fs.info.clone();
        let session = Session::new(fs);

        let mut sender = CapturingSender::default();
        session.dispatch(
            Request::new(request_data.as_slice()).unwrap(),
            &mut sender,
            None,
        );

        assert!(session.is_initialized());

        let info = info_ref.lock();
        let &(_want, want2, _capable) = info
            .as_ref()
            .expect("filesystem init info should be captured after initialization");
        assert_eq!(
            want2, 0,
            "want2 must be zero when kernel flags2 lacks FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2"
        );

        let (_hdr, init_out) = sender.parse_init_reply();
        assert_eq!(init_out.flags2, 0, "Reply flags2 must be zero");
    }

    #[test]
    fn init_higher_major_version_replies_with_supported_version() {
        // Kernel reports major version 8, higher than FUSE_KERNEL_VERSION (7).
        let request_data = make_init_request(8, 0, 131072, 0, 0);

        let fs = InitCapturingFs::default();
        let info_ref = fs.info.clone();
        let session = Session::new(fs);

        let mut sender = CapturingSender::default();
        session.dispatch(
            Request::new(request_data.as_slice()).unwrap(),
            &mut sender,
            None,
        );

        // Session should NOT be marked initialized — the kernel will resend INIT.
        assert!(!session.is_initialized());

        // The filesystem's init callback should NOT have been called.
        assert!(info_ref.lock().is_none());

        // Reply should carry the supported version.
        let (hdr, init_out) = sender.parse_init_reply();
        assert_eq!(hdr.error, 0);
        assert_eq!(init_out.major, FUSE_KERNEL_VERSION);
        assert_eq!(init_out.minor, FUSE_KERNEL_MINOR_VERSION);
    }

    #[test]
    fn init_old_unsupported_version_returns_error() {
        // Kernel reports version 7.26, below the minimum (7.27).
        let request_data = make_init_request(7, 26, 131072, 0, 0);

        let fs = InitCapturingFs::default();
        let session = Session::new(fs);

        let mut sender = ErrorCheckingSender::default();
        session.dispatch(
            Request::new(request_data.as_slice()).unwrap(),
            &mut sender,
            None,
        );

        // Session should not be initialized after an unsupported version.
        assert!(!session.is_initialized());

        // An error reply should have been sent.
        assert!(sender.last_error.is_some());
    }

    /// Creates a FUSE_LOOKUP request with a name that's too long (256 bytes, exceeds NAME_MAX of 255)
    fn make_lookup_name_too_long() -> Vec<u8> {
        let mut data = vec![0u8; 297]; // 40 byte header + 256 byte name + 1 null terminator

        // fuse_in_header (40 bytes):
        // len: u32 = 297 (0x129)
        data[0] = 0x29;
        data[1] = 0x01;
        data[2] = 0x00;
        data[3] = 0x00;

        // opcode: u32 = 1 (FUSE_LOOKUP)
        data[4] = 0x01;
        data[5] = 0x00;
        data[6] = 0x00;
        data[7] = 0x00;

        // unique: u64 = 99
        data[8] = 99;
        data[9] = 0x00;
        data[10] = 0x00;
        data[11] = 0x00;
        data[12] = 0x00;
        data[13] = 0x00;
        data[14] = 0x00;
        data[15] = 0x00;

        // nodeid: u64 = 1
        data[16] = 0x01;
        data[17] = 0x00;
        data[18] = 0x00;
        data[19] = 0x00;
        data[20] = 0x00;
        data[21] = 0x00;
        data[22] = 0x00;
        data[23] = 0x00;

        // uid: u32 = 0
        // gid: u32 = 0
        // pid: u32 = 971 (0x3CB)
        data[32] = 0xCB;
        data[33] = 0x03;
        data[34] = 0x00;
        data[35] = 0x00;

        // padding: u32 = 0

        // Name: 256 'a' characters (0x61) starting at byte 40
        for item in data.iter_mut().take(296).skip(40) {
            *item = 0x61; // 'a'
        }
        // Null terminator at byte 296
        data[296] = 0x00;

        data
    }
}
