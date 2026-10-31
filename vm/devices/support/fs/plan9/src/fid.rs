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
    fn stat_fs(&self) -> lx::Result<lx::StatFs> {
        Err(lx::Error::EINVAL)
    }

    fn fsync(&self, _data_only: bool) -> lx::Result<()> {
        Err(lx::Error::EINVAL)
    }

    fn symlink(&self, _name: &lx::LxStr, _target: &lx::LxStr, _gid: u32) -> lx::Result<Qid> {
        Err(lx::Error::EINVAL)
    }

    fn read_link(&self) -> lx::Result<lx::LxString> {
        Err(lx::Error::EINVAL)
    }

    fn file_path(&self) -> lx::Result<(Arc<LxVolume>, PathBuf)> {
        Err(lx::Error::EINVAL)
    }

    fn link(&self, _source: &dyn Fid, _name: &lx::LxStr) -> lx::Result<()> {
        Err(lx::Error::EINVAL)
    }

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

        let options = Self::open_options(flags)?;
        let file = self.root.open(&self.path, options, None)?;
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

        let open_options = Self::open_options(flags)?;
        let open_options = open_options | lx::O_CREAT;
        let child_path = self.child_path(name)?;

        let file = self.root.open(&child_path, open_options, Some(options))?;
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
        if self.qid.qid_type & QID_TYPE_DIRECTORY == 0 {
            return Err(lx::Error::ENOTDIR);
        }
        // Defense in depth: the protocol parser already validates names,
        // but assert here to catch any bypass.
        assert!(!name.is_empty(), "empty child name");
        assert!(!name.as_bytes().contains(&b'/'), "child name contains '/'");
        assert!(name != "." && name != "..", "child name is '.' or '..'");

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

    fn open_options(flags: u32) -> lx::Result<i32> {
        let mut result = match flags & OPEN_FLAG_ACCESS_MASK {
            OPEN_FLAG_RDONLY => lx::O_RDONLY,
            OPEN_FLAG_WRONLY => lx::O_WRONLY,
            OPEN_FLAG_RDWR => lx::O_RDWR,
            OPEN_FLAG_NOACCESS => lx::O_NOACCESS,
            _ => return Err(lx::Error::EINVAL),
        };
        let mut supported = OPEN_FLAG_ACCESS_MASK
            | OPEN_FLAG_LARGEFILE
            | OPEN_FLAG_NOFOLLOW
            | OPEN_FLAG_NOCTTY
            | OPEN_FLAG_CLOEXEC;
        for (source, target) in [
            (OPEN_FLAG_CREAT, lx::O_CREAT),
            (OPEN_FLAG_EXCL, lx::O_EXCL),
            (OPEN_FLAG_TRUNC, lx::O_TRUNC),
            (OPEN_FLAG_APPEND, lx::O_APPEND),
            (OPEN_FLAG_NONBLOCK, lx::O_NONBLOCK),
            (OPEN_FLAG_DSYNC, lx::O_DSYNC),
            (OPEN_FLAG_SYNC, lx::O_SYNC),
            (OPEN_FLAG_DIRECT, lx::O_DIRECT),
            (OPEN_FLAG_DIRECTORY, lx::O_DIRECTORY),
            (OPEN_FLAG_NOATIME, lx::O_NOATIME),
        ] {
            supported |= source;
            if flags & source != 0 {
                result |= target;
            }
        }
        if flags & !supported != 0 {
            return Err(lx::Error::EINVAL);
        }
        Ok(result)
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
            file.fstat()?.into()
        } else {
            self.root.lstat(&self.path)?
        };

        Ok((Self::stat_to_qid(&stat), stat))
    }

    fn get_file_attributes(file: &LxFile) -> lx::Result<(Qid, lx::Stat)> {
        let stat = file.fstat()?.into();
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
    fn stat_fs(&self) -> lx::Result<lx::StatFs> {
        let state = self.state.read();
        if let Some(file) = &state.file {
            file.stat_fs()
        } else {
            state.root.stat_fs(&state.path)
        }
    }

    fn fsync(&self, data_only: bool) -> lx::Result<()> {
        let state = self.state.read();
        state
            .file
            .as_ref()
            .ok_or(lx::Error::EBADF)?
            .fsync(data_only)
    }

    fn symlink(&self, name: &lx::LxStr, target: &lx::LxStr, gid: u32) -> lx::Result<Qid> {
        let state = self.state.read();
        let path = state.child_path(name)?;
        let stat = state.root.symlink_stat(
            path,
            target,
            LxCreateOptions::new(lx::S_IFLNK | 0o777, self.uid, gid),
        )?;
        Ok(FileState::stat_to_qid(&stat))
    }

    fn read_link(&self) -> lx::Result<lx::LxString> {
        let state = self.state.read();
        state.root.read_link(&state.path)
    }

    fn file_path(&self) -> lx::Result<(Arc<LxVolume>, PathBuf)> {
        let state = self.state.read();
        Ok((Arc::clone(&state.root), state.path.clone()))
    }

    fn link(&self, source: &dyn Fid, name: &lx::LxStr) -> lx::Result<()> {
        // Snapshot before locking the destination, which may be the same fid.
        let (root, path) = source.file_path()?;
        let state = self.state.read();
        if !Arc::ptr_eq(&root, &state.root) {
            return Err(lx::Error::EXDEV);
        }
        state.root.link(path, state.child_path(name)?)
    }

    // Get the file's attributes.
    fn get_attr(&self) -> lx::Result<(Qid, lx::Stat)> {
        self.state.read().get_attributes()
    }

    // Walk to a child, and make sure it exists.
    fn walk(&self, name: &lx::LxStr) -> lx::Result<Qid> {
        let mut state = self.state.write();
        let path = state.child_path(name)?;
        let stat = state.root.lstat(&path)?;
        state.path = path;
        state.qid = FileState::stat_to_qid(&stat);
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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn stat_fs_after_unlink() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), b"retained").unwrap();
        let volume = Arc::new(LxVolume::new(tmp.path()).unwrap());
        let (root, _) = File::new(Arc::clone(&volume), 0).unwrap();
        let fid = root.fid_clone();
        fid.walk("file".as_ref()).unwrap();
        let before = fid.stat_fs().unwrap();
        fid.open(lx::O_RDONLY as u32).unwrap();
        root.unlink_at("file".as_ref(), 0).unwrap();
        assert_eq!(volume.lstat("file").unwrap_err(), lx::Error::ENOENT);

        let after = fid.stat_fs().unwrap();
        assert_eq!(after.file_system_id, before.file_system_id);
        assert_eq!(after.fs_type, before.fs_type);
        assert_eq!(after.block_size, before.block_size);
        let mut buffer = [0; 8];
        assert_eq!(fid.read(0, &mut buffer).unwrap(), 8);
        assert_eq!(&buffer, b"retained");
    }

    #[test]
    fn open_flags_use_the_9p_wire_values() {
        for (wire, expected) in [
            (0o40000, lx::O_DIRECT),
            (0o100000, lx::O_RDONLY),
            (0o200000, lx::O_DIRECTORY),
            (0o400000, lx::O_RDONLY),
            (0o4000000, lx::O_SYNC),
            (0o10000, lx::O_DSYNC),
            (0o3, lx::O_NOACCESS),
            (0o400, lx::O_RDONLY),
            (0o2000000, lx::O_RDONLY),
        ] {
            assert_eq!(FileState::open_options(wire).unwrap(), expected);
        }
    }

    #[test]
    fn unsupported_open_flags_do_not_change_files_or_fids() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), b"unchanged").unwrap();
        let volume = Arc::new(LxVolume::new(tmp.path()).unwrap());
        let (root, root_qid) = File::new(Arc::clone(&volume), 0).unwrap();
        let (file, _) = File::new(volume, 0).unwrap();
        file.walk("file".as_ref()).unwrap();
        for unsupported in [0x200000, 0x2000, 0x400000, 1 << 31] {
            let flags = unsupported | OPEN_FLAG_WRONLY | OPEN_FLAG_TRUNC;
            assert_eq!(file.open(flags).unwrap_err(), lx::Error::EINVAL);
            assert!(file.state.read().file.is_none());
            for name in ["file", "new"] {
                assert_eq!(
                    root.create(name.as_ref(), flags, 0o666, 0).unwrap_err(),
                    lx::Error::EINVAL
                );
                let state = root.state.read();
                assert!(state.file.is_none());
                assert!(state.path.as_os_str().is_empty());
                assert_eq!(state.qid.path, root_qid.path);
            }
        }
        assert_eq!(
            std::fs::read(tmp.path().join("file")).unwrap(),
            b"unchanged"
        );
        assert!(!tmp.path().join("new").exists());
    }

    #[test]
    fn non_directory_fids_reject_children() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), b"unchanged").unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        let volume = Arc::new(LxVolume::new(tmp.path()).unwrap());
        volume
            .symlink("link", "dir", LxCreateOptions::default())
            .unwrap();
        for name in ["file", "link"] {
            let (fid, _) = File::new(Arc::clone(&volume), 0).unwrap();
            fid.walk(name.as_ref()).unwrap();
            assert_eq!(fid.walk("child".as_ref()).unwrap_err(), lx::Error::ENOTDIR);
            assert_eq!(
                fid.create("child".as_ref(), 0, 0o600, 0).unwrap_err(),
                lx::Error::ENOTDIR
            );
            assert_eq!(
                fid.mkdir("child".as_ref(), 0o700, 0).unwrap_err(),
                lx::Error::ENOTDIR
            );
            assert_eq!(
                fid.unlink_at("child".as_ref(), 0).unwrap_err(),
                lx::Error::ENOTDIR
            );
        }
        assert_eq!(
            std::fs::read(tmp.path().join("file")).unwrap(),
            b"unchanged"
        );
        assert!(!tmp.path().join("dir/child").exists());
    }

    #[test]
    fn final_symlinks_are_not_opened_or_created() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("target"), b"unchanged").unwrap();
        std::fs::write(tmp.path().join("stale"), b"original").unwrap();
        let volume = Arc::new(LxVolume::new(tmp.path()).unwrap());
        volume
            .symlink("link", "target", LxCreateOptions::default())
            .unwrap();
        let (stale, _) = File::new(Arc::clone(&volume), 0).unwrap();
        assert_eq!(
            stale.walk("stale".as_ref()).unwrap().qid_type,
            QID_TYPE_FILE
        );
        std::fs::rename(tmp.path().join("stale"), tmp.path().join("original")).unwrap();
        volume
            .symlink("stale", "target", LxCreateOptions::default())
            .unwrap();
        let (link, _) = File::new(Arc::clone(&volume), 0).unwrap();
        assert_eq!(
            link.walk("link".as_ref()).unwrap().qid_type,
            QID_TYPE_SYMLINK
        );
        for fid in [&link, &stale] {
            for flags in [
                OPEN_FLAG_RDONLY,
                OPEN_FLAG_WRONLY,
                OPEN_FLAG_RDWR,
                OPEN_FLAG_WRONLY | OPEN_FLAG_TRUNC,
            ] {
                assert_eq!(fid.open(flags).unwrap_err(), lx::Error::ELOOP);
                assert!(fid.state.read().file.is_none());
            }
        }
        let (root, root_qid) = File::new(Arc::clone(&volume), 0).unwrap();
        for name in ["link", "stale"] {
            for flags in [OPEN_FLAG_RDONLY, OPEN_FLAG_WRONLY | OPEN_FLAG_TRUNC] {
                assert_eq!(
                    root.create(name.as_ref(), flags, 0o666, 0).unwrap_err(),
                    lx::Error::ELOOP
                );
                let state = root.state.read();
                assert!(state.file.is_none());
                assert!(state.path.as_os_str().is_empty());
                assert_eq!(state.qid.path, root_qid.path);
                assert_eq!(state.qid.qid_type, root_qid.qid_type);
            }
            assert_eq!(volume.read_link(name).unwrap(), "target");
        }
        let (file, _) = File::new(Arc::clone(&volume), 0).unwrap();
        file.walk("target".as_ref()).unwrap();
        file.open(OPEN_FLAG_RDONLY).unwrap();
        let mut data = [0; 9];
        assert_eq!(file.read(0, &mut data).unwrap(), data.len() as u32);
        assert_eq!(&data, b"unchanged");
        root.create("new".as_ref(), OPEN_FLAG_WRONLY, 0o666, 0)
            .unwrap();
        assert_eq!(root.write(0, b"new").unwrap(), 3);
        assert_eq!(std::fs::read(tmp.path().join("new")).unwrap(), b"new");
        assert_eq!(
            std::fs::read(tmp.path().join("target")).unwrap(),
            b"unchanged"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("original")).unwrap(),
            b"original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_directory_fid_rejects_replacement_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::write(outside.path().join("file"), b"outside").unwrap();
        let volume = Arc::new(LxVolume::new(tmp.path()).unwrap());
        let (fid, _) = File::new(volume, 0).unwrap();
        fid.walk("dir".as_ref()).unwrap();
        std::fs::rename(tmp.path().join("dir"), tmp.path().join("original")).unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("dir")).unwrap();
        assert!(matches!(
            fid.walk("file".as_ref()).unwrap_err(),
            lx::Error::ELOOP | lx::Error::ENOTDIR
        ));
        assert_eq!(
            fid.create("new".as_ref(), OPEN_FLAG_WRONLY, 0o600, 0)
                .unwrap_err(),
            lx::Error::ELOOP
        );
        assert_eq!(
            std::fs::read(outside.path().join("file")).unwrap(),
            b"outside"
        );
        assert!(!outside.path().join("new").exists());
    }
}
