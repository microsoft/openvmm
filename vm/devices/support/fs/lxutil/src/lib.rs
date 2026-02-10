// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The LxUtil crate provides an API that allows you to write the same file system code on Windows
//! and Linux, using Linux semantics on both platforms (subject to the limitations of the underlying
//! file system).

#![cfg(any(windows, target_os = "linux"))]
#![expect(clippy::field_reassign_with_default)] // protocol code benefits from imperative field assignment

mod path;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;

pub use path::PathBufExt;
pub use path::PathExt;

/// A platform-independent abstraction that allows you to treat an area of the file system as if
/// it has Unix semantics.
///
/// N.B.: all methods take relative paths, but do not attempt to make sure the path does not escape
///       the root of the `LxVolume`, and therefore should not be relied upon for security.
///
/// Use `PathExt` and `PathBufExt` to write cross-platform code that deals only with Unix-style
/// paths.
///
/// # Windows
///
/// Unix behavior is emulated on Windows, specifically targeting the behavior of Linux. The
/// semantics of some calls may differ slightly. In particular:
///
/// - Linux specific attributes (such as a file's mode and owner, and special file types like fifos
///   and device files) are only supported if metadata is enabled in the `LxVolumeOptions`, and
///   the underlying file system supports the required functionality (extended attributes and
///   reparse points). If this is not enabled, emulation of certain behavior like `chmod` is
///   limited.
/// - For files that don't have Linux metadata, `LxVolumeOptions` can be used to influence how
///   values for Linux attributes are created.
/// - The owner of a newly created file is specified in the `LxCreateOptions` passed to the
///   relevant create call.
/// - Linux permissions are not enforced, even if metadata is enabled.
///
/// # Unix
///
/// All calls pass through directly to their libc equivalent. Attributes like mode are always
/// enabled if the file system supports them. `LxVolumeOptions` is entirely ignored, as are the
/// `uid` and `gid` fields of `LxCreateOptions`.
pub struct LxVolume {
    inner: sys::LxVolume,
}

// This top-level implementation exists to ensure the Windows and Unix implementation have the same
// interface.
impl LxVolume {
    /// Creates a new instance of `LxVolume` using the specified root path.
    pub fn new(root_path: impl AsRef<Path>) -> lx::Result<Self> {
        Self::new_with_options(root_path, &LxVolumeOptions::new())
    }

    /// Indicates whether the file IDs (inode numbers) on this file system are stable.
    ///
    /// # Windows
    ///
    /// This is determined by whether or not a file system supports the
    /// `FILE_SUPPORTS_OPEN_BY_FILE_ID` flag. For example, FAT does not have stable inode numbers.
    ///
    /// If a file system doesn't have stable inode numbers, it means a file's inode number can
    /// change when that file is renamed, and the original inode number can be reused by another
    /// file.
    ///
    /// # Unix
    ///
    /// This is a requirement for file systems in Linux, so this is always `true`. Note that IDs
    /// may still conflict if a path traverses a mount point.
    pub fn supports_stable_file_id(&self) -> bool {
        self.inner.supports_stable_file_id()
    }

    /// Retrieves the attributes of a file. Symlinks are not followed.
    pub fn lstat(&self, path: impl AsRef<Path>) -> lx::Result<lx::Stat> {
        self.inner.lstat(path.as_ref()).map(|x| x.into())
    }

    /// Retrieves the statx details of a file. Symlinks are not followed.
    pub fn statx(&self, path: impl AsRef<Path>) -> lx::Result<lx::StatEx> {
        self.inner.lstat(path.as_ref())
    }

    /// Sets the attributes of a file. Symlinks are not followed.
    ///
    /// This function combines the functionality of `truncate`, `chmod`, `chown` and `utimensat`.
    ///
    /// If this function fails, some of the operations may still have succeeded.
    ///
    /// # Windows
    ///
    /// Chmod and chown are only fully supported if metadata is enabled and the file system supports
    /// it. Without metadata, chmod only changes the read-only attribute if all write bits are
    /// removed from the mode, and chown silently succeeds without taking any action.
    ///
    /// This function disables the set-user-ID and set-group-ID as required if a request is made
    /// to change the size, owner or group of a file. This is done based on whether the
    /// `SetAttributes::thread_uid` field indicates the user is root.
    ///
    /// # Unix
    ///
    /// Symlinks are followed for chmod, because the `fchmodat` syscall does not offer a way to not
    /// follow symlinks.
    ///
    /// The `SetAttributes::thread_uid` field is ignored, and the thread's actual capabilities are
    /// are used.
    ///
    /// If `SetAttributes::ctime` is set, the ctime is set to the current time rather than the
    /// specified value.
    pub fn set_attr(&self, path: impl AsRef<Path>, attr: SetAttributes) -> lx::Result<()> {
        self.inner.set_attr(path.as_ref(), attr)
    }

    /// Sets the attributes of a file, and gets the new attributes. Symlinks are not followed.
    ///
    /// See `set_attr` for more details.
    ///
    /// # Windowows
    ///
    /// Attributes are set and retrieved using the same handle, and is therefore faster than
    /// calling `set_attr` and `lstat` separately.
    ///
    /// # Unix
    ///
    /// This does the operations separately, and is therefore susceptible to a race if the item is
    /// removed or replaced between creation and retrieving its attributes.
    pub fn set_attr_stat(
        &self,
        path: impl AsRef<Path>,
        attr: SetAttributes,
    ) -> lx::Result<lx::Stat> {
        self.inner.set_attr_stat(path.as_ref(), attr)
    }

    /// Truncates a file.
    ///
    /// # Windows
    ///
    /// The `thread_uid` argument is used to determine whether or not the set-user-ID and
    /// set-group-ID bits should be cleared. This is ignored if metadata is disabled.
    ///
    /// # Unix
    ///
    /// Unlike the normal `truncate` syscall on Linux, this function does not follow symlinks.
    /// The `thread_uid` argument is ignored, and the thread's actual capabilities are used.
    pub fn truncate(
        &self,
        path: impl AsRef<Path>,
        size: lx::off_t,
        thread_uid: lx::uid_t,
    ) -> lx::Result<()> {
        let mut attr = SetAttributes::default();
        attr.size = Some(size);
        attr.thread_uid = thread_uid;
        self.set_attr(path, attr)
    }

    /// Changes the permissions of a file.
    ///
    /// # Windows
    ///
    /// Chmod is only fully supported if metadata is enabled and the file system supports it.
    /// Without metadata, chmod only changes the read-only attribute if all write bites are
    /// removed from the mode.
    ///
    /// # Unix
    ///
    /// Symlinks are followed for chmod, because the `fchmodat` syscall does not offer a way to not
    /// follow symlinks.
    pub fn chmod(&self, path: impl AsRef<Path>, mode: lx::mode_t) -> lx::Result<()> {
        let mut attr = SetAttributes::default();
        attr.mode = Some(mode);
        self.set_attr(path, attr)
    }

    /// Changes the owner and/or group of a file.
    ///
    /// # Windows
    ///
    /// Chown is only fully supported if metadata is enabled and the file system supports it.
    /// Without metadata, chown silently succeeds without taking any action.
    pub fn chown(
        &self,
        path: impl AsRef<Path>,
        uid: Option<lx::uid_t>,
        gid: Option<lx::gid_t>,
    ) -> lx::Result<()> {
        let mut attr = SetAttributes::default();
        attr.uid = uid;
        attr.gid = gid;
        self.set_attr(path, attr)
    }

    /// Changes a file's time stamps.
    ///
    /// The change time of the file is always set to the current time if this function is called.
    pub fn set_times(
        &self,
        path: impl AsRef<Path>,
        atime: SetTime,
        mtime: SetTime,
    ) -> lx::Result<()> {
        let mut attr = SetAttributes::default();
        attr.atime = atime;
        attr.mtime = mtime;
        attr.ctime = SetTime::Now;
        self.set_attr(path, attr)
    }

    /// Opens or creates a file.
    ///
    /// # Windows
    ///
    /// Not all open flags are supported. In particular, only the flags present in the `lx` module
    /// are supported. Unknown flags are ignored.
    ///
    /// The `O_NOFOLLOW` flag will successfully open a symbolic link, whereas on Unix it will fail
    /// without the `O_PATH` flag (the O_PATH flag is ignored on Windows).
    pub fn open(
        &self,
        path: impl AsRef<Path>,
        flags: i32,
        options: Option<LxCreateOptions>,
    ) -> lx::Result<LxFile> {
        Ok(LxFile {
            inner: self.inner.open(path.as_ref(), flags, options)?,
        })
    }

    /// Creates a new directory.
    pub fn mkdir(&self, path: impl AsRef<Path>, options: LxCreateOptions) -> lx::Result<()> {
        self.inner.mkdir(path.as_ref(), options)
    }

    /// Creates a new directory and retrieves its attributes.
    ///
    /// # Windows
    ///
    /// This uses the handle opened during creation, and is therefore faster than doing the
    /// operations separately.
    ///
    /// # Unix
    ///
    /// This does the operations separately, and is therefore susceptible to a race if the item is
    /// removed or replaced between creation and retrieving its attributes.
    pub fn mkdir_stat(
        &self,
        path: impl AsRef<Path>,
        options: LxCreateOptions,
    ) -> lx::Result<lx::Stat> {
        self.inner.mkdir_stat(path.as_ref(), options)
    }

    /// Creates a new symbolic link.
    ///
    /// The mode on the create options is ignored, as symbolic links always have a mode of 0o777.
    ///
    /// # Windows
    ///
    /// This will attempt to create an NTFS symbolic link, but will fall back to a WSL-style link
    /// if this is not possible.
    pub fn symlink(
        &self,
        path: impl AsRef<Path>,
        target: impl AsRef<lx::LxStr>,
        options: LxCreateOptions,
    ) -> lx::Result<()> {
        self.inner.symlink(path.as_ref(), target.as_ref(), options)
    }

