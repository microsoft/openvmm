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
    pub fn new(root_path: &str, debug: bool) -> lx::Result<Plan9FileSystem> {
        let root = Arc::new(LxVolume::new(root_path)?);
        Ok(Plan9FileSystem {
            negotiated_size: AtomicU32::new(0),
            fids: RwLock::new(HashMap::new()),
            root,
            debug,
        })
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
            Plan9Message::Tlopen(m) => self.handle_lopen(m, response),
            Plan9Message::Tlcreate(m) => self.handle_lcreate(m, response),
            Plan9Message::Tgetattr(m) => self.handle_get_attr(m, response),
            // Setattr is not supported but returns success to unblock many scenarios.
            Plan9Message::Tsetattr(_) => self.handle_ignored("Tsetattr"),
            Plan9Message::Treaddir(m) => self.handle_read_dir(m, response),
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
