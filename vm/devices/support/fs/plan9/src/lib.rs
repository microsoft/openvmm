// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![cfg(any(windows, target_os = "linux"))]

mod fid;
mod protocol;

pub use lx::Error;

use fid::*;
use lxutil::LxVolume;
use parking_lot::RwLock;
use protocol::*;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::str;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

const MINIMUM_REQUEST_BUFFER_SIZE: u32 = 4096;
const MAXIMUM_REQUEST_BUFFER_SIZE: u32 = 256 * 1024;

// The maximum size of an IO request (0 means no limit).
const IO_UNIT: u32 = 0;

pub struct Plan9FileSystem {
    negotiated_size: AtomicU32,
    fids: RwLock<HashMap<u32, Arc<dyn Fid>>>,
    root: Arc<LxVolume>,
    debug: bool,
}

impl Plan9FileSystem {
    pub fn new(
        root_path: impl AsRef<Path>,
        debug: bool,
    ) -> Result<Plan9FileSystem, lxutil::VolumeError> {
        let root = Arc::new(LxVolume::new(root_path)?);
        Ok(Plan9FileSystem {
            negotiated_size: AtomicU32::new(0),
            fids: RwLock::new(HashMap::new()),
            root,
            debug,
        })
    }

    /// Reset the filesystem state, clearing all open fids and the
    /// negotiated protocol size. Called on guest-initiated device reset.
    pub fn reset(&self) {
        self.fids.write().clear();
        self.negotiated_size.store(0, Ordering::Relaxed);
    }

    // Process a message received from virtio.
    pub fn process_message(&self, message: &[u8], response: &mut [u8]) -> lx::Result<usize> {
        let mut reader = SliceReader::new(message);
        let header = reader.header()?;

        let mut writer = SliceWriter::new(response);
        if let Err(errno) = self.handle_message(&header, reader, &mut writer) {
            writer.reset();
            writer.u32(errno.value() as u32)?;
            writer.header(MESSAGE_RLERROR, header.tag)?;
        } else {
            writer.header(header.message_type + 1, header.tag)?;
        }

        let size = writer.size();
        if self.debug {
            Self::log_response(&response[..size]);
        }

        Ok(size)
    }

    // Dispatch a message to the correct handle function.
    pub fn handle_message(
        &self,
        header: &Header,
        reader: SliceReader<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let msg = Plan9Message::read(header.message_type, reader)?;
        if self.debug {
            tracing::info!(
                message_type = header.message_type,
                tag = header.tag,
                ?msg,
                "[9P] message",
            );
        }

        match msg {
            Plan9Message::Tstatfs(m) => self.handle_statfs(m, response),
            Plan9Message::Tlopen(m) => self.handle_lopen(m, response),
            Plan9Message::Tlcreate(m) => self.handle_lcreate(m, response),
            Plan9Message::Tsymlink(m) => self.handle_symlink(m, response),
            Plan9Message::Treadlink(m) => self.handle_readlink(m, response),
            Plan9Message::Tgetattr(m) => self.handle_get_attr(m, response),
            // Setattr is not supported but returns success to unblock many scenarios.
            Plan9Message::Tsetattr(_) => self.handle_ignored("Tsetattr"),
            // Linux treats ENOTSUP as no file capabilities, but EINVAL prevents execution.
            Plan9Message::Txattrwalk(_) | Plan9Message::Txattrcreate(_) => Err(Error::ENOTSUP),
            Plan9Message::Treaddir(m) => self.handle_read_dir(m, response),
            Plan9Message::Tfsync(m) => self.lookup_fid(m.fid)?.fsync(m.datasync != 0),
            Plan9Message::Tlink(m) => self
                .lookup_fid(m.dfid)?
                .link(self.lookup_fid(m.fid)?.as_ref(), m.name),
            Plan9Message::Tmkdir(m) => self.handle_mkdir(m, response),
            Plan9Message::Tunlinkat(m) => self.handle_unlinkat(m),
            Plan9Message::Tversion(m) => self.handle_version(m, response),
            Plan9Message::Tattach(m) => self.handle_attach(m, response),
            Plan9Message::Twalk(m) => self.handle_walk(m, response),
            Plan9Message::Tread(m) => self.handle_read(m, response),
            Plan9Message::Twrite(m) => self.handle_write(m, response),
            Plan9Message::Tclunk(m) => self.handle_clunk(m),
            _ => {
                tracing::warn!(message_type = header.message_type, "Unhandled message type");
                Err(Error::ENOTSUP)
            }
        }
    }

