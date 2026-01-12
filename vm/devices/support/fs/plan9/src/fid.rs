// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::protocol::*;
use lxutil::LxCreateOptions;
use lxutil::LxFile;
use lxutil::LxVolume;
use lxutil::PathBufExt;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

// Common trait for fids (files and xattrs)
// Default implementation is provided for most functions since not all are needed by all fid types.
pub trait Fid: Send + Sync {
    fn get_attr(&self) -> lx::Result<(Qid, lx::Stat)> {
        Err(lx::Error::EINVAL)
    }

    fn walk(&self, _name: &lx::LxStr) -> lx::Result<Qid> {
        Err(lx::Error::EINVAL)
    }

    fn open(&self, _flags: u32) -> lx::Result<Qid> {
        Err(lx::Error::EINVAL)
    }

    fn create(&self, _name: &lx::LxStr, _flags: u32, _mode: u32, _gid: u32) -> lx::Result<Qid> {
        Err(lx::Error::EINVAL)
    }

    fn read(&self, _offset: u64, _buffer: &mut [u8]) -> lx::Result<u32> {
        Err(lx::Error::EINVAL)
    }

    fn write(&self, _offset: u64, _buffer: &[u8]) -> lx::Result<u32> {
        Err(lx::Error::EINVAL)
    }

    fn read_dir(&self, _offset: u64, _buffer: &mut [u8]) -> lx::Result<u32> {
        Err(lx::Error::EINVAL)
    }

    fn mkdir(&self, _name: &lx::LxStr, _mode: u32, _gid: u32) -> lx::Result<Qid> {
        Err(lx::Error::EINVAL)
    }

    fn unlink_at(&self, _name: &lx::LxStr, _flags: u32) -> lx::Result<()> {
        Err(lx::Error::EINVAL)
    }

    fn fid_clone(&self) -> Arc<dyn Fid>;

    fn clunk(&self) -> lx::Result<()> {
        Ok(())
    }
}

// Contains the mutable state of a File.
pub struct FileState {
    root: Arc<LxVolume>,
    path: PathBuf,
    qid: Qid,
    file: Option<LxFile>,
}

impl FileState {
    // Initializes a file and make sure it exists.
    fn new(root: Arc<LxVolume>) -> lx::Result<FileState> {
        let mut state = FileState {
            root,
            path: PathBuf::new(),
            qid: Default::default(),
            file: None,
        };

        state.validate_exists()?;
        Ok(state)
    }

    // Not a full clone; doesn't copy file open state.
    fn fid_clone(&self) -> FileState {
        FileState {
            root: Arc::clone(&self.root),
            path: self.path.clone(),
            file: None,
            ..*self
        }
    }

    // Checks if the file exists and sets the qid if it does.
    fn validate_exists(&mut self) -> lx::Result<()> {
        let (qid, _) = self.get_attributes()?;
        self.qid = qid;
        Ok(())
    }

    // Open a file.
    fn open(&mut self, flags: u32) -> lx::Result<Qid> {
        self.check_not_open()?;

        let flags = Self::open_flags_to_o_flags(flags) | lx::O_NOFOLLOW;
        let file = self.root.open(&self.path, flags, None)?;
        self.file = Some(file);
        Ok(self.qid)
    }

    // Create a file.
    fn create(
        &mut self,
        name: &lx::LxStr,
        flags: u32,
        options: LxCreateOptions,
    ) -> lx::Result<Qid> {
        self.check_not_open()?;

        let flags = Self::open_flags_to_o_flags(flags) | lx::O_CREAT | lx::O_NOFOLLOW;
        let child_path = self.child_path(name)?;

        let file = self.root.open(&child_path, flags, Some(options))?;
        let (qid, _) = Self::get_file_attributes(&file)?;
        self.path = child_path;
        self.file = Some(file);
        self.qid = qid;
        Ok(qid)
    }

    // Enumerate a directory.
    fn read_dir(&mut self, offset: u64, buffer: &mut [u8]) -> lx::Result<u32> {
        let mut writer = SliceWriter::new_raw(buffer);
        let file = self.file.as_mut().ok_or(lx::Error::EBADF)?;

        let mut has_entry = false;
        let self_qid = self.qid;
        file.read_dir(offset as lx::off_t, |entry| {
            has_entry = true;

            // Windows doesn't report the inode number for . and .., so just use the current file's
            // qid for that.
            let qid = if entry.inode_nr == 0 {
                self_qid
            } else {
                Qid {
                    path: entry.inode_nr,
                    version: 0,
                    qid_type: Self::get_qid_type_from_mode((entry.file_type as u32) << 12),
                }
            };

            Ok(writer.dir_entry(&entry.name, &qid, entry.offset as u64, entry.file_type))
        })?;

        // If the buffer was too small for even one entry, return an error.
        if has_entry && writer.size() == 0 {
            return Err(lx::Error::EINVAL);
        }

        Ok(writer.size() as u32)
    }

    pub fn read(&self, offset: u64, buffer: &mut [u8]) -> lx::Result<u32> {
        assert!(buffer.len() < u32::MAX as usize);

        let file = self.file.as_ref().ok_or(lx::Error::EBADF)?;
        let size = file.pread(buffer, offset as i64)?;
        Ok(size as u32)
    }