    /// Creates a new symbolic link and retrieves its attributes.
    ///
    /// The mode on the create options is ignored, as symbolic links always have a mode of 0o777.
    ///
    /// # Windows
    ///
    /// This uses the handle opened during creation, and is therefore faster than doing the
    /// operations separately.
    ///
    /// # Unix
    ///
    /// This does the operations separately, and is therefore susceptible to a race if the item is
    /// removed or replaced between creation and retrieving its attributes.
    pub fn symlink_stat(
        &self,
        path: impl AsRef<Path>,
        target: impl AsRef<lx::LxStr>,
        options: LxCreateOptions,
    ) -> lx::Result<lx::Stat> {
        self.inner
            .symlink_stat(path.as_ref(), target.as_ref(), options)
    }

    /// Reads the target of a symbolic link.
    ///
    /// # Windows
    ///
    /// NTFS symlinks will be translated to a Unix-style path. WSL-style symlinks are returned as
    /// is. Use `PathExt` or `PathBufExt` to convert the result to a native path if required.
    pub fn read_link(&self, path: impl AsRef<Path>) -> lx::Result<lx::LxString> {
        self.inner.read_link(path.as_ref())
    }

    /// Removes a file or directory.
    ///
    /// When the `lx::AT_REMOVEDIR` flag is specified, this method removes directories; otherwise,
    /// it removes files.
    ///
    /// # Windows
    ///
    /// NTFS directory symbolic links are counted as files, not directories.
    pub fn unlink(&self, path: impl AsRef<Path>, flags: i32) -> lx::Result<()> {
        self.inner.unlink(path.as_ref(), flags)
    }

    /// Creates a regular, character device, block device, fifo or socket file.
    ///
    /// # Windows
    ///
    /// Only regular files are supported unless metadata is enabled.
    pub fn mknod(
        &self,
        path: impl AsRef<Path>,
        options: LxCreateOptions,
        device_id: lx::dev_t,
    ) -> lx::Result<()> {
        self.inner.mknod(path.as_ref(), options, device_id)
    }

    /// Creates a regular, character device, block device, fifo or socket file, and retrieves its
    /// attributes.
    ///
    /// # Windows
    ///
    /// Only regular files are supported unless metadata is enabled.
    ///
    /// This uses the handle opened during creation, and is therefore faster than doing the
    /// operations separately.
    ///
    /// # Unix
    ///
    /// This does the operations separately, and is therefore susceptible to a race if the item is
    /// removed or replaced between creation and retrieving its attributes.
    pub fn mknod_stat(
        &self,
        path: impl AsRef<Path>,
        options: LxCreateOptions,
        device_id: lx::dev_t,
    ) -> lx::Result<lx::Stat> {
        self.inner.mknod_stat(path.as_ref(), options, device_id)
    }

    /// Renames a file.
    ///
    /// Flags correspond to the flags of the `renameat2` syscall in Linux.
    ///
    /// # Windows
    ///
    /// This function will use POSIX rename if the file system supports it. No flags are currently
    /// supported.
    pub fn rename(
        &self,
        path: impl AsRef<Path>,
        new_path: impl AsRef<Path>,
        flags: u32,
    ) -> lx::Result<()> {
        self.inner.rename(path.as_ref(), new_path.as_ref(), flags)
    }

    /// Creates a new hard link to a file.
    pub fn link(&self, path: impl AsRef<Path>, new_path: impl AsRef<Path>) -> lx::Result<()> {
        self.inner.link(path.as_ref(), new_path.as_ref())
    }

    /// Creates a new hard link to a file and retrieves its attributes.
    ///
    /// # Windows
    ///
    /// This uses the handle opened during creation, and is therefore faster than doing the
    /// operations separately.
    ///
    /// # Unix
    ///
    /// This does the operations separately, and is therefore susceptible to a race if the item is
    /// removed or replaced between creation and retrieving its attributes.
    pub fn link_stat(
        &self,
        path: impl AsRef<Path>,
        new_path: impl AsRef<Path>,
    ) -> lx::Result<lx::Stat> {
        self.inner.link_stat(path.as_ref(), new_path.as_ref())
    }

    /// Retrieve attributes of the file system.
    ///
    /// The path passed should not really matter, unless there are multiple file systems accessible
    /// from this LxVolume.
    ///
    /// # Windows
    ///
    /// The `StatFs::fs_type` and `StatFs::flags` field will not be set as they are not relevant
    /// to Windows.
    pub fn stat_fs(&self, path: impl AsRef<Path>) -> lx::Result<lx::StatFs> {
        self.inner.stat_fs(path.as_ref())
    }

    /// Sets an extended attribute on a file.
    ///
    /// # Windows
    ///
    /// Extended attribute names are not case sensitive. They are stored as upper case in NTFS but
    /// `list_xattr` will report them as lower case for greater compatibility with Linux.
    ///
    /// Extended attribute names are prefixed with "LX.", and have a slightly shorter maximum length
    /// limit than Linux. Attribute values are prefixed with a 4-byte header to allow for "empty"
    /// values, which NTFS does not normally allow. `get_xattr` and `list_xattr` will strip these
    /// prefixes.
    ///
    /// Security for accessing the various attribute namespaces is not enforced.
    ///
    /// If the flags `XATTR_CREATE` or `XATTR_REPLACE` are used, the operation is not atomic
    /// because Windows has to separately check for the attribute's existence. In this case, there
    /// is a small possibility of a race where an attribute created by another thread gets
    /// overwritten.
    pub fn set_xattr(
        &self,
        path: impl AsRef<Path>,
        name: impl AsRef<lx::LxStr>,
        value: &[u8],
        flags: i32,
    ) -> lx::Result<()> {
        self.inner
            .set_xattr(path.as_ref(), name.as_ref(), value, flags)
    }

    /// Gets the value or size of an extended attribute on a file.
    ///
    /// This function will return the size of the attribute.
    ///
    /// # Windows
    ///
    /// Extended attribute names are not case sensitive. They are stored as upper case in NTFS but
    /// `list_xattr` will report them as lower case for greater compatibility with Linux.
    ///
    /// Extended attribute names are prefixed with "LX.", and have a slightly shorter maximum length
    /// limit than Linux. Attribute values are prefixed with a 4-byte header to allow for "empty"
    /// values, which NTFS does not normally allow. `get_xattr` and `list_xattr` will strip these
    /// prefixes.
    ///
    /// Security for accessing the various attribute namespaces is not enforced.
    pub fn get_xattr(
        &self,
        path: impl AsRef<Path>,
        name: impl AsRef<lx::LxStr>,
        value: Option<&mut [u8]>,
    ) -> lx::Result<usize> {
        self.inner.get_xattr(path.as_ref(), name.as_ref(), value)
    }

    /// Gets a list of all the extended attributes on a file.
    ///
    /// This function will return the size of the list.
    ///
    /// The list contains the names of all the attributes, separated by NULL characters.
    ///
    /// # Windows
    ///
    /// Extended attribute names are not case sensitive. They are stored as upper case in NTFS but
    /// `list_xattr` will report them as lower case for greater compatibility with Linux.
    ///
    /// Extended attribute names are prefixed with "LX.", and have a slightly shorter maximum length
    /// limit than Linux. Attribute values are prefixed with a 4-byte header to allow for "empty"
    /// values, which NTFS does not normally allow. `get_xattr` and `list_xattr` will strip these
    /// prefixes.
    ///
    /// Security for accessing the various attribute namespaces is not enforced.
    pub fn list_xattr(&self, path: impl AsRef<Path>, list: Option<&mut [u8]>) -> lx::Result<usize> {
        self.inner.list_xattr(path.as_ref(), list)
    }

    /// Removes an extended attribute from the file.
    ///
    /// # Windows
    ///
    /// Extended attribute names are not case sensitive. They are stored as upper case in NTFS but
    /// `list_xattr` will report them as lower case for greater compatibility with Linux.
    ///
    /// Extended attribute names are prefixed with "LX.", and have a slightly shorter maximum length
    /// limit than Linux. Attribute values are prefixed with a 4-byte header to allow for "empty"
    /// values, which NTFS does not normally allow. `get_xattr` and `list_xattr` will strip these
    /// prefixes.
    ///
    /// Security for accessing the various attribute namespaces is not enforced.
    pub fn remove_xattr(
        &self,
        path: impl AsRef<Path>,
        name: impl AsRef<lx::LxStr>,
    ) -> lx::Result<()> {
        self.inner.remove_xattr(path.as_ref(), name.as_ref())
    }

    /// Creates a new instance of `LxVolume` using the specified root path and options.
    fn new_with_options(
        root_path: impl AsRef<Path>,
        options: &LxVolumeOptions,
    ) -> lx::Result<Self> {
        Ok(Self {
            inner: sys::LxVolume::new(root_path.as_ref(), options)?,
        })
    }
}

/// A platform-independent abstraction that allows you to treat a file as if it has Unix semantics.
///
/// `LxFile` instances are created by using `LxVolume::open`.
pub struct LxFile {
    inner: sys::LxFile,
}

impl LxFile {
    /// Retrieves the attributes of the file.
    pub fn fstat(&self) -> lx::Result<lx::StatEx> {
        self.inner.fstat()
    }