    pub fn handle_ignored(&self, msg: &str) -> lx::Result<()> {
        if self.debug {
            tracing::warn!(msg, "[9P] Ignored message");
        }

        Ok(())
    }

    pub fn handle_version(
        &self,
        message: Tversion<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let old_size = self.negotiated_size.load(Ordering::SeqCst);

        if message.msize < MINIMUM_REQUEST_BUFFER_SIZE {
            return Err(Error::ENOTSUP);
        }

        if message.version != PROTOCOL_VERSION {
            return Err(Error::ENOTSUP);
        }

        let negotiated_size = std::cmp::min(message.msize, MAXIMUM_REQUEST_BUFFER_SIZE);

        // Renegotiation is allowed, particularly because this implementation doesn't really use
        // the size for anything since it's virtio only, but still prevent multiple changes at once.
        self.negotiated_size
            .compare_exchange(
                old_size,
                negotiated_size,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map_err(|_| Error::EINVAL)?;

        response.u32(negotiated_size)?;
        response.string(message.version)?;
        Ok(())
    }

    pub fn handle_attach(
        &self,
        message: Tattach<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        // Create the fid for the root.
        let (file, qid) = File::new(Arc::clone(&self.root), message.n_uname)?;
        self.emplace_fid(message.fid, Arc::new(file))?;
        response.qid(&qid)?;
        Ok(())
    }

    pub fn handle_clunk(&self, message: Tclunk<'_>) -> lx::Result<()> {
        match self.remove_fid(message.fid) {
            Some(item) => item.clunk(),
            None => Err(Error::EINVAL),
        }
    }

    pub fn handle_get_attr(
        &self,
        message: Tgetattr<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let file = self.lookup_fid(message.fid)?;
        let (qid, stat) = file.get_attr()?;

        // Mask is just echoed back; all fields are valid.
        response.u64(message.request_mask)?;
        response.qid(&qid)?;
        response.u32(stat.mode)?;
        response.u32(stat.uid)?;
        response.u32(stat.gid)?;
        response.u64(stat.link_count as u64)?;
        response.u64(stat.device_nr_special)?;
        response.u64(stat.file_size)?;
        response.u64(stat.block_size as u64)?;
        response.u64(stat.block_count)?;
        response.timespec(&stat.access_time)?;
        response.timespec(&stat.write_time)?;
        response.timespec(&stat.change_time)?;
        response.u64(0)?; // btime sec (reserved)
        response.u64(0)?; // btime nsec (reserved)
        response.u64(0)?; // gen (reserved)
        response.u64(0)?; // data version (reserved)
        Ok(())
    }

    fn handle_statfs(
        &self,
        message: Tstatfs<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let stat = self.lookup_fid(message.fid)?.stat_fs()?;
        response.u32(stat.fs_type as u32)?;
        response.u32(stat.block_size as u32)?;
        response.u64(stat.block_count)?;
        response.u64(stat.free_block_count)?;
        response.u64(stat.available_block_count)?;
        response.u64(stat.file_count)?;
        response.u64(stat.available_file_count)?;
        response.u64(u64::from_ne_bytes(stat.file_system_id))?;
        response.u32(stat.maximum_file_name_length as u32)?;
        Ok(())
    }

    fn handle_symlink(
        &self,
        message: Tsymlink<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let qid =
            self.lookup_fid(message.fid)?
                .symlink(message.name, message.symtgt, message.gid)?;
        response.qid(&qid)
    }

    fn handle_readlink(
        &self,
        message: Treadlink<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let target = self.lookup_fid(message.fid)?.read_link()?;
        response.string(&target)
    }

    pub fn handle_walk(
        &self,
        message: Twalk<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        // Create a new fid for the walk.
        let item = self.lookup_fid(message.fid)?.fid_clone();

        // Walk each name and write the response.
        response.u16(message.wnames.name_count())?;
        for name in message.wnames {
            let qid = item.walk(name?)?;
            response.qid(&qid)?;
        }

        self.emplace_fid(message.newfid, item)?;
        Ok(())
    }

    pub fn handle_lopen(
        &self,
        message: Tlopen<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let item = self.lookup_fid(message.fid)?;
        let qid = item.open(message.flags)?;
        response.qid(&qid)?;
        response.u32(IO_UNIT)?;
        Ok(())
    }

    pub fn handle_lcreate(
        &self,
        message: Tlcreate<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let item = self.lookup_fid(message.fid)?;
        let qid = item.create(message.name, message.flags, message.mode, message.gid)?;
        response.qid(&qid)?;
        response.u32(IO_UNIT)?;
        Ok(())
    }

    pub fn handle_read(
        &self,
        message: Tread<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let file = self.lookup_fid(message.fid)?;
        let start = size_of::<u32>();
        let end = start + message.count as usize;
        let size = file.read(message.offset, response.peek(start..end)?)?;
        response.u32(size)?;
        response.next(size as usize)?;
        Ok(())
    }

    pub fn handle_write(
        &self,
        mut message: Twrite<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let file = self.lookup_fid(message.fid)?;
        let data = message.reader.read(message.count as usize)?;
        let size = file.write(message.offset, data)?;
        response.u32(size)?;
        Ok(())
    }

    pub fn handle_read_dir(
        &self,
        message: Treaddir<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let file = self.lookup_fid(message.fid)?;
        let start = size_of::<u32>();
        let end = start + message.count as usize;
        let size = file.read_dir(message.offset, response.peek(start..end)?)?;
        response.u32(size)?;
        response.next(size as usize)?;
        Ok(())
    }

    pub fn handle_mkdir(
        &self,
        message: Tmkdir<'_>,
        response: &mut SliceWriter<'_>,
    ) -> lx::Result<()> {
        let dir = self.lookup_fid(message.dfid)?;
        let qid = dir.mkdir(message.name, message.mode, message.gid)?;
        response.qid(&qid)?;
        Ok(())
    }

    pub fn handle_unlinkat(&self, message: Tunlinkat<'_>) -> lx::Result<()> {
        let dir = self.lookup_fid(message.dfid)?;
        dir.unlink_at(message.name, message.flags)?;
        Ok(())
    }

    // Store a new fid. It's an error if the fid already exists.
    fn emplace_fid(&self, fid: u32, item: Arc<dyn Fid>) -> lx::Result<()> {
        let mut fids = self.fids.write();
        match fids.entry(fid) {
            Entry::Occupied(_) => return Err(Error::EINVAL),
            Entry::Vacant(v) => v.insert(item),
        };

        Ok(())
    }

    // Find a fid with the specified number.
    fn lookup_fid(&self, fid: u32) -> lx::Result<Arc<dyn Fid>> {
        let fids = self.fids.read();
        if let Some(item) = fids.get(&fid) {
            return Ok(Arc::clone(item));
        }

        Err(Error::EINVAL)
    }

    // Remove a fid from the collection.
    fn remove_fid(&self, fid: u32) -> Option<Arc<dyn Fid>> {
        let mut fids = self.fids.write();
        fids.remove(&fid)
    }

    fn log_response(response: &[u8]) {
        let mut reader = SliceReader::new(response);
        if let Ok(header) = reader.header() {
            if let Ok(msg) = Plan9Message::read(header.message_type, reader) {
                tracing::info!(
                    message_type = header.message_type,
                    tag = header.tag,
                    ?msg,
                    "[9P] Response",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const TAG: u16 = 0x1234;

    fn request(message_type: u8, fields: impl FnOnce(&mut SliceWriter<'_>)) -> Vec<u8> {
        let mut request = vec![0; 128];
        let mut writer = SliceWriter::new(&mut request);
        fields(&mut writer);
        writer.header(message_type, TAG).unwrap();
        let size = writer.size();
        request.truncate(size);
        request
    }

    fn xattr_requests() -> [Vec<u8>; 3] {
        [(30, "security.capability"), (30, ""), (32, "user.test")].map(|(message_type, name)| {
            let mut request = vec![0; 128];
            let mut writer = SliceWriter::new(&mut request);
            writer.u32(1).unwrap();
            if message_type == 30 {
                writer.u32(2).unwrap();
            }
            writer.string(lx::LxStr::new(name)).unwrap();
            if message_type == 32 {
                writer.u64(64).unwrap();
                writer.u32(0).unwrap();
            }
            writer.header(message_type, TAG).unwrap();
            let size = writer.size();
            request.truncate(size);
            request
        })
    }

    fn assert_error_reply(fs: &Plan9FileSystem, request: &[u8], error: Error) {
        let mut response = [0; 128];
        let size = fs.process_message(request, &mut response).unwrap();
        assert_eq!(size, 11);
        let mut reader = SliceReader::new(&response[..size]);
        let header = reader.header().unwrap();
        assert_eq!(header.size as usize, size);
        assert_eq!(header.message_type, MESSAGE_RLERROR);
        assert_eq!(header.tag, TAG);
        assert_eq!(reader.u32().unwrap(), error.value() as u32);
    }

    #[test]
    fn unsupported_xattrs_return_enotsup() {
        let root = tempfile::tempdir().unwrap();
        let fs = Plan9FileSystem::new(root.path(), false).unwrap();
        for request in xattr_requests() {
            assert_error_reply(&fs, &request, Error::ENOTSUP);
        }
        assert!(fs.fids.read().is_empty());
    }

    #[test]
    fn malformed_xattrs_still_return_einval() {
        let root = tempfile::tempdir().unwrap();
        let fs = Plan9FileSystem::new(root.path(), false).unwrap();
        for request in xattr_requests() {
            for size in 7..request.len() {
                let mut truncated = request[..size].to_vec();
                truncated[..4].copy_from_slice(&(size as u32).to_le_bytes());
                assert_error_reply(&fs, &truncated, Error::EINVAL);
            }
        }
    }

    #[test]
    fn fsync_flags_and_errors() {
        struct SyncFid(Arc<AtomicU32>);
        impl Fid for SyncFid {
            fn fsync(&self, data_only: bool) -> lx::Result<()> {
                self.0.store(u32::from(data_only), Ordering::Relaxed);
                Err(Error::EIO)
            }

            fn fid_clone(&self) -> Arc<dyn Fid> {
                Arc::new(Self(Arc::clone(&self.0)))
            }
        }

        let root = tempfile::tempdir().unwrap();
        let fs = Plan9FileSystem::new(root.path(), false).unwrap();
        let flag = Arc::new(AtomicU32::new(u32::MAX));
        fs.emplace_fid(1, Arc::new(SyncFid(Arc::clone(&flag))))
            .unwrap();
        let (file, _) = File::new(Arc::clone(&fs.root), 0).unwrap();
        fs.emplace_fid(2, Arc::new(file)).unwrap();
        for datasync in [0, 1] {
            let message = |fid| {
                request(50, |writer| {
                    writer.u32(fid).unwrap();
                    writer.u32(datasync).unwrap();
                })
            };
            assert_error_reply(&fs, &message(1), Error::EIO);
            assert_eq!(flag.load(Ordering::Relaxed), datasync);
            assert_error_reply(&fs, &message(2), Error::EBADF);
            assert_error_reply(&fs, &message(3), Error::EINVAL);
        }
    }

    #[test]
    fn hard_link_rejects_cross_volume_fids() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let fs = Plan9FileSystem::new(root.path(), false).unwrap();
        let (dir, _) = File::new(Arc::clone(&fs.root), 0).unwrap();
        let (source, _) = File::new(Arc::new(LxVolume::new(other.path()).unwrap()), 0).unwrap();
        fs.emplace_fid(1, Arc::new(dir)).unwrap();
        fs.emplace_fid(2, Arc::new(source)).unwrap();
        let message = request(70, |writer| {
            writer.u32(1).unwrap();
            writer.u32(2).unwrap();
            writer.string(lx::LxStr::new("link")).unwrap();
        });
        assert_error_reply(&fs, &message, Error::EXDEV);
        assert!(!root.path().join("link").exists());
    }
}