    pub fn write(&self, offset: u64, buffer: &[u8], request_uid: lx::uid_t) -> lx::Result<u32> {
        assert!(buffer.len() < u32::MAX as usize);

        let file = self.file.as_ref().ok_or(lx::Error::EBADF)?;
        let size = file.pwrite(buffer, offset as i64, request_uid)?;
        Ok(size as u32)
    }

    fn child_path(&self, name: &lx::LxStr) -> lx::Result<PathBuf> {
        let mut path = self.path.clone();
        path.push_lx(name)?;
        Ok(path)
    }

    // Convert a Linux mode_t into a qid type.
    fn get_qid_type_from_mode(mode: u32) -> u8 {
        match mode & lx::S_IFMT {
            lx::S_IFLNK => QID_TYPE_SYMLINK,
            lx::S_IFDIR => QID_TYPE_DIRECTORY,
            _ => QID_TYPE_FILE,
        }
    }

    fn stat_to_qid(stat: &lx::Stat) -> Qid {
        Qid {
            path: stat.inode_nr,
            version: 0,
            qid_type: Self::get_qid_type_from_mode(stat.mode),
        }
    }

    // Convert the Tlopen flags to Linux open flags.
    fn open_flags_to_o_flags(flags: u32) -> i32 {
        let mut result = (flags & !OPEN_FLAG_DIRECTORY) as i32;

        // O_DIRECTORY may not match OPEN_FLAG_DIRECTORY depending on the architecture.
        if flags & OPEN_FLAG_DIRECTORY != 0 {
            result |= lx::O_DIRECTORY;
        }

        result
    }

    // Return an error if the file is already open.
    fn check_not_open(&self) -> lx::Result<()> {
        if self.file.is_some() {
            return Err(lx::Error::EINVAL);
        }

        Ok(())
    }

    // Determine file attributes based on the stored path.
    fn get_attributes(&self) -> lx::Result<(Qid, lx::Stat)> {
        let stat = if let Some(file) = self.file.as_ref() {
            file.fstat()?
        } else {
            self.root.lstat(&self.path)?
        };

        Ok((Self::stat_to_qid(&stat), stat))
    }

    fn get_file_attributes(file: &LxFile) -> lx::Result<(Qid, lx::Stat)> {
        let stat = file.fstat()?;
        Ok((Self::stat_to_qid(&stat), stat))
    }
}

// Fid that represents a file on the server.
pub struct File {
    uid: u32,
    state: RwLock<FileState>,
}

impl File {
    // Creates a file and makes sure it exists.
    pub fn new(root: Arc<LxVolume>, uid: u32) -> lx::Result<(File, Qid)> {
        let state = FileState::new(root)?;
        let qid = state.qid;
        let file = File {
            uid,
            state: RwLock::new(state),
        };

        Ok((file, qid))
    }
}

impl Fid for File {
    // Get the file's attributes.
    fn get_attr(&self) -> lx::Result<(Qid, lx::Stat)> {
        self.state.read().get_attributes()
    }

    // Walk to a child, and make sure it exists.
    fn walk(&self, name: &lx::LxStr) -> lx::Result<Qid> {
        let mut state = self.state.write();
        state.path.push_lx(name)?;
        state.validate_exists()?;
        Ok(state.qid)
    }

    // Create a clone of everything except the open file state.
    fn fid_clone(&self) -> Arc<dyn Fid> {
        let state = self.state.read().fid_clone();
        let clone = File {
            state: RwLock::new(state),
            ..*self
        };

        Arc::new(clone)
    }

    // Open the file.
    fn open(&self, flags: u32) -> lx::Result<Qid> {
        self.state.write().open(flags)
    }

    // Create a new file.
    fn create(&self, name: &lx::LxStr, flags: u32, mode: u32, gid: u32) -> lx::Result<Qid> {
        // On Unix, the specified gid, as well as the uid from Tattach, are currently ignored. All
        // operations are done as the user that's running OpenVMM.
        self.state
            .write()
            .create(name, flags, LxCreateOptions::new(mode, self.uid, gid))
    }

    // Read from the file.
    fn read(&self, offset: u64, buffer: &mut [u8]) -> lx::Result<u32> {
        let state = self.state.read();
        state.read(offset, buffer)
    }

    // Write to the file.
    fn write(&self, offset: u64, buffer: &[u8]) -> lx::Result<u32> {
        let state = self.state.read();
        state.write(offset, buffer, self.uid)
    }

    // Read directory contents.
    fn read_dir(&self, offset: u64, buffer: &mut [u8]) -> lx::Result<u32> {
        let mut state = self.state.write();
        state.read_dir(offset, buffer)
    }

    // Create a directory.
    fn mkdir(&self, name: &lx::LxStr, mode: u32, gid: u32) -> lx::Result<Qid> {
        // On Unix, the specified gid, as well as the uid from Tattach, are currently ignored. All
        // operations are done as the user that's running OpenVMM.
        let state = self.state.read();
        let child_path = state.child_path(name)?;
        let stat = state
            .root
            .mkdir_stat(child_path, LxCreateOptions::new(mode, self.uid, gid))?;

        Ok(FileState::stat_to_qid(&stat))
    }

    // Remove a file or directory.
    fn unlink_at(&self, name: &lx::LxStr, flags: u32) -> lx::Result<()> {
        let state = self.state.read();
        let child_path = state.child_path(name)?;
        state.root.unlink(child_path, flags as i32)
    }
}