    /// Sets the attributes of the file.
    ///
    /// This function combines the functionality of `truncate`, `chmod`, `chown` and `utimensat`.
    ///
    /// If this function fails, some of the operations may still have succeeded.
    ///
    /// # Windows
    ///
    /// Chmod and chown are only fully supported if metadata is enabled and the file system supports
    /// it. Without metadata, chmod only changes the read-only attribute if all write bits are
    /// removed from the mode, and chown silently succeeds without taking any action.
    ///
    /// This function disables the set-user-ID and set-group-ID as required if a request is made
    /// to change the size, owner or group of a file. This is done based on whether the
    /// `SetAttributes::thread_uid` field indicates the user is root.
    ///
    /// # Unix
    ///
    /// The `SetAttributes::thread_uid` field is ignored, and the thread's actual capabilities are
    /// are used.
    ///
    /// If `SetAttributes::ctime` is set, the ctime is set to the current time rather than the
    /// specified value.
    pub fn set_attr(&self, attr: SetAttributes) -> lx::Result<()> {
        self.inner.set_attr(attr)
    }

    /// Truncates a file.
    ///
    /// # Windows
    ///
    /// The `thread_uid` argument is used to determine whether or not the set-user-ID and
    /// set-group-ID bits should be cleared. This is ignored if metadata is disabled.
    ///
    /// # Unix
    ///
    /// Unlike the normal `truncate` syscall on Linux, this function does not follow symlinks.
    /// The `thread_uid` argument is ignored, and the thread's actual capabilities are used.
    pub fn truncate(&self, size: lx::off_t, thread_uid: lx::uid_t) -> lx::Result<()> {
        let mut attr = SetAttributes::default();
        attr.size = Some(size);
        attr.thread_uid = thread_uid;
        self.set_attr(attr)
    }

    /// Changes the permissions of a file.
    ///
    /// # Windows
    ///
    /// Chmod is only fully supported if metadata is enabled and the file system supports it.
    /// Without metadata, chmod only changes the read-only attribute if all write bites are
    /// removed from the mode.
    pub fn chmod(&self, mode: lx::mode_t) -> lx::Result<()> {
        let mut attr = SetAttributes::default();
        attr.mode = Some(mode);
        self.set_attr(attr)
    }

    /// Changes the owner and/or group of a file.
    ///
    /// # Windows
    ///
    /// Chown is only fully supported if metadata is enabled and the file system supports it.
    /// Without metadata, chown silently succeeds without taking any action.
    pub fn chown(&self, uid: Option<lx::uid_t>, gid: Option<lx::gid_t>) -> lx::Result<()> {
        let mut attr = SetAttributes::default();
        attr.uid = uid;
        attr.gid = gid;
        self.set_attr(attr)
    }

    /// Changes a file's time stamps.
    ///
    /// The change time of the file is always set to the current time if this function is called.
    pub fn set_times(&self, atime: SetTime, mtime: SetTime) -> lx::Result<()> {
        let mut attr = SetAttributes::default();
        attr.atime = atime;
        attr.mtime = mtime;
        attr.ctime = SetTime::Now;
        self.set_attr(attr)
    }

    /// Reads a number of bytes starting from a given offset.
    ///
    /// Returns the number of bytes read.
    ///
    /// On Windows, the file pointer is changed after this operation, while on Unix, it is not.
    pub fn pread(&self, buffer: &mut [u8], offset: lx::off_t) -> lx::Result<usize> {
        self.inner.pread(buffer, offset)
    }

    /// Writes a number of bytes starting from a given offset.
    ///
    /// Returns the number of bytes written.
    ///
    /// # Windows
    ///
    /// The file pointer is changed after this operation, while on Unix, it is not.
    ///
    /// The `thread_uid` argument is used to determine whether or not the set-user-ID and
    /// set-group-ID bits should be cleared. This is ignored if metadata is disabled.
    ///
    /// # Unix
    ///
    /// The `thread_uid` argument is ignored, and the thread's actual capabilities are used.
    pub fn pwrite(
        &self,
        buffer: &[u8],
        offset: lx::off_t,
        thread_uid: lx::uid_t,
    ) -> lx::Result<usize> {
        self.inner.pwrite(buffer, offset, thread_uid)
    }

    /// Reads the contents of the directory, invoking the callback for each item.
    ///
    /// If the callback returns an error, it is propagated to the caller. If the callback returns
    /// false, enumeration is stopped but no error is returned. Enumeration can be continued
    /// from the same position by calling this function again with the offset of the entry *before*
    /// the one that cancelled the enumeration.
    ///
    /// # Windows
    ///
    /// The . and .. entries are always returned, but their inode number is not set.
    ///
    /// # Unix
    ///
    /// The . and .. entries are returned only if the underlying file system returns them.
    pub fn read_dir<F>(&mut self, offset: lx::off_t, callback: F) -> lx::Result<()>
    where
        F: FnMut(lx::DirEntry) -> lx::Result<bool>,
    {
        self.inner.read_dir(offset, callback)
    }

    /// Synchronizes the file's buffer.
    ///
    /// This function can optionally synchronize only data, not metadata.
    pub fn fsync(&self, data_only: bool) -> lx::Result<()> {
        self.inner.fsync(data_only)
    }
}

/// Sets options used by an LxVolume. These control whether metadata is enabled, and set defaults
/// to use for files without metadata.
///
/// # Unix
///
/// These options have no effect on Unix platforms.
#[derive(Clone)]
pub struct LxVolumeOptions {
    uid: Option<lx::uid_t>,
    gid: Option<lx::uid_t>,
    mode: Option<u32>,
    default_uid: lx::uid_t,
    default_gid: lx::gid_t,
    umask: u32,
    fmask: u32,
    dmask: u32,
    metadata: bool,
    create_case_sensitive_dirs: bool,
    sandbox: bool,
    sandbox_disallowed_extensions: Vec<OsString>,
    symlink_root: String,
    override_xattrs: HashMap<String, Vec<u8>>,
    readonly: bool,
}

impl LxVolumeOptions {
    /// Create a new `LxVolumeOptions` with default options.
    pub fn new() -> Self {
        Self {
            uid: None,
            gid: None,
            mode: None,
            default_uid: 0,
            default_gid: 0,
            umask: u32::MAX,
            fmask: u32::MAX,
            dmask: u32::MAX,
            metadata: false,
            create_case_sensitive_dirs: false,
            sandbox: false,
            sandbox_disallowed_extensions: Vec::new(),
            symlink_root: "".to_string(),
            override_xattrs: HashMap::new(),
            readonly: false,
        }
    }

    /// Create a new 'LxVolumeOptions' using a semi-colon separated list of options of the form
    /// uid=1000;gid=1000;symlinkroot=/mnt/
    pub fn from_option_string(option_string: &str) -> Self {
        let mut options = Self::new();
        for next in option_string.split(';') {
            if next.is_empty() {
                continue;
            }
            let (keyword, value) = match next.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (next, None),
            };
            match keyword {
                "metadata" => {
                    if value.is_none() {
                        options.metadata(true);
                    } else {
                        tracing::warn!(value, "'metadata' option does not support value");
                    }
                }
                "case" => {
                    if let Some(value) = value {
                        if value == "dir" {
                            options.create_case_sensitive_dirs(true);
                        } else if value == "off" {
                            options.create_case_sensitive_dirs(false);
                        } else {
                            tracing::warn!(value, "Unrecognized 'case' option");
                        }
                    } else {
                        tracing::warn!("'case' option requires value");
                    }
                }
                "uid" => {
                    if let Some(value) = value {
                        if let Ok(uid) = value.parse::<u32>() {
                            options.uid(uid);
                        } else {
                            tracing::warn!(value, "Unrecognized value for 'uid'");
                        }
                    } else {
                        tracing::warn!("'uid' option requires value");
                    }
                }
                "gid" => {
                    if let Some(value) = value {
                        if let Ok(gid) = value.parse::<u32>() {
                            options.gid(gid);
                        } else {
                            tracing::warn!(value, "Unrecognized value for 'gid'");
                        }
                    } else {
                        tracing::warn!("'gid' option requires value");
                    }
                }
                "mode" => {
                    if let Some(value) = value {
                        if let Ok(mode) = value.parse::<u32>() {
                            if (mode & !0o777) == 0 {
                                options.mode(mode);
                            } else {
                                tracing::warn!(value, "Invalid 'mode' value");
                            }
                        } else {
                            tracing::warn!(value, "Unrecognized value for 'mode'");
                        }
                    } else {
                        tracing::warn!("'mode' option requires value");
                    }
                }
                "default_uid" => {
                    if let Some(value) = value {
                        if let Ok(uid) = value.parse::<u32>() {
                            options.default_uid(uid);
                        } else {
                            tracing::warn!(value, "Unrecognized value for 'uid'");
                        }
                    } else {
                        tracing::warn!("'default_uid' option requires value");
                    }
                }
                "default_gid" => {
                    if let Some(value) = value {
                        if let Ok(gid) = value.parse::<u32>() {
                            options.default_gid(gid);
                        } else {
                            tracing::warn!(value, "Unrecognized value for 'gid'");
                        }
                    } else {
                        tracing::warn!("'default_gid' option requires value");
                    }
                }
                "umask" => {
                    if let Some(value) = value {
                        if let Ok(umask) = value.parse::<u32>() {
                            if (umask & !0o777) == 0 {
                                options.umask(umask);
                            } else {
                                tracing::warn!(value, "Invalid 'umask' value");
                            }
                        } else {
                            tracing::warn!(value, "Unrecognized value for 'umask'");
                        }
                    } else {
                        tracing::warn!("'umask' option requires value");
                    }
                }
                "dmask" => {
                    if let Some(value) = value {
                        if let Ok(dmask) = value.parse::<u32>() {
                            if (dmask & !0o777) == 0 {
                                options.dmask(dmask);
                            } else {
                                tracing::warn!(value, "Invalid 'dmask' value");
                            }
                        } else {
                            tracing::warn!(value, "Unrecognized value for 'dmask'");
                        }
                    } else {
                        tracing::warn!("'dmask' option requires value");
                    }
                }
                "fmask" => {
                    if let Some(value) = value {
                        if let Ok(fmask) = value.parse::<u32>() {
                            if (fmask & !0o777) == 0 {
                                options.fmask(fmask);
                            } else {
                                tracing::warn!(value, "Invalid 'fmask' value");
                            }
                        } else {
                            tracing::warn!(value, "Unrecognized value for 'fmask'");
                        }
                    } else {
                        tracing::warn!("'fmask' option requires value");
                    }
                }
                "symlinkroot" => {
                    if let Some(value) = value {
                        options.symlink_root(value);
                    } else {
                        tracing::warn!("'symlinkroot' option requires value");
                    }
                }
                "xattr" => {
                    if let Some(value) = value {
                        let (xattr_key, xattr_val) = match value.split_once('=') {
                            Some(v) => v,
                            None => (value, ""),
                        };
                        options.override_xattr(xattr_key, xattr_val.as_bytes());
                    } else {
                        tracing::warn!("'xattr' option requires value");
                    }
                }
                "sandbox" => {
                    if value.is_none() {
                        options.sandbox(true);
                    } else {
                        tracing::warn!(value, "'sandbox' options does not support value");
                    }
                }
                "sandbox_disallowed_extensions" => {
                    if let Some(value) = value {
                        let extensions: Vec<&str> = value.split(',').collect();
                        options.sandbox_disallowed_extensions(extensions);
                    } else {
                        tracing::warn!("'sandbox_disallowed_extensions' option requires value");
                    }
                }
                "ro" => {
                    if value.is_none() {
                        options.readonly(true);
                    } else {
                        tracing::warn!(value, "'ro' option does not support value");
                    }
                }
                _ => tracing::warn!(option = %next, keyword, "Unrecognized mount option"),
            }
        }

        options
    }

    /// Creates a new `LxVolume` with the current options.
    pub fn new_volume(&self, root_path: impl AsRef<Path>) -> lx::Result<LxVolume> {
        LxVolume::new_with_options(root_path, self)
    }

    /// Set the owner user ID for all files.
    pub fn uid(&mut self, uid: lx::uid_t) -> &mut Self {
        self.uid = Some(uid);
        self
    }

    /// Set the owner group ID for all files.
    pub fn gid(&mut self, gid: lx::gid_t) -> &mut Self {
        self.gid = Some(gid);
        self
    }

    /// Sets the mode bits for all files. Directories will add 'x' automatically if 'r' is set to allow list.
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = Some(mode & 0o777);
        self
    }

    /// Set the owner user ID for files without metadata.
    pub fn default_uid(&mut self, uid: lx::uid_t) -> &mut Self {
        self.default_uid = uid;
        self
    }

    /// Set the owner group ID for files without metadata.
    pub fn default_gid(&mut self, gid: lx::gid_t) -> &mut Self {
        self.default_gid = gid;
        self
    }

    /// Set a mask of mode bits that will always be disabled on files and directories that have no
    /// metadata.
    pub fn umask(&mut self, umask: u32) -> &mut Self {
        self.umask = !(umask & 0o7777);
        self
    }

    /// Set a mask of mode bits that will always be disabled on files that have no metadata.
    pub fn fmask(&mut self, fmask: u32) -> &mut Self {
        self.fmask = !(fmask & 0o7777);
        self
    }

    /// Set a mask of mode bits that will always be disabled on directories that have no metadata.
    pub fn dmask(&mut self, dmask: u32) -> &mut Self {
        self.dmask = !(dmask & 0o7777);
        self
    }

    /// Enable or disable metadata for the volume.
    ///
    /// This will be ignored if the underlying file system does not support the required features
    /// to emulate Linux attributes on Windows.
    pub fn metadata(&mut self, metadata: bool) -> &mut Self {
        self.metadata = metadata;
        self
    }

    /// Apply additional file restrictions.
    ///
    /// Hide files and directories that are marked as hidden or may cause hydration (e.g. OneDrive backed).
    pub fn sandbox(&mut self, enabled: bool) -> &mut Self {
        self.sandbox = enabled;
        self
    }

    /// Exclude specific file extensions when sandbox mode is enabled.
    ///
    /// Hide files and directories with specific file extensions. Do not allow creation of new files with these extensions.
    pub fn sandbox_disallowed_extensions(&mut self, disallowed_extensions: Vec<&str>) -> &mut Self {
        let mut disallowed_extensions = disallowed_extensions
            .into_iter()
            .map(OsString::from)
            .map(|ext| ext.to_ascii_lowercase())
            .collect();
        self.sandbox_disallowed_extensions
            .append(&mut disallowed_extensions);
        self
    }

    /// Enable or disable whether new directories are created as case sensitive.
    ///
    /// This will be ignored if the underlying file system does not support case sensitive
    /// directories.
    ///
    /// This does not affect the behavior of existing case sensitive directories, where operations
    /// will be case sensitive and new directories will inherit the flag.
    pub fn create_case_sensitive_dirs(&mut self, create_case_sensitive_dirs: bool) -> &mut Self {
        self.create_case_sensitive_dirs = create_case_sensitive_dirs;
        self
    }

    /// Set the root used to translate absolute Windows symlinks paths.
    ///
    /// EXAMPLE: A symlink to C:\my\target will return /mnt/c/my/target if symlink_root is set to "/mnt/".
    pub fn symlink_root(&mut self, symlink_root: &str) -> &mut Self {
        self.symlink_root = symlink_root.to_string();
        self
    }

    /// Add an extended attribute to return with every file in the volume.
    ///
    /// This will be used in place of any actual extended attributes associated with the file.
    ///
    /// N.B. Since some attributes may be related, replacing the returned attributes is deemed easier and safer than
    ///      trying to union overrides with existing attributes.
    pub fn override_xattr(&mut self, name: &str, val: &[u8]) -> &mut Self {
        let mut val_data = Vec::with_capacity(val.len());
        val_data.extend_from_slice(val);
        self.override_xattrs.insert(name.to_string(), val_data);
        self
    }

    /// Enable or disable readonly mode for the volume.
    ///
    /// When readonly is enabled, all write operations will be rejected with EROFS.
    pub fn readonly(&mut self, readonly: bool) -> &mut Self {
        self.readonly = readonly;
        self
    }

    /// Returns whether the volume is configured as readonly.
    pub fn is_readonly(&self) -> bool {
        self.readonly
    }
}

impl Default for LxVolumeOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Specifies options to use when creating a file.
///
/// The user ID and group ID are only used on Windows; on Unix platforms, set the thread's effective
/// user ID and group ID to change the owner of a newly created file.
#[derive(Default)]
pub struct LxCreateOptions {
    mode: lx::mode_t,
    #[cfg_attr(not(windows), expect(dead_code))]
    uid: lx::uid_t,
    #[cfg_attr(not(windows), expect(dead_code))]
    gid: lx::gid_t,
}

impl LxCreateOptions {
    /// Creates a new `LxCreateOptions`.
    pub fn new(mode: lx::mode_t, uid: lx::uid_t, gid: lx::gid_t) -> Self {
        Self { mode, uid, gid }
    }
}

/// Supplies the attributes to change for `set_attr`.
#[derive(Default, Clone, Copy)]
pub struct SetAttributes {
    /// Truncate the file.
    pub size: Option<lx::off_t>,

    /// Set the access time.
    pub atime: SetTime,

    /// Set the modified time.
    pub mtime: SetTime,

    /// Set the change time.
    ///
    /// Some file systems only support setting the change time to the current time.
    pub ctime: SetTime,

    /// Set the file's mode.
    ///
    /// # Windows
    ///
    /// The mode must include the file type, and must match the existing file type.
    ///
    /// # Unix
    ///
    /// The file type will be ignored.
    pub mode: Option<lx::mode_t>,

    /// Set the file's owner user ID.
    pub uid: Option<lx::uid_t>,

    /// Set the file's owner group ID.
    pub gid: Option<lx::gid_t>,

    /// The current thread's effective user ID.
    ///
    /// # Windows
    ///
    /// This is used to determine whether truncation needs to clear the set-user-ID and set-group-ID
    /// attributes. It is ignored if metadata is disabled.
    ///
    /// # Unix
    ///
    /// The actual thread's capabilities are used, so this value is ignored.
    pub thread_uid: lx::uid_t,
}

/// Supplies the value to set a time attribute to.
#[derive(Clone, Copy, Default)]
pub enum SetTime {
    /// Don't change the time.
    #[default]
    Omit,
    /// Set the time to the specified value.
    Set(std::time::Duration),
    /// Set the time to the current time.
    Now,
}

impl SetTime {
    /// Checks whether the value matches the `Omit` variant.
    pub fn is_omit(&self) -> bool {
        matches!(self, SetTime::Omit)
    }
}

#[cfg(test)]
// UNSAFETY: Calls to libc to check and manipulate permissions.
#[cfg_attr(all(test, unix), expect(unsafe_code))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn lstat() {
        let env = TestEnv::new();
        env.create_file("testfile", "test");
        let stat = env.volume.lstat("testfile").unwrap();
        println!("{:#?}", stat);
        assert_ne!(stat.inode_nr, 0);
        assert_eq!(stat.link_count, 1);
        assert_eq!(stat.mode & lx::S_IFMT, lx::S_IFREG);
        assert_ne!(stat.mode & 0o777, 0);
        assert_eq!(stat.file_size, 4);

        let result = env.volume.lstat("no_ent").unwrap_err();
        assert_eq!(result.value(), lx::ENOENT);

        let stat = env.volume.lstat("").unwrap();
        println!("{:#?}", stat);
        assert_ne!(stat.inode_nr, 0);
        assert!(stat.link_count >= 1);
        assert_eq!(stat.mode & lx::S_IFMT, lx::S_IFDIR);
        assert_ne!(stat.mode & 0o777, 0);
    }

    #[test]
    fn fstat() {
        let env = TestEnv::new();
        env.create_file("testfile", "test");
        let stat = env.volume.lstat("testfile").unwrap();
        let file = env.volume.open("testfile", lx::O_RDONLY, None).unwrap();
        let fstat = file.fstat().unwrap().into();
        println!("{:#?}", fstat);
        assert_eq!(stat, fstat);

        let stat = env.volume.lstat("").unwrap();
        let file = env
            .volume
            .open("", lx::O_RDONLY | lx::O_DIRECTORY, None)
            .unwrap();

        let fstat = file.fstat().unwrap().into();
        println!("{:#?}", fstat);
        assert_eq!(stat, fstat);
    }

    #[test]
    fn read_write() {
        let env = TestEnv::new();
        let file = env
            .volume
            .open(
                "testfile",
                lx::O_RDWR | lx::O_CREAT | lx::O_EXCL,
                Some(LxCreateOptions::new(0o666, 0, 0)),
            )
            .unwrap();

        assert_eq!(file.fstat().unwrap().file_size, 0);

        // Write some text.
        assert_eq!(file.pwrite(b"Hello", 0, 0).unwrap(), 5);
        assert_eq!(file.fstat().unwrap().file_size, 5);
        assert_eq!(file.pwrite(b", world!", 5, 0).unwrap(), 8);
        assert_eq!(file.fstat().unwrap().file_size, 13);

        // Read the whole thing back.
        let mut buffer = [0; 1024];
        assert_eq!(file.pread(&mut buffer, 0).unwrap(), 13);
        assert_eq!(&buffer[..13], b"Hello, world!");

        // Read at EOF.
        assert_eq!(file.pread(&mut buffer, 13).unwrap(), 0);

        // Write over part of it.
        assert_eq!(file.pwrite(b"Bye", 4, 0).unwrap(), 3);
        assert_eq!(file.fstat().unwrap().file_size, 13);

        // Read part of it.
        assert_eq!(file.pread(&mut buffer[..8], 2).unwrap(), 8);
        assert_eq!(&buffer[..8], b"llByewor");

        // Can't write if O_RDONLY.
        let file = env.volume.open("testfile", lx::O_RDONLY, None).unwrap();
        assert_eq!(file.pwrite(b"Hello", 0, 0).unwrap_err().value(), lx::EBADF);
        assert_eq!(file.pread(&mut buffer, 0).unwrap(), 13);

        // Can't read if O_WRONLY
        let file = env.volume.open("testfile", lx::O_WRONLY, None).unwrap();
        assert_eq!(file.pread(&mut buffer, 0).unwrap_err().value(), lx::EBADF);
        assert_eq!(file.pwrite(b"Hello", 0, 0).unwrap(), 5);

        // Can't do either if O_NOACCESS
        let file = env.volume.open("testfile", lx::O_NOACCESS, None).unwrap();
        assert_eq!(file.pwrite(b"Hello", 0, 0).unwrap_err().value(), lx::EBADF);
        assert_eq!(file.pread(&mut buffer, 0).unwrap_err().value(), lx::EBADF);
    }

    #[test]
    fn read_dir() {
        let env = TestEnv::new();
        let mut map = HashMap::new();
        map.insert(String::from("."), false);
        map.insert(String::from(".."), false);
        for i in 0..10 {
            let name = format!("file{}", i);
            env.create_file(&name, "test");
            assert!(map.insert(name, false).is_none());
        }

        let mut dir = env
            .volume
            .open("", lx::O_RDONLY | lx::O_DIRECTORY, None)
            .unwrap();

        let mut count = 0;
        let mut next_offset = 0;
        let mut seek_file = String::new();
        let mut seek_offset = 0;
        let mut prev_offset = 0;

        // Read up until the 6th file.
        dir.read_dir(0, |entry| {
            if count == 6 {
                return Ok(false);
            }

            count += 1;
            next_offset = entry.offset;
            println!("Entry 1: {:?}", entry);
            env.check_dir_entry(&entry);
            let name = entry.name.to_str().unwrap();
            let found_entry = map.get_mut(name).unwrap();
            assert!(!*found_entry);
            *found_entry = true;

            // Remember a file name and its offset so we can seek back to it later.
            if count == 4 {
                seek_file = String::from(name);
                seek_offset = prev_offset;
            }

            prev_offset = entry.offset;
            Ok(true)
        })
        .unwrap();

        // Continue from the last offset seen; this tests that files aren't skipped or double reported.
        dir.read_dir(next_offset, |entry| {
            count += 1;
            println!("Entry 2: {:?}", entry);
            env.check_dir_entry(&entry);
            let name = entry.name.to_str().unwrap();
            let found_entry = map.get_mut(name).unwrap();
            assert!(!*found_entry);
            *found_entry = true;
            Ok(true)
        })
        .unwrap();

        // Confirm that every file has been seen exactly once.
        assert_eq!(count, 12);
        for (_, value) in map {
            assert!(value);
        }

        // Confirm that we can use the same file to seek to the start and enumerate again.
        count = 0;
        dir.read_dir(0, |_| {
            count += 1;
            Ok(true)
        })
        .unwrap();

        assert_eq!(count, 12);

        // Check that we can seek to a specific file.
        count = 0;
        dir.read_dir(seek_offset, |entry| {
            assert_eq!(entry.name.to_str().unwrap(), seek_file);
            count += 1;
            Ok(false)
        })
        .unwrap();

        assert_eq!(count, 1);

        // Check that errors are propagated.
        count = 0;
        let error = dir
            .read_dir(0, |_| {
                count += 1;
                Err(lx::Error::ECONNREFUSED)
            })
            .unwrap_err();

        assert_eq!(count, 1);
        assert_eq!(error.value(), lx::ECONNREFUSED);
    }

    #[test]
    #[should_panic(expected = "at the disco")]
    fn read_dir_panic() {
        let env = TestEnv::new();
        env.create_file("testfile", "test");
        let mut dir = env
            .volume
            .open("", lx::O_RDONLY | lx::O_DIRECTORY, None)
            .unwrap();

        // Make sure the closure can safely panic even when invoked through the C callback on
        // Windows.
        dir.read_dir(0, |entry| {
            if entry.file_type == lx::DT_REG {
                panic!("at the disco");
            }

            Ok(true)
        })
        .unwrap();
    }

    #[test]
    fn metadata() {
        let env = TestEnv::with_options(LxVolumeOptions::new().metadata(true));
        let file = env
            .volume
            .open(
                "testfile",
                lx::O_RDWR | lx::O_CREAT | lx::O_EXCL,
                Some(LxCreateOptions::new(0o640, 1000, 2000)),
            )
            .unwrap();

        let stat = file.fstat().unwrap();
        assert_eq!(stat.mode as u32, lx::S_IFREG | 0o640);
        // Only Windows uses the uid/gid
        if cfg!(windows) {
            assert_eq!(stat.uid, 1000);
            assert_eq!(stat.gid, 2000);
        }

        env.volume
            .mkdir("testdir", LxCreateOptions::new(0o751, 1001, 2001))
            .unwrap();

        let stat = env.volume.lstat("testdir").unwrap();
        assert_eq!(stat.mode, lx::S_IFDIR | 0o751);
        if cfg!(windows) {
            assert_eq!(stat.uid, 1001);
            assert_eq!(stat.gid, 2001);
        }

        let stat = env
            .volume
            .mkdir_stat("testdir2", LxCreateOptions::new(0o777, 1002, 2002))
            .unwrap();

        assert_eq!(stat.mode, lx::S_IFDIR | 0o777);
        if cfg!(windows) {
            assert_eq!(stat.uid, 1002);
            assert_eq!(stat.gid, 2002);
        }

        env.volume
            .symlink("testlink", "testdir", LxCreateOptions::new(0, 2000, 3000))
            .unwrap();

        let stat = env.volume.lstat("testlink").unwrap();
        assert_eq!(stat.mode, lx::S_IFLNK | 0o777);
        assert_eq!(stat.file_size, 7);
        if cfg!(windows) {
            assert_eq!(stat.uid, 2000);
            assert_eq!(stat.gid, 3000);
        }

        let stat = env
            .volume
            .symlink_stat("testlink2", "testdir2", LxCreateOptions::new(0, 2001, 3001))
            .unwrap();

        assert_eq!(stat.mode, lx::S_IFLNK | 0o777);
        assert_eq!(stat.file_size, 8);
        if cfg!(windows) {
            assert_eq!(stat.uid, 2001);
            assert_eq!(stat.gid, 3001);
        }

        // Chmod/chown (chown is skipped on Linux because we may not have permission).
        let mut attr = SetAttributes::default();
        attr.mode = Some(lx::S_IFREG | 0o664);
        if cfg!(windows) {
            attr.uid = Some(2000);
            attr.gid = Some(3000);
        }

        env.volume.set_attr("testfile", attr).unwrap();
        let stat = env.volume.lstat("testfile").unwrap();
        assert_eq!(stat.mode, lx::S_IFREG | 0o664);
        if cfg!(windows) {
            assert_eq!(stat.uid, 2000);
            assert_eq!(stat.gid, 3000);
        }

        // On real Linux, root is needed to create device files.
        if is_lx_root() {
            env.volume
                .mknod(
                    "testchr",
                    LxCreateOptions::new(lx::S_IFCHR | 0o640, 1000, 2000),
                    lx::make_dev(1, 5),
                )
                .unwrap();

            let stat = env.volume.lstat("testchr").unwrap();
            assert_eq!(stat.mode, lx::S_IFCHR | 0o640);
            if cfg!(windows) {
                assert_eq!(stat.uid, 1000);
                assert_eq!(stat.gid, 2000);
            }

            assert_eq!(lx::major64(stat.device_nr_special as lx::dev_t), 1);
            assert_eq!(lx::minor(stat.device_nr_special as lx::dev_t), 5);

            let stat = env
                .volume
                .mknod_stat(
                    "testblk",
                    LxCreateOptions::new(lx::S_IFBLK | 0o660, 1001, 2001),
                    lx::make_dev(2, 6),
                )
                .unwrap();

            assert_eq!(stat.mode, lx::S_IFBLK | 0o660);
            if cfg!(windows) {
                assert_eq!(stat.uid, 1001);
                assert_eq!(stat.gid, 2001);
            }

            assert_eq!(lx::major64(stat.device_nr_special as lx::dev_t), 2);
            assert_eq!(lx::minor(stat.device_nr_special as lx::dev_t), 6);
        }

        env.volume
            .mknod(
                "testfifo",
                LxCreateOptions::new(lx::S_IFIFO | 0o666, 1002, 2002),
                lx::make_dev(2, 6),
            )
            .unwrap();

        let stat = env.volume.lstat("testfifo").unwrap();
        assert_eq!(stat.mode, lx::S_IFIFO | 0o666);
        if cfg!(windows) {
            assert_eq!(stat.uid, 1002);
            assert_eq!(stat.gid, 2002);
        }

        assert_eq!(stat.device_nr_special, 0);

        let stat = env
            .volume
            .mknod_stat(
                "testsock",
                LxCreateOptions::new(lx::S_IFSOCK | 0o600, 1003, 2003),
                lx::make_dev(2, 6),
            )
            .unwrap();

        assert_eq!(stat.mode, lx::S_IFSOCK | 0o600);
        if cfg!(windows) {
            assert_eq!(stat.uid, 1003);
            assert_eq!(stat.gid, 2003);
        }

        assert_eq!(stat.device_nr_special, 0);
    }

    #[test]
    fn path_escape() {
        let env = TestEnv::new();
        env.volume
            .mkdir("testdir", LxCreateOptions::new(0o777, 0, 0))
            .unwrap();
        env.create_file("testdir/testfile", "foo");
        env.volume
            .lstat(Path::from_lx("testdir/testfile").unwrap())
            .unwrap();
        let path = PathBuf::from_lx("testdir/foo:bar").unwrap();
        let file = env
            .volume
            .open(
                &path,
                lx::O_RDONLY | lx::O_CREAT,
                Some(LxCreateOptions::new(0o666, 0, 0)),
            )
            .unwrap();

        let file_stat: lx::Stat = file.fstat().unwrap().into();
        assert_eq!(file_stat, env.volume.lstat(&path).unwrap());
    }

    #[test]
    fn symlink() {
        let env = TestEnv::new();
        env.create_file("testdir/testfile", "foo");
        check_symlink(&env.volume, "testlink", "testdir/testfile");
        check_symlink(&env.volume, "testlink2", "doesntexit");
        check_symlink(&env.volume, "testlink3", "/proc");
        check_symlink(&env.volume, "testlink4", "../foo");

        assert_eq!(
            env.volume.read_link("doesntexit").unwrap_err().value(),
            lx::ENOENT
        );
        assert_eq!(
            env.volume
                .read_link(Path::from_lx("testdir/testfile").unwrap())
                .unwrap_err()
                .value(),
            lx::EINVAL
        );
    }

    #[test]
    fn unlink() {
        let env = TestEnv::new();
        env.create_file("testfile", "test");
        env.volume
            .mkdir("testdir", LxCreateOptions::new(0o777, 0, 0))
            .unwrap();

        env.volume
            .symlink("testlink", "testfile", LxCreateOptions::new(0, 0, 0))
            .unwrap();

        // Symlink to a directory, which on Windows will be a directory but should be treated by
        // unlink as a file.
        // N.B. This only tests the right thing if developer mode is on, otherwise it creates an LX symlink.
        env.volume
            .symlink("testlink2", "testdir", LxCreateOptions::new(0, 0, 0))
            .unwrap();

        check_unlink(&env.volume, "testfile", false);
        check_unlink(&env.volume, "testdir", true);
        check_unlink(&env.volume, "testlink", false);
        check_unlink(&env.volume, "testlink2", false);

        env.volume
            .open(
                "readonly",
                lx::O_RDONLY | lx::O_CREAT | lx::O_EXCL,
                Some(LxCreateOptions::new(0o444, 0, 0)),
            )
            .unwrap();
        check_unlink(&env.volume, "readonly", false);
    }

    #[test]
    fn set_attr() {
        let env = TestEnv::new();
        env.create_file("testfile", "test");
        let stat = env.volume.lstat("testfile").unwrap();
        assert_eq!(stat.file_size, 4);

        // Truncate.
        let mut attr = SetAttributes::default();
        attr.size = Some(2);
        env.volume.set_attr("testfile", attr).unwrap();
        let stat = env.volume.lstat("testfile").unwrap();
        assert_eq!(stat.file_size, 2);

        // Chmod (read-only attribute)
        let mut attr = SetAttributes::default();
        attr.mode = Some(lx::S_IFREG | 0o444);
        env.volume.set_attr("testfile", attr).unwrap();
        let stat = env.volume.lstat("testfile").unwrap();
        assert_eq!(stat.mode & 0o222, 0);
        attr.mode = Some(lx::S_IFREG | 0o666);
        env.volume.set_attr("testfile", attr).unwrap();
        let stat = env.volume.lstat("testfile").unwrap();
        assert_eq!(stat.mode & 0o222, 0o222);

        // Chown (silent succeed on Windows; skip on Linux since we may not have permission)
        if cfg!(windows) {
            let mut attr = SetAttributes::default();
            attr.uid = Some(1000);
            attr.gid = Some(1000);
            env.volume.set_attr("testfile", attr).unwrap();
        }

        // Set times, and test set_and_get_attr()
        let mut attr = SetAttributes::default();
        attr.atime = SetTime::Set(Duration::new(111111, 222200));
        attr.mtime = SetTime::Set(Duration::new(333333, 444400));
        let stat = env.volume.set_attr_stat("testfile", attr).unwrap();
        assert_eq!(stat.access_time.seconds, 111111);
        assert_eq!(stat.access_time.nanoseconds, 222200);
        assert_eq!(stat.write_time.seconds, 333333);
        assert_eq!(stat.write_time.nanoseconds, 444400);
    }

    #[test]
    fn file_set_attr() {
        let env = TestEnv::new();
        env.create_file("testfile", "test");

        let file = env.volume.open("testfile", lx::O_RDWR, None).unwrap();
        let stat = file.fstat().unwrap();
        assert_eq!(stat.file_size, 4);

        // Truncate.
        let mut attr = SetAttributes::default();
        attr.size = Some(2);
        file.set_attr(attr).unwrap();
        let stat = file.fstat().unwrap();
        assert_eq!(stat.file_size, 2);

        // Chmod (read-only attribute)
        let mut attr = SetAttributes::default();
        attr.mode = Some(lx::S_IFREG | 0o444);
        file.set_attr(attr).unwrap();
        let stat = file.fstat().unwrap();
        assert_eq!(stat.mode & 0o222, 0);
        attr.mode = Some(lx::S_IFREG | 0o666);
        file.set_attr(attr).unwrap();
        let stat = file.fstat().unwrap();
        assert_eq!(stat.mode & 0o222, 0o222);

        // Chown (silent succeed on Windows; skip on Linux since we may not have permission)
        if cfg!(windows) {
            let mut attr = SetAttributes::default();
            attr.uid = Some(1000);
            attr.gid = Some(1000);
            file.set_attr(attr).unwrap();
        }

        // Set times
        let mut attr = SetAttributes::default();
        attr.atime = SetTime::Set(Duration::new(111111, 222200));
        attr.mtime = SetTime::Set(Duration::new(333333, 444400));
        file.set_attr(attr).unwrap();
        let stat = file.fstat().unwrap();
        assert_eq!(stat.access_time.seconds, 111111);
        assert_eq!(stat.access_time.nanoseconds, 222200);
        assert_eq!(stat.write_time.seconds, 333333);
        assert_eq!(stat.write_time.nanoseconds, 444400);
    }

    #[test]
    fn kill_priv() {
        let env = TestEnv::with_options(LxVolumeOptions::new().metadata(true));
        let file = env
            .volume
            .open(
                "testfile",
                lx::O_RDWR | lx::O_CREAT | lx::O_EXCL,
                Some(LxCreateOptions::new(
                    lx::S_ISUID | lx::S_ISGID | 0o777,
                    1000,
                    2000,
                )),
            )
            .unwrap();

        let stat = file.fstat().unwrap();
        assert_eq!(stat.mode as u32, lx::S_IFREG | 0o6777);

        let write_result = if cfg!(windows) || !is_lx_root() {
            lx::S_IFREG | 0o777
        } else {
            lx::S_IFREG | 0o6777
        };

        // Write clears it (except for root).
        file.pwrite(b"hello", 0, 1000).unwrap();
        let stat = file.fstat().unwrap();
        assert_eq!(stat.mode as u32, write_result);
        if cfg!(windows) {
            // Write does not clear it for root.
            file.chmod(lx::S_IFREG | 0o6777).unwrap();
            let stat = file.fstat().unwrap();
            assert_eq!(stat.mode as u32, lx::S_IFREG | 0o6777);
            file.pwrite(b"hello", 0, 0).unwrap();
            let stat = file.fstat().unwrap();
            assert_eq!(stat.mode as u32, lx::S_IFREG | 0o6777);
        }

        file.chmod(lx::S_IFREG | 0o6777).unwrap();
        let stat = file.fstat().unwrap();
        assert_eq!(stat.mode as u32, lx::S_IFREG | 0o6777);

        // Truncate clears it (except for root).
        file.truncate(2, 1000).unwrap();
        let stat = file.fstat().unwrap();
        assert_eq!(stat.file_size, 2);
        assert_eq!(stat.mode as u32, write_result);
        if cfg!(windows) {
            // Truncate does not clear it as root.
            file.chmod(lx::S_IFREG | 0o6777).unwrap();
            let stat = file.fstat().unwrap();
            assert_eq!(stat.mode as u32, lx::S_IFREG | 0o6777);
            file.truncate(2, 0).unwrap();
            let stat = file.fstat().unwrap();
            assert_eq!(stat.file_size, 2);
            assert_eq!(stat.mode as u32, lx::S_IFREG | 0o6777);
        }

        file.chmod(lx::S_IFREG | 0o6777).unwrap();
        let stat = file.fstat().unwrap();
        assert_eq!(stat.mode as u32, lx::S_IFREG | 0o6777);

        // Chown no changes does not clear it.
        let stat = file.fstat().unwrap();
        file.chown(None, None).unwrap();
        assert_eq!(stat.mode as u32, lx::S_IFREG | 0o6777);

        // Chown clears it.
        // N.B. Only perform this test if we have permissions to do so.
        if is_lx_root() {
            file.chown(Some(1001), Some(2001)).unwrap();
            let stat = file.fstat().unwrap();
            assert_eq!(stat.uid, 1001);
            assert_eq!(stat.gid, 2001);
            assert_eq!(stat.mode as u32, lx::S_IFREG | 0o777);

            // Chown doesn't clear setgid if not group executable.
            file.chmod(lx::S_IFREG | 0o6767).unwrap();
            let stat = file.fstat().unwrap();
            assert_eq!(stat.mode as u32, lx::S_IFREG | 0o6767);
            file.chown(Some(1001), Some(2001)).unwrap();
            let stat = file.fstat().unwrap();
            assert_eq!(stat.uid, 1001);
            assert_eq!(stat.gid, 2001);
            assert_eq!(stat.mode as u32, lx::S_IFREG | 0o2767);
        }
    }

    #[test]
    fn mknod() {
        let env = TestEnv::new();
        env.create_file("mknod", "test");

        // Test without metadata, so on Windows only regular files will work, and mode/uid/gid are
        // not used.
        env.volume
            .mknod(
                "testfile",
                LxCreateOptions::new(lx::S_IFREG | 0o640, 1000, 2000),
                0,
            )
            .unwrap();

        let stat = env.volume.lstat("testfile").unwrap();
        assert!(lx::s_isreg(stat.mode));
        assert_eq!(stat.file_size, 0);

        let stat = env
            .volume
            .mknod_stat(
                "testfile2",
                LxCreateOptions::new(lx::S_IFREG | 0o640, 1000, 2000),
                0,
            )
            .unwrap();

        let stat2 = env.volume.lstat("testfile2").unwrap();
        assert_eq!(stat, stat2);
    }

    #[test]
    fn rename() {
        let env = TestEnv::new();
        env.create_file("testfile", "test");
        let stat = env.volume.lstat("testfile").unwrap();

        // Rename to a new name.
        env.volume.rename("testfile", "testfile2", 0).unwrap();
        let stat2 = env.volume.lstat("testfile2").unwrap();
        assert_eq!(stat.inode_nr, stat2.inode_nr);
        let err = env.volume.lstat("testfile").unwrap_err();
        assert_eq!(err.value(), lx::ENOENT);

        // Into a directory.
        env.volume
            .mkdir("testdir", LxCreateOptions::new(0o755, 0, 0))
            .unwrap();

        env.volume
            .rename("testfile2", Path::from_lx("testdir/testfile").unwrap(), 0)
            .unwrap();

        let stat2 = env
            .volume
            .lstat(Path::from_lx("testdir/testfile").unwrap())
            .unwrap();

        assert_eq!(stat.inode_nr, stat2.inode_nr);
        let err = env.volume.lstat("testfile2").unwrap_err();
        assert_eq!(err.value(), lx::ENOENT);

        // Dir over dir, not empty.
        env.volume
            .mkdir("testdir2", LxCreateOptions::new(0o755, 0, 0))
            .unwrap();

        let dirstat = env.volume.lstat("testdir").unwrap();
        let dirstat2 = env.volume.lstat("testdir2").unwrap();
        assert_ne!(dirstat.inode_nr, dirstat2.inode_nr);
        let err = env.volume.rename("testdir2", "testdir", 0).unwrap_err();
        assert_eq!(err.value(), lx::ENOTEMPTY);

        // File over file.
        env.create_file("testfile3", "foo");
        let stat2 = env.volume.lstat("testfile3").unwrap();
        assert_ne!(stat2.inode_nr, stat.inode_nr);
        env.volume
            .rename(Path::from_lx("testdir/testfile").unwrap(), "testfile3", 0)
            .unwrap();

        let stat2 = env.volume.lstat("testfile3").unwrap();
        assert_eq!(stat.inode_nr, stat2.inode_nr);
        let err = env
            .volume
            .lstat(Path::from_lx("testdir/testfile").unwrap())
            .unwrap_err();

        assert_eq!(err.value(), lx::ENOENT);

        // Dir over dir.
        env.volume.rename("testdir2", "testdir", 0).unwrap();
        let dirstat = env.volume.lstat("testdir").unwrap();
        assert_eq!(dirstat.inode_nr, dirstat2.inode_nr);
        let err = env.volume.lstat("testdir2").unwrap_err();
        assert_eq!(err.value(), lx::ENOENT);

        // File over dir.
        let err = env.volume.rename("testfile3", "testdir", 0).unwrap_err();
        assert_eq!(err.value(), lx::EISDIR);

        // Dir over file.
        let err = env.volume.rename("testdir", "testfile3", 0).unwrap_err();
        assert_eq!(err.value(), lx::ENOTDIR);

        // Scope exit to unlink these files explicitly since they're read-only, which doesn't work
        // with TestEnv's drop method.
        let _exit = pal::ScopeExit::new(|| {
            env.volume.unlink("testfile4", 0).unwrap_or_default();
            env.volume.unlink("testfile5", 0).unwrap_or_default();
        });

        // Readonly file over file.
        env.volume
            .mknod(
                "testfile4",
                LxCreateOptions::new(lx::S_IFREG | 0o444, 0, 0),
                0,
            )
            .unwrap();

        env.volume
            .mknod(
                "testfile5",
                LxCreateOptions::new(lx::S_IFREG | 0o444, 0, 0),
                0,
            )
            .unwrap();

        env.volume.rename("testfile4", "testfile5", 0).unwrap();

        // Rename changing only the case.
        let dirstat = env.volume.lstat("testdir").unwrap();
        env.volume.rename("testdir", "TestDir", 0).unwrap();
        let dirstat2 = env.volume.lstat("TestDir").unwrap();
        assert_eq!(dirstat.inode_nr, dirstat2.inode_nr);
    }

    #[test]
    fn link() {
        let env = TestEnv::new();
        env.create_file("testfile", "test");
        let stat = env.volume.lstat("testfile").unwrap();
        assert_eq!(stat.link_count, 1);

        env.volume.link("testfile", "testfile2").unwrap();
        let stat2 = env.volume.lstat("testfile2").unwrap();
        assert_eq!(stat2.inode_nr, stat.inode_nr);
        assert_eq!(stat2.link_count, 2);

        let stat2 = env.volume.link_stat("testfile", "testfile3").unwrap();
        assert_eq!(stat2.inode_nr, stat.inode_nr);
        assert_eq!(stat2.link_count, 3);
    }

    #[test]
    fn stat_fs() {
        let env = TestEnv::new();
        let stat_fs = env.volume.stat_fs("").unwrap();
        let stat = env.volume.lstat("").unwrap();
        assert_eq!(stat_fs.block_size, stat.block_size as usize);
    }

    #[test]
    fn fsync() {
        let env = TestEnv::new();
        {
            let file = env
                .volume
                .open(
                    "testfile",
                    lx::O_WRONLY | lx::O_CREAT,
                    Some(LxCreateOptions::new(0o666, 0, 0)),
                )
                .unwrap();

            file.pwrite(b"test", 0, 0).unwrap();
            file.fsync(false).unwrap();
            file.fsync(true).unwrap();
        }

        // Ensure no error is returned for read-only files.
        let file = env.volume.open("testfile", lx::O_RDONLY, None).unwrap();
        file.fsync(false).unwrap();
        file.fsync(true).unwrap();
    }

    #[test]
    fn xattr() {
        let env = TestEnv::new();
        env.create_file("testfile", "test");

        // No attributes to start with.
        let err = env
            .volume
            .get_xattr("testfile", "user.test", None)
            .unwrap_err();

        assert_eq!(err.value(), lx::ENODATA);

        let size = env.volume.list_xattr("testfile", None).unwrap();
        assert_eq!(size, 0);

        let err = env
            .volume
            .remove_xattr("testfile", "user.test")
            .unwrap_err();

        assert_eq!(err.value(), lx::ENODATA);

        // Set an attribute and retrieve it.
        env.volume
            .set_xattr("testfile", "user.test", b"foo", 0)
            .unwrap();

        let size = env.volume.get_xattr("testfile", "user.test", None).unwrap();

        assert_eq!(size, 3);
        let mut buffer = [0u8; 1024];
        let size = env
            .volume
            .get_xattr("testfile", "user.test", Some(&mut buffer))
            .unwrap();

        assert_eq!(size, 3);
        assert_eq!(&buffer[..3], b"foo");

        // Set an empty attribute and retrieve it.
        env.volume
            .set_xattr("testfile", "user.empty", b"", 0)
            .unwrap();

        let size = env
            .volume
            .get_xattr("testfile", "user.empty", None)
            .unwrap();

        assert_eq!(size, 0);

        // List the attributes.
        let size = env.volume.list_xattr("testfile", None).unwrap();
        assert_eq!(size, 21);
        let size = env
            .volume
            .list_xattr("testfile", Some(&mut buffer))
            .unwrap();

        assert_eq!(size, 21);
        assert_eq!(&buffer[..21], b"user.test\0user.empty\0");

        // Remove an attribute.
        env.volume.remove_xattr("testfile", "user.empty").unwrap();

        let err = env
            .volume
            .get_xattr("testfile", "user.empty", None)
            .unwrap_err();

        assert_eq!(err.value(), lx::ENODATA);
        let size = env
            .volume
            .list_xattr("testfile", Some(&mut buffer))
            .unwrap();

        assert_eq!(size, 10);
        assert_eq!(&buffer[..10], b"user.test\0");

        // Test flags.
        let err = env
            .volume
            .set_xattr("testfile", "user.test", b"bar", lx::XATTR_CREATE)
            .unwrap_err();

        assert_eq!(err.value(), lx::EEXIST);
        let size = env
            .volume
            .get_xattr("testfile", "user.test", Some(&mut buffer))
            .unwrap();

        assert_eq!(size, 3);
        assert_eq!(&buffer[..3], b"foo");
        env.volume
            .set_xattr("testfile", "user.test2", b"bar", lx::XATTR_CREATE)
            .unwrap();

        let size = env
            .volume
            .get_xattr("testfile", "user.test2", Some(&mut buffer))
            .unwrap();

        assert_eq!(size, 3);
        assert_eq!(&buffer[..3], b"bar");
        let err = env
            .volume
            .set_xattr("testfile", "user.test3", b"baz", lx::XATTR_REPLACE)
            .unwrap_err();

        assert_eq!(err.value(), lx::ENODATA);
        let err = env
            .volume
            .get_xattr("testfile", "user.test3", None)
            .unwrap_err();

        assert_eq!(err.value(), lx::ENODATA);
        env.volume
            .set_xattr("testfile", "user.test2", b"baz", lx::XATTR_REPLACE)
            .unwrap();

        let size = env
            .volume
            .get_xattr("testfile", "user.test2", Some(&mut buffer))
            .unwrap();

        assert_eq!(size, 3);
        assert_eq!(&buffer[..3], b"baz");
    }

    // This test is disabled in CI on Windows only, because it requires NTFS support for setting
    // the case sensitive directory attribute, which is only enabled if the WSL optional component
    // is installed.
    #[test]
    #[cfg(not(all(windows, feature = "ci")))]
    fn case_sensitive() {
        let env = TestEnv::with_options(LxVolumeOptions::new().create_case_sensitive_dirs(true));

        env.volume
            .mkdir("testdir", LxCreateOptions::new(0o777, 0, 0))
            .expect("Could not create case sensitive directory. This may indicate WSL needs to be installed.");

        env.volume
            .mknod(
                Path::from_lx("testdir/testfile").unwrap(),
                LxCreateOptions::new(lx::S_IFREG | 0o666, 0, 0),
                0,
            )
            .unwrap();

        env.volume
            .mknod(
                Path::from_lx("testdir/TESTFILE").unwrap(),
                LxCreateOptions::new(lx::S_IFREG | 0o666, 0, 0),
                0,
            )
            .unwrap();

        let stat1 = env
            .volume
            .lstat(Path::from_lx("testdir/testfile").unwrap())
            .unwrap();

        let stat2 = env
            .volume
            .lstat(Path::from_lx("testdir/TESTFILE").unwrap())
            .unwrap();

        assert_ne!(stat1.inode_nr, stat2.inode_nr);
    }

    #[test]
    #[cfg(windows)]
    fn case_insensitive() {
        let env = TestEnv::new();
        env.volume
            .mkdir("testdir", LxCreateOptions::new(0o777, 0, 0))
            .unwrap();

        env.volume
            .mknod(
                Path::from_lx("testdir/testfile").unwrap(),
                LxCreateOptions::new(lx::S_IFREG | 0o666, 0, 0),
                0,
            )
            .unwrap();

        let err = env
            .volume
            .mknod(
                Path::from_lx("testdir/TESTFILE").unwrap(),
                LxCreateOptions::new(lx::S_IFREG | 0o666, 0, 0),
                0,
            )
            .unwrap_err();

        assert_eq!(err.value(), lx::EEXIST);
    }

    // This test is disabled in CI, because it requires NTFS support for setting the case sensitive
    // directory attribute, which is only enabled if the WSL optional component is installed.
    #[test]
    #[cfg(all(windows, not(feature = "ci")))]
    fn case_sensitive_dir_xattr() {
        let env = TestEnv::new();
        env.volume
            .mkdir("testdir", LxCreateOptions::new(0o777, 0, 0))
            .unwrap();

        let size = env.volume.list_xattr("testdir", None).unwrap();
        assert_eq!(size, b"system.wsl_case_sensitive\0".len());

        let mut buffer = [0u8; 1024];
        let size = env.volume.list_xattr("testdir", Some(&mut buffer)).unwrap();
        assert_eq!(&buffer[..size], b"system.wsl_case_sensitive\0");

        let size = env
            .volume
            .get_xattr("testdir", "system.wsl_case_sensitive", Some(&mut buffer))
            .unwrap();

        assert_eq!(&buffer[..size], b"0");

        env.volume
            .set_xattr("testdir", "system.wsl_case_sensitive", b"1", 0)
            .expect("Could not create case sensitive directory. This may indicate WSL needs to be installed.");

        let size = env
            .volume
            .get_xattr("testdir", "system.wsl_case_sensitive", Some(&mut buffer))
            .unwrap();

        assert_eq!(&buffer[..size], b"1");

        env.volume
            .mknod(
                Path::from_lx("testdir/testfile").unwrap(),
                LxCreateOptions::new(lx::S_IFREG | 0o666, 0, 0),
                0,
            )
            .unwrap();

        env.volume
            .mknod(
                Path::from_lx("testdir/TESTFILE").unwrap(),
                LxCreateOptions::new(lx::S_IFREG | 0o666, 0, 0),
                0,
            )
            .unwrap();

        let stat1 = env
            .volume
            .lstat(Path::from_lx("testdir/testfile").unwrap())
            .unwrap();

        let stat2 = env
            .volume
            .lstat(Path::from_lx("testdir/TESTFILE").unwrap())
            .unwrap();

        assert_ne!(stat1.inode_nr, stat2.inode_nr);
    }

    fn check_symlink(volume: &LxVolume, path: impl AsRef<Path>, target: &str) {
        volume
            .symlink(&path, target, LxCreateOptions::new(0, 0, 0))
            .unwrap();

        let stat = volume.lstat(&path).unwrap();
        assert_eq!(stat.mode, lx::S_IFLNK | 0o777);
        assert_eq!(stat.file_size, target.len() as u64);
        assert_eq!(volume.read_link(&path).unwrap(), target);
    }

    fn check_unlink(volume: &LxVolume, path: impl AsRef<Path>, dir: bool) {
        let (good_flags, bad_flags, error) = if dir {
            (lx::AT_REMOVEDIR, 0, lx::EISDIR)
        } else {
            (0, lx::AT_REMOVEDIR, lx::ENOTDIR)
        };

        assert_eq!(volume.unlink(&path, bad_flags).unwrap_err().value(), error);
        volume.lstat(&path).unwrap();
        volume.unlink(&path, good_flags).unwrap();
        assert_eq!(volume.lstat(&path).unwrap_err().value(), lx::ENOENT);
    }

    #[cfg(unix)]
    fn is_lx_root() -> bool {
        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe { libc::getuid() == 0 }
    }

    #[cfg(windows)]
    fn is_lx_root() -> bool {
        true
    }

    struct TestEnv {
        volume: LxVolume,
        root_dir: TempDir,
    }

    impl TestEnv {
        fn new() -> Self {
            Self::with_options(&LxVolumeOptions::new())
        }

        fn with_options(options: &LxVolumeOptions) -> Self {
            Self::clear_umask();
            let root_dir = tempfile::tempdir().unwrap();
            let volume = options.new_volume(root_dir.path()).unwrap();
            Self { volume, root_dir }
        }

        fn create_file(&self, name: &str, contents: &str) {
            let mut path: PathBuf = self.root_dir.path().into();
            path.push(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        fn check_dir_entry(&self, entry: &lx::DirEntry) {
            // Since seeking to an offset gives you the next entry after that offset, no entry
            // should ever have an offset of zero.
            assert_ne!(entry.offset, 0);

            let name = entry.name.to_str().unwrap();
            if name == "." || name == ".." {
                // On Windows, the inode number is not set for . and .. entries, so don't check it.
                assert_eq!(entry.file_type, lx::DT_DIR);
            } else {
                let stat = self.volume.lstat(name).unwrap();
                assert_eq!(entry.inode_nr, stat.inode_nr);
                assert_eq!((entry.file_type as u32) << 12, stat.mode & lx::S_IFMT);
            }
        }

        #[cfg(unix)]
        fn clear_umask() {
            // SAFETY: Calling C API as documented, with no special requirements.
            unsafe { libc::umask(0) };
        }

        #[cfg(windows)]
        fn clear_umask() {}
    }
}
