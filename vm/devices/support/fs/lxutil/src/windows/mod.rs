// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Calling Windows API.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

mod macros;

mod fs;
pub(crate) mod path;
mod readdir;
mod symlink;
mod util;
mod xattr;

use super::PathExt;
use super::SetAttributes;
use parking_lot::Mutex;
use std::ffi;
use std::os::windows::prelude::*;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use windows::Wdk::Storage::FileSystem;
use windows::Wdk::System::SystemServices;
use windows::Win32::Foundation;
use windows::Win32::Storage::FileSystem as W32Fs;
use windows::Win32::System::SystemServices as W32Ss;
use windows::Win32::System::WindowsProgramming;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const DOT_ENTRY_COUNT: lx::off_t = 2;

// State that LxVolume can share with LxFiles.
pub struct VolumeState {
    fs_context: fs::FsContext,
    options: super::LxVolumeOptions,
    block_size: u32,
}

impl VolumeState {
    pub fn new(
        fs_context: fs::FsContext,
        options: super::LxVolumeOptions,
        block_size: u32,
    ) -> Arc<Self> {
        Arc::new(VolumeState {
            fs_context,
            options,
            block_size,
        })
    }

    pub fn get_attributes_by_handle(
        &self,
        handle: &OwnedHandle,
    ) -> lx::Result<util::LxStatInformation> {
        util::get_attributes_by_handle(&self.fs_context, self, handle)
    }

    pub fn get_attributes(
        &self,
        root_handle: Option<&OwnedHandle>,
        path: &Path,
        existing_handle: Option<&OwnedHandle>,
    ) -> lx::Result<util::LxStatInformation> {
        util::get_attributes(&self.fs_context, self, root_handle, path, existing_handle)
    }

    pub fn read_reparse_link(&self, handle: &OwnedHandle) -> lx::Result<Option<lx::LxString>> {
        fs::read_reparse_link(handle, self)
    }
}

// Windows implementation of LxVolume.
// See crate::LxVolume for more detailed comments.
pub struct LxVolume {
    root: OwnedHandle,
    root_path: PathBuf,
    state: Arc<VolumeState>,
}

impl LxVolume {
    pub fn new(root_path: &Path, options: &super::LxVolumeOptions) -> lx::Result<Self> {
        // Open a handle to the root.
        let (root, _) = util::open_relative_file(
            None,
            root_path,
            util::MINIMUM_PERMISSIONS,
            FileSystem::FILE_OPEN,
            util::OpenOptions {
                file_attributes: W32Fs::FILE_ATTRIBUTE_DIRECTORY,
                create_options: FileSystem::FILE_DIRECTORY_FILE,
                ..Default::default()
            },
        )?;

        // Determine the capabilities of the file system.
        let fs_context = fs::FsContext::new(&root, fs::LX_DRVFS_DISABLE_NONE, false)?;

        let mut options = options.clone();
        if !fs_context.compatibility_flags.supports_metadata() {
            options.metadata = false;
        }

        if !fs_context.compatibility_flags.supports_case_sensitive_dir() {
            options.create_case_sensitive_dirs = false;
        }

        // Determine the block size for use in stat calls.
        // N.B. If this volume contains more than one file system, this value could be wrong for
        //      some queries. However, this is not the intended use of this class.
        let block_size = fs::get_fs_block_size(&root);
        Ok(Self {
            root,
            #[expect(clippy::disallowed_methods, reason = "need actual canonical path here")]
            root_path: root_path.canonicalize()?,
            state: VolumeState::new(fs_context, options, block_size),
        })
    }

    pub fn supports_stable_file_id(&self) -> bool {
        self.state
            .fs_context
            .compatibility_flags
            .supports_stable_file_id()
    }

    fn check_sandbox_enforcement(&self, path: &Path) -> lx::Result<()> {
        if !self.state.options.sandbox {
            return Ok(());
        }
        let mut cur_path = PathBuf::new();
        for next_path in path.components() {
            let next_path = match next_path {
                Component::Normal(next) => next,
                _ => return Err(lx::Error::EINVAL),
            };

            if util::is_dos_device_name(next_path)? {
                return Err(lx::Error::EACCES);
            }

            if let Some(extension) = Path::new(next_path).extension() {
                let ext = extension.to_ascii_lowercase();
                if self
                    .state
                    .options
                    .sandbox_disallowed_extensions
                    .contains(&ext)
                {
                    return Err(lx::Error::EACCES);
                }
            }

            cur_path = cur_path.join(next_path);

            let information = match self.state.get_attributes(
                Some(&self.root),
                cur_path.as_path(),
                None,
            ) {
                Ok(info) => info.stat,
                Err(err) => {
                    // Non-existent paths are not filtered
                    if err == lx::Error::ENOENT {
                        return Ok(());
                    }
                    // Otherwise couldn't get attributes, so safest recourse is to fail
                    tracing::warn!(path = %path.display(), ?err, "Unable to get attributes; denying access");
                    return Err(lx::Error::EACCES);
                }
            };
            self.check_sandbox_attributes(information.FileAttributes)?;
        }

        Ok(())
    }

    fn check_sandbox_attributes(&self, attributes: u32) -> lx::Result<()> {
        if self.state.options.sandbox
            && attributes
                & (W32Fs::FILE_ATTRIBUTE_HIDDEN
                    | W32Fs::FILE_ATTRIBUTE_SYSTEM
                    | W32Fs::FILE_ATTRIBUTE_OFFLINE
                    | W32Fs::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
                    .0
                != 0
        {
            return Err(lx::Error::EACCES);
        }

        Ok(())
    }

    pub fn lstat(&self, path: &Path) -> lx::Result<lx::StatEx> {
        assert!(path.is_relative());

        // Special-case returning attributes of the root itself to just use the existing handle.
        let mut info = if path.as_os_str().is_empty() {
            self.state.get_attributes_by_handle(&self.root)?
        } else {
            self.check_sandbox_enforcement(path)?;
            self.state.get_attributes(Some(&self.root), path, None)?
        };

        util::file_info_to_stat(
            &self.state.fs_context,
            &mut info,
            &self.state.options,
            self.state.block_size,
        )
    }

    pub fn set_attr(&self, path: &Path, attr: SetAttributes) -> lx::Result<()> {
        self.set_attr_helper(path, attr, false)?;
        Ok(())
    }

    pub fn set_attr_stat(&self, path: &Path, attr: SetAttributes) -> lx::Result<lx::Stat> {
        Ok(self.set_attr_helper(path, attr, true)?.unwrap())
    }

    pub fn open(
        &self,
        path: &Path,
        flags: i32,
        mut options: Option<super::LxCreateOptions>,
    ) -> lx::Result<LxFile> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;
        // Convert flags to relevant NT options.
        let desired_access = util::open_flags_to_access_mask(flags);
        let (file_attributes, create_options) = util::open_flags_to_attributes_options(flags);
        let disposition = util::open_flags_to_disposition(flags);

        // This function creates a regular file and ignores the file type in the specified mode.
        if let Some(opt) = options.as_mut() {
            opt.mode = lx::S_IFREG | (opt.mode & 0o7777);
        }

        // Try to open/create the file.
        let (file, create_result) = self.create_file(
            path,
            desired_access,
            disposition,
            file_attributes,
            create_options,
            options,
            0,
        )?;

        // O_TRUNC can't be handled with FILE_OVERWRITE because that clears metadata, so handle
        // it here.
        if flags & lx::O_TRUNC != 0 && create_result != WindowsProgramming::FILE_CREATED as usize {
            fs::truncate(&file, 0)?;
        }

        let is_app_exec_alias = match self.state.get_attributes_by_handle(&file) {
            Ok(info) => info.is_app_execution_alias,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to get attributes for newly opened file");
                false
            }
        };
        Ok(LxFile {
            handle: file,
            state: Arc::clone(&self.state),
            enumerator: None,
            access: desired_access,
            kill_priv: AtomicBool::new(true),
            is_app_exec_alias: Mutex::new(is_app_exec_alias),
        })
    }

    pub fn mkdir(&self, path: &Path, options: super::LxCreateOptions) -> lx::Result<()> {
        self.mkdir_helper(path, options)?;
        Ok(())
    }

    pub fn mkdir_stat(&self, path: &Path, options: super::LxCreateOptions) -> lx::Result<lx::Stat> {
        let handle = self.mkdir_helper(path, options)?;
        self.get_stat_by_handle(&handle)
    }

    pub fn symlink(
        &self,
        path: &Path,
        target: &lx::LxStr,
        options: super::LxCreateOptions,
    ) -> lx::Result<()> {
        self.symlink_helper(path, target, options)?;
        Ok(())
    }

    pub fn symlink_stat(
        &self,
        path: &Path,
        target: &lx::LxStr,
        options: super::LxCreateOptions,
    ) -> lx::Result<lx::Stat> {
        let handle = self.symlink_helper(path, target, options)?;
        self.get_stat_by_handle(&handle)
    }

    pub fn read_link(&self, path: &Path) -> lx::Result<lx::LxString> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;

        // Open the file to read its reparse data.
        let mut handle = self.open_file(path, W32Fs::FILE_READ_ATTRIBUTES, Default::default())?;
        let target_string = self.state.read_reparse_link(&handle)?;

        // If the function succeeded but returned None, this is a V1 LX symlink which must be
        // opened for read to read the target.
        // N.B. The initial open above does not use FILE_READ_DATA because some symlinks (notably the
        //      back-compat symlinks that Windows creates for things like "C:\Documents and Settings")
        //      deny this permission.
        let target = if let Some(target_string) = target_string {
            target_string
        } else {
            handle =
                util::reopen_file(&handle, W32Fs::FILE_READ_ATTRIBUTES | W32Fs::FILE_READ_DATA)?;
            symlink::read(&handle)?
        };

        Ok(target)
    }

    pub fn unlink(&self, path: &Path, flags: i32) -> lx::Result<()> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;

        // Directories can be pre-filtered with FILE_DIRECTORY_FILE. We cannot use
        // FILE_NON_DIRECTORY_FILE, because without AT_REMOVEDIR this function still needs to
        // remove NT symlinks which are directories.
        let mut create_options = FileSystem::FILE_OPEN_REPARSE_POINT;
        if flags & lx::AT_REMOVEDIR != 0 {
            create_options |= FileSystem::FILE_DIRECTORY_FILE;
        }

        let handle = self.open_file(
            path,
            W32Fs::DELETE | W32Fs::FILE_READ_ATTRIBUTES,
            create_options,
        )?;

        // Query file attributes to check if it matches what AT_REMOVEDIR wants.
        let info: FileSystem::FILE_BASIC_INFORMATION = util::query_information_file(&handle)?;
        if flags & lx::AT_REMOVEDIR != 0 {
            // Must be a directory and not a reparse point (NT directory symlinks are treated
            // as files).
            if info.FileAttributes & W32Fs::FILE_ATTRIBUTE_DIRECTORY.0 == 0
                || info.FileAttributes & W32Fs::FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            {
                return Err(lx::Error::ENOTDIR);
            }
        } else {
            // Must be a regular file, or a reparse point directory.
            if info.FileAttributes & W32Fs::FILE_ATTRIBUTE_DIRECTORY.0 != 0
                && info.FileAttributes & W32Fs::FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
            {
                return Err(lx::Error::EISDIR);
            }
        }

        self.delete_file(&handle)
    }

    pub fn mknod(
        &self,
        path: &Path,
        options: super::LxCreateOptions,
        device_id: lx::dev_t,
    ) -> lx::Result<()> {
        self.mknod_helper(path, options, device_id)?;
        Ok(())
    }

    pub fn mknod_stat(
        &self,
        path: &Path,
        options: super::LxCreateOptions,
        device_id: lx::dev_t,
    ) -> lx::Result<lx::Stat> {
        let handle = self.mknod_helper(path, options, device_id)?;
        self.get_stat_by_handle(&handle)
    }

    pub fn rename(&self, path: &Path, new_path: &Path, flags: u32) -> lx::Result<()> {
        assert!(path.is_relative());
        assert!(new_path.is_relative());
        self.check_sandbox_enforcement(path)?;
        self.check_sandbox_enforcement(new_path)?;

        // Currently, no flags are supported.
        // TODO: RENAME_NOREPLACE could be trivially supported. The other flags would require NT
        //       changes to be atomic.
        if flags != 0 {
            return Err(lx::Error::EINVAL);
        }

        let handle = self.open_file(
            path,
            W32Fs::FILE_READ_ATTRIBUTES | W32Fs::DELETE,
            Default::default(),
        )?;
        let parent = self.open_parent(new_path)?;
        let (target_parent, target_path) = match &parent {
            Some((parent, name)) => (parent, *name),
            None => (&self.root, new_path),
        };
        self.rename_file(path, new_path, &handle, target_parent, target_path)
    }

    fn rename_file(
        &self,
        path: &Path,
        new_path: &Path,
        handle: &OwnedHandle,
        target_parent: &OwnedHandle,
        target_path: &Path,
    ) -> lx::Result<()> {
        let supports_posix_rename = self
            .state
            .fs_context
            .compatibility_flags
            .supports_posix_unlink_rename();
        let source_is_dir = if supports_posix_rename {
            None
        } else {
            let info: FileSystem::FILE_BASIC_INFORMATION = util::query_information_file(handle)?;
            Some(info.FileAttributes & W32Fs::FILE_ATTRIBUTE_DIRECTORY.0 != 0)
        };
        let open_target = |desired_access| -> lx::Result<OwnedHandle> {
            let (target, _) = util::open_relative_file(
                Some(target_parent),
                target_path,
                desired_access,
                FileSystem::FILE_OPEN,
                util::OpenOptions {
                    create_options: FileSystem::FILE_OPEN_REPARSE_POINT,
                    ..Default::default()
                },
            )?;
            if self.state.options.sandbox || source_is_dir.is_some() {
                let info: FileSystem::FILE_BASIC_INFORMATION =
                    util::query_information_file(&target)?;
                self.check_sandbox_attributes(info.FileAttributes)?;
                if let Some(source_is_dir) = source_is_dir {
                    let target_is_dir =
                        info.FileAttributes & W32Fs::FILE_ATTRIBUTE_DIRECTORY.0 != 0;
                    if source_is_dir && !target_is_dir {
                        return Err(lx::Error::ENOTDIR);
                    }
                    if !source_is_dir && target_is_dir {
                        return Err(lx::Error::EISDIR);
                    }
                }
            }
            Ok(target)
        };

        // Emulate Linux rename semantics that NTFS enforces natively but FAT/exFAT does not.
        if let Some(source_is_dir) = source_is_dir {
            // Reject renaming a directory into its own descendant (EINVAL).
            if source_is_dir && new_path != path && new_path.starts_with(path) {
                return Err(lx::Error::EINVAL);
            }

            // Reject cross-type replacement: file-over-directory (ENOTDIR) or directory-over-file (EISDIR).
            match open_target(W32Fs::FILE_READ_ATTRIBUTES) {
                Ok(_) => {}
                // A missing target is fine; the rename creates it.
                Err(err) if err.value() == lx::ENOENT => {}
                Err(err) => return Err(err),
            }
        }

        let flags = fs::RenameFlags::default();
        let error = match fs::rename(
            handle,
            target_parent,
            target_path,
            &self.state.fs_context,
            flags,
        ) {
            Ok(_) => return Ok(()),
            Err(error) => error,
        };

        // If the rename failed and POSIX rename is not supported, the target may need to be removed
        // beforehand in case it's a directory (superseding rename does not work in this case), or in
        // case it's read-only.
        // N.B. This MUST be done after attempting the rename because on non-case-sensitive file
        //      systems renaming foo to FOO would otherwise delete the original file.
        if (error.value() == lx::EPERM
            || error.value() == lx::EACCES
            || error.value() == lx::EEXIST)
            && !supports_posix_rename
        {
            match open_target(W32Fs::FILE_READ_ATTRIBUTES | W32Fs::DELETE) {
                Ok(target_handle) => self.delete_file(&target_handle)?,
                Err(err) => {
                    // ENOENT means the rename can proceed.
                    if err.value() != lx::ENOENT {
                        return Err(err);
                    }
                }
            }

            // Retry the rename.
            fs::rename(
                handle,
                target_parent,
                target_path,
                &self.state.fs_context,
                flags,
            )?;
        } else {
            return Err(error);
        }

        Ok(())
    }

    pub fn link(&self, path: &Path, new_path: &Path) -> lx::Result<()> {
        self.link_helper(path, new_path)?;
        Ok(())
    }

    pub fn link_stat(&self, path: &Path, new_path: &Path) -> lx::Result<lx::Stat> {
        let handle = self.link_helper(path, new_path)?;
        self.get_stat_by_handle(&handle)
    }

    pub fn stat_fs(&self, path: &Path) -> lx::Result<lx::StatFs> {
        assert!(path.is_relative());
        let handle = self.open_file(path, W32Fs::FILE_READ_ATTRIBUTES, Default::default())?;
        fs::get_lx_file_system_attributes(&handle, 0)
    }

    pub fn set_xattr(
        &self,
        path: &Path,
        name: &lx::LxStr,
        value: &[u8],
        flags: i32,
    ) -> lx::Result<()> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;
        if !self.state.options.override_xattrs.is_empty() {
            tracing::info!(
                path = %path.display(),
                ?name,
                "Ignoring xattr write on volume with overridden xattrs"
            );
            return Ok(());
        }

        if !self.supports_xattr() {
            return Err(lx::Error::ENOTSUP);
        }

        let system = name.as_bytes().starts_with(b"system.");
        let name = name.to_str().ok_or(lx::Error::EINVAL)?;
        let desired_access = if system {
            W32Fs::FILE_WRITE_ATTRIBUTES
        } else if flags != 0 {
            // If there are flags, read access is required to check if the attribute exists.
            W32Fs::FILE_WRITE_EA | W32Fs::FILE_READ_EA
        } else {
            W32Fs::FILE_WRITE_EA
        };

        let file = self.open_file(path, desired_access, Default::default())?;

        if system {
            xattr::set_system(&file, name, value, flags)
        } else {
            xattr::set(&file, name, value, flags)
        }
    }

    pub fn get_xattr(
        &self,
        path: &Path,
        name: &lx::LxStr,
        value: Option<&mut [u8]>,
    ) -> lx::Result<usize> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;
        if !self.state.options.override_xattrs.is_empty() {
            let name = match name.to_str() {
                Some(name) => name,
                None => {
                    tracing::warn!(path = %path.display(), ?name, "Invalid xattr name");
                    return Err(lx::Error::EINVAL);
                }
            };
            match self.state.options.override_xattrs.get(name) {
                Some(val) => {
                    let len = val.len();
                    if let Some(ret_val) = value {
                        if ret_val.len() < len {
                            return Err(lx::Error::ERANGE);
                        }
                        ret_val[..len].copy_from_slice(val);
                    }
                    return Ok(len);
                }
                None => {
                    tracing::trace!(path = %path.display(), name, "Not an overridden xattr name");
                    return Err(lx::Error::ENOENT);
                }
            }
        }

        if !self.supports_xattr() {
            return Err(lx::Error::ENOTSUP);
        }

        let system = name.as_bytes().starts_with(b"system.");
        let desired_access = if system {
            W32Fs::FILE_READ_ATTRIBUTES
        } else {
            W32Fs::FILE_READ_EA
        };

        let file = self.open_file(path, desired_access, Default::default())?;

        let name = name.to_str().ok_or(lx::Error::EINVAL)?;
        if system {
            xattr::get_system(&file, name, value)
        } else {
            xattr::get(&file, name, value)
        }
    }

    pub fn list_xattr(&self, path: &Path, list: Option<&mut [u8]>) -> lx::Result<usize> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;
        if !self.state.options.override_xattrs.is_empty() {
            let len = self
                .state
                .options
                .override_xattrs
                .keys()
                .fold(0usize, |a, k| a + k.len() + 1);
            if let Some(list) = list {
                if list.len() < len {
                    return Err(lx::Error::ERANGE);
                }
                let mut offset = 0;
                for key in self.state.options.override_xattrs.keys() {
                    let len = key.len();
                    list[offset..offset + len].copy_from_slice(key.as_bytes());
                    list[offset + len] = 0;
                    offset += len + 1;
                }
            }
            return Ok(len);
        }

        if !self.supports_xattr() {
            return Err(lx::Error::ENOTSUP);
        }

        let mut desired_access = W32Fs::FILE_READ_EA;
        if self.supports_case_sensitive_dir() {
            desired_access |= W32Fs::FILE_READ_ATTRIBUTES;
        }

        let file = self.open_file(path, desired_access, Default::default())?;
        let mut flags = 0;

        // If the file system supports case sensitive directories, and this is a directory,
        // include the "system.wsl_case_sensitive" attribute in the list.
        if self.supports_case_sensitive_dir() {
            let result = util::query_information_file::<
                SystemServices::FILE_ATTRIBUTE_TAG_INFORMATION,
            >(&file);

            if let Ok(info) = result {
                if info.FileAttributes & W32Fs::FILE_ATTRIBUTE_DIRECTORY.0 != 0
                    && !util::is_symlink(info.FileAttributes, info.ReparseTag)
                {
                    flags |= xattr::LX_UTIL_XATTR_LIST_CASE_SENSITIVE_DIR;
                }
            }
        }

        let size = xattr::list(&file, list, flags)?;

        Ok(size)
    }

    pub fn remove_xattr(&self, path: &Path, name: &lx::LxStr) -> lx::Result<()> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;
        if !self.state.options.override_xattrs.is_empty() {
            tracing::info!(
                path = %path.display(),
                ?name,
                "Ignoring xattr remove on volume with overridden xattrs"
            );
            return Ok(());
        }
        if !self.supports_xattr() || name.as_bytes().starts_with(b"system.") {
            return Err(lx::Error::ENOTSUP);
        }

        let file = self.open_file(
            path,
            W32Fs::FILE_READ_EA | W32Fs::FILE_WRITE_EA,
            Default::default(),
        )?;
        xattr::remove(&file, name.to_str().ok_or(lx::Error::EINVAL)?)
    }

    fn supports_xattr(&self) -> bool {
        self.state.fs_context.compatibility_flags.supports_xattr()
    }

    fn supports_case_sensitive_dir(&self) -> bool {
        self.state
            .fs_context
            .compatibility_flags
            .supports_case_sensitive_dir()
    }

    fn mkdir_helper(
        &self,
        path: &Path,
        mut options: super::LxCreateOptions,
    ) -> lx::Result<OwnedHandle> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;

        let mut desired_access = util::MINIMUM_PERMISSIONS;
        if self.state.options.create_case_sensitive_dirs {
            desired_access |= W32Fs::DELETE | W32Fs::FILE_WRITE_ATTRIBUTES;
        }

        // The file type in the mode is ignored; this function only creates directories.
        options.mode = lx::S_IFDIR | (options.mode & 0o1777);
        let (handle, _) = self.create_file(
            path,
            desired_access,
            FileSystem::FILE_CREATE,
            W32Fs::FILE_ATTRIBUTE_DIRECTORY,
            FileSystem::FILE_DIRECTORY_FILE | FileSystem::FILE_OPEN_REPARSE_POINT,
            Some(options),
            0,
        )?;

        if self.state.options.create_case_sensitive_dirs {
            // If setting case sensitive info fails, the file should be deleted.
            let delete_on_failure = pal::ScopeExit::new(|| {
                let _ = fs::delete_file(&self.state.fs_context, &handle);
            });

            let info = FileSystem::FILE_CASE_SENSITIVE_INFORMATION {
                Flags: W32Ss::FILE_CS_FLAG_CASE_SENSITIVE_DIR,
            };

            util::set_information_file(&handle, &info)?;
            delete_on_failure.dismiss();
        }

        Ok(handle)
    }

    fn symlink_helper(
        &self,
        path: &Path,
        target: &lx::LxStr,
        mut options: super::LxCreateOptions,
    ) -> lx::Result<OwnedHandle> {
        assert!(path.is_relative() && !path.as_os_str().is_empty());
        self.check_sandbox_enforcement(path)?;

        // Handle filesystems that do not support reparse points.
        if !self
            .state
            .fs_context
            .compatibility_flags
            .supports_reparse_points()
        {
            return Err(lx::Error::EPERM);
        }

        // Convert the target to its native Windows format.
        let win_target = Path::from_lx(target);

        // Determine whether a NT symlink can be created, and if so whether it should be a file or
        // directory.
        let link_type = if let Ok(win_target) = &win_target {
            self.determine_symlink_type(path.parent().unwrap(), win_target)
        } else {
            SymlinkType::Lx
        };

        // Create the reparse point data for the symlink type.
        let mut create_options = FileSystem::FILE_OPEN_REPARSE_POINT;
        let reparse_data = match link_type {
            SymlinkType::Lx => fs::create_link_reparse_buffer(target)?,
            SymlinkType::Nt(dir) => {
                if dir {
                    create_options |= FileSystem::FILE_DIRECTORY_FILE;
                }

                // In this path, win_target cannot be an error.
                util::create_nt_link_reparse_buffer(win_target.unwrap().as_os_str(), 0)?
            }
        };

        // Mode is ignored, as symlinks always have full permissions mode.
        options.mode = lx::S_IFLNK | 0o777;
        let (handle, _) = self.create_file(
            path,
            util::MINIMUM_PERMISSIONS | W32Fs::FILE_WRITE_ATTRIBUTES | W32Fs::DELETE,
            FileSystem::FILE_CREATE,
            Default::default(),
            create_options,
            Some(options),
            0,
        )?;

        // If setting the reparse point fails, the file should be deleted.
        let delete_on_failure = pal::ScopeExit::new(|| {
            let _ = fs::delete_file(&self.state.fs_context, &handle);
        });

        // Try to set the reparse point.
        if let Err(e) = util::set_reparse_point(&handle, &reparse_data) {
            if let SymlinkType::Lx = link_type {
                return Err(e);
            }

            // If creating an NT link failed with a permission error (which means the user was not
            // elevated and doesn't have developer mode enabled), fall back to an LX link and try
            // again.
            let reparse_data = fs::create_link_reparse_buffer(target)?;
            util::set_reparse_point(&handle, &reparse_data)?;
        }

        delete_on_failure.dismiss();
        Ok(handle)
    }

    /// Helper to create a hard link.
    fn link_helper(&self, path: &Path, new_path: &Path) -> lx::Result<OwnedHandle> {
        assert!(path.is_relative());
        assert!(new_path.is_relative());
        self.check_sandbox_enforcement(path)?;
        self.check_sandbox_enforcement(new_path)?;

        if !self
            .state
            .fs_context
            .compatibility_flags
            .supports_hard_links()
        {
            return Err(lx::Error::EPERM);
        }

        let handle = self.open_file(
            path,
            util::MINIMUM_PERMISSIONS | W32Fs::FILE_WRITE_ATTRIBUTES | W32Fs::SYNCHRONIZE,
            Default::default(),
        )?;

        let parent = self.open_parent(new_path)?;
        let (target_parent, target_path) = match &parent {
            Some((parent, name)) => (parent, *name),
            None => (&self.root, new_path),
        };
        util::create_link(&handle, target_parent, target_path)?;
        Ok(handle)
    }

    // Helper to set attributes and optionally retrieve them.
    fn set_attr_helper(
        &self,
        path: &Path,
        attr: SetAttributes,
        get_attr: bool,
    ) -> lx::Result<Option<lx::Stat>> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;
        let desired_access = util::permissions_for_set_attr(&attr, self.state.options.metadata);
        let file = self.open_file(path, desired_access, Default::default())?;
        util::set_attr(&file, &self.state, attr)?;
        if get_attr {
            Ok(Some(self.get_stat_by_handle(&file)?))
        } else {
            Ok(None)
        }
    }

    fn mknod_helper(
        &self,
        path: &Path,
        options: super::LxCreateOptions,
        mut device_id: lx::dev_t,
    ) -> lx::Result<OwnedHandle> {
        assert!(path.is_relative());
        self.check_sandbox_enforcement(path)?;

        if !lx::s_isreg(options.mode)
            && !lx::s_ischr(options.mode)
            && !lx::s_isblk(options.mode)
            && !lx::s_isfifo(options.mode)
            && !lx::s_issock(options.mode)
        {
            return Err(lx::Error::EINVAL);
        }

        if !lx::s_isreg(options.mode) && !self.state.options.metadata {
            return Err(lx::Error::EPERM);
        }

        if !lx::s_ischr(options.mode) && !lx::s_isblk(options.mode) {
            device_id = 0;
        }

        let mode = options.mode;
        let (handle, _) = self.create_file(
            path,
            util::MINIMUM_PERMISSIONS | W32Fs::FILE_WRITE_ATTRIBUTES | W32Fs::DELETE,
            FileSystem::FILE_CREATE,
            Default::default(),
            FileSystem::FILE_OPEN_REPARSE_POINT,
            Some(options),
            device_id,
        )?;

        if !lx::s_isreg(mode) {
            // If setting the reparse point fails, the file should be deleted.
            let delete_on_failure = pal::ScopeExit::new(|| {
                let _ = fs::delete_file(&self.state.fs_context, &handle);
            });

            /// Only the required header for FSCTL_SET_REPARSE_POINT, with data length of zero.
            /// See ntifs.h REPARSE_DATA_BUFFER.
            #[expect(non_snake_case)]
            #[repr(C)]
            #[derive(Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
            struct REPARSE_DATA_BUFFER {
                ReparseTag: u32,
                ReparseDataLength: u16,
                Reserved: u16,
            }

            let reparse_tag = util::file_mode_to_reparse_tag(mode);

            let reparse_buffer = REPARSE_DATA_BUFFER {
                ReparseTag: reparse_tag,
                ReparseDataLength: 0,
                Reserved: 0,
            };

            util::set_reparse_point(&handle, reparse_buffer.as_bytes())?;

            delete_on_failure.dismiss();
        }

        Ok(handle)
    }

    // Helper to create files using a relative path from the root of this volume.
    fn create_file(
        &self,
        path: &Path,
        desired_access: W32Fs::FILE_ACCESS_RIGHTS,
        disposition: FileSystem::NTCREATEFILE_CREATE_DISPOSITION,
        mut file_attributes: W32Fs::FILE_FLAGS_AND_ATTRIBUTES,
        create_options: FileSystem::NTCREATEFILE_CREATE_OPTIONS,
        options: Option<super::LxCreateOptions>,
        device_id: lx::dev_t,
    ) -> lx::Result<(OwnedHandle, usize)> {
        self.check_sandbox_enforcement(path)?;

        assert!(
            disposition == FileSystem::FILE_OPEN
                || disposition == FileSystem::FILE_OPEN_IF
                || disposition == FileSystem::FILE_CREATE
        );

        let mut ea_buffer = [0u8; fs::LX_UTIL_FS_METADATA_EA_BUFFER_SIZE];
        let mut ea = None;
        if disposition != FileSystem::FILE_OPEN {
            // If a new file is being created, create an EA buffer for Linux metadata.
            let mut options = options.ok_or(lx::Error::EINVAL)?;
            if self.state.options.metadata {
                util::apply_attr_overrides(
                    &self.state,
                    Some(&mut options.uid),
                    Some(&mut options.gid),
                    Some(&mut options.mode),
                );
                self.determine_creation_info(path, &mut options.mode, &mut options.gid)?;
                util::apply_attr_overrides(
                    &self.state,
                    Some(&mut options.uid),
                    Some(&mut options.gid),
                    Some(&mut options.mode),
                );
                let len = fs::create_metadata_ea_buffer(
                    options.uid,
                    options.gid,
                    options.mode,
                    device_id,
                    &mut ea_buffer,
                )?;

                ea = Some(&ea_buffer[..len]);
            }

            // Set the read-only attribute if no write bits are set.
            if lx::s_isreg(options.mode) && options.mode & 0o222 == 0 {
                file_attributes |= W32Fs::FILE_ATTRIBUTE_READONLY;
            }
        }

        util::open_relative_file(
            Some(&self.root),
            path,
            desired_access,
            disposition,
            util::OpenOptions {
                file_attributes,
                create_options,
                ea_buffer: ea,
            },
        )
    }

    /// Resolves namespace-operation destinations without following reparse points.
    fn open_parent<'a>(&self, path: &'a Path) -> lx::Result<Option<(OwnedHandle, &'a Path)>> {
        crate::path::validate_relative_path(path)?;
        let parent_path = path.parent().ok_or(lx::Error::EINVAL)?;
        let name = Path::new(path.file_name().ok_or(lx::Error::EINVAL)?);
        if parent_path.as_os_str().is_empty() {
            return Ok(None);
        }
        let (parent, _) = util::open_relative_file(
            Some(&self.root),
            parent_path,
            W32Fs::FILE_ACCESS_RIGHTS(0),
            FileSystem::FILE_OPEN,
            util::OpenOptions {
                create_options: FileSystem::FILE_DIRECTORY_FILE,
                ..Default::default()
            },
        )
        .map_err(|err| {
            if err == lx::Error::ELOOP {
                lx::Error::ENOTDIR
            } else {
                err
            }
        })?;
        Ok(Some((parent, name)))
    }

    /// Helper to open existing files using a relative path from the root of this volume.
    ///
    /// N.B. This function always adds FILE_OPEN_REPARSE_POINT to the options.
    fn open_file(
        &self,
        path: &Path,
        desired_access: W32Fs::FILE_ACCESS_RIGHTS,
        create_options: FileSystem::NTCREATEFILE_CREATE_OPTIONS,
    ) -> lx::Result<OwnedHandle> {
        self.check_sandbox_enforcement(path)?;
        let (handle, _) = self.create_file(
            path,
            desired_access,
            FileSystem::FILE_OPEN,
            Default::default(),
            create_options | FileSystem::FILE_OPEN_REPARSE_POINT,
            None,
            0,
        )?;

        Ok(handle)
    }

    /// Deletes a file, clearing the read-only attribute if necessary.
    fn delete_file(&self, handle: &OwnedHandle) -> lx::Result<()> {
        let result = fs::delete_file_core(&self.state.fs_context, handle);

        match result {
            Ok(_) => result,
            Err(e) => match fs::analyze_delete_error(e, handle) {
                fs::DeleteErrorAction::ReturnError(err) => Err(err),
                fs::DeleteErrorAction::TryReadOnlyWorkaround => {
                    // Reopen with the correct permissions to query and clear the read-only attribute,
                    // and try again.
                    let handle = util::reopen_file(
                        handle,
                        W32Fs::DELETE | W32Fs::FILE_READ_ATTRIBUTES | W32Fs::FILE_WRITE_ATTRIBUTES,
                    )?;

                    fs::delete_read_only_file(&self.state.fs_context, &handle)
                }
            },
        }
    }

    // Determines whether the mode and gid must be changed based on the parent's set-group-id bit.
    fn determine_creation_info(
        &self,
        path: &Path,
        mode: &mut lx::mode_t,
        gid: &mut lx::gid_t,
    ) -> lx::Result<()> {
        let dir = if let Some(parent) = path.parent() {
            parent
        } else {
            // This means the user is attempting to create the root, which makes no sense, so
            // nothing to do here.
            return Ok(());
        };

        let info = self.state.get_attributes(Some(&self.root), dir, None)?;
        // If the parent doesn't have explicit mode metadata, it can't have the set-group-id bit.
        if info.stat.LxFlags & FileSystem::LX_FILE_METADATA_HAS_MODE != 0 {
            util::determine_creation_info(info.stat.LxMode, info.stat.LxGid, mode, gid);
        }

        Ok(())
    }

    // Determines what kind of symlink to create for a given target.
    fn determine_symlink_type(&self, symlink_parent: &Path, target: &Path) -> SymlinkType {
        // Can't create NT symlinks for absolute paths.
        // N.B. This complicated check is necessary because is_absolute returns false for paths that
        //      have no drive letter, or paths that use the form C:foo (without a \).
        if target.has_root() || matches!(target.components().next(), Some(Component::Prefix(_))) {
            return SymlinkType::Lx;
        }

        // Get the canonical form of the target. If the target doesn't exist, create an LX symlink.
        let mut path = self.root_path.clone();
        path.push(symlink_parent);
        path.push(target);
        path = {
            #[expect(clippy::disallowed_methods)] // need actual canonical path here
            match path.canonicalize() {
                Ok(p) => p,
                Err(_) => return SymlinkType::Lx,
            }
        };

        // If the target isn't inside the volume, create an LX symlink. This catches cases where
        // symlinks in the path point outside the mount.
        if !path.starts_with(&self.root_path) {
            return SymlinkType::Lx;
        }

        // Manually resolve the target path to check if it would cross the mount boundary. This
        // catches cases where the target contains ".." components that would escape the volume
        // (e.g., "../..") or that walk out and back in (e.g., "../../mount/dir").
        let mut working_path = self.root_path.clone();
        working_path.push(symlink_parent);
        for component in target.components() {
            match component {
                Component::ParentDir => {
                    if !working_path.pop() {
                        return SymlinkType::Lx;
                    }
                    if !working_path.starts_with(&self.root_path) {
                        return SymlinkType::Lx;
                    }
                }
                Component::Normal(name) => working_path.push(name),
                Component::CurDir => {}
                _ => return SymlinkType::Lx,
            }
        }

        // Determine the attributes of the found target. If an error occurs, create an LX symlink.
        let info = match self.state.get_attributes(None, &path, None) {
            Ok(i) => i.stat,
            Err(_) => return SymlinkType::Lx,
        };

        // If the target is an LX symlink, create an LX symlink. Otherwise the file type should
        // match the target.
        if info.FileAttributes & W32Fs::FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            && info.ReparseTag == FileSystem::IO_REPARSE_TAG_LX_SYMLINK as u32
        {
            SymlinkType::Lx
        } else if info.FileAttributes & W32Fs::FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            SymlinkType::Nt(true)
        } else {
            SymlinkType::Nt(false)
        }
    }

    fn get_stat_by_handle(&self, handle: &OwnedHandle) -> lx::Result<lx::Stat> {
        let mut info = self.state.get_attributes_by_handle(handle)?;
        util::file_info_to_stat(
            &self.state.fs_context,
            &mut info,
            &self.state.options,
            self.state.block_size,
        )
        .map(|x| x.into())
    }
}

// Windows version of an LxFile.
// See `crate::LxFile` for more comments.
pub struct LxFile {
    handle: OwnedHandle,
    state: Arc<VolumeState>,
    enumerator: Option<readdir::DirectoryEnumerator>,
    access: W32Fs::FILE_ACCESS_RIGHTS,
    kill_priv: AtomicBool,
    is_app_exec_alias: Mutex<bool>,
}

impl LxFile {
    pub fn fstat(&self) -> lx::Result<lx::StatEx> {
        let mut info = self.state.get_attributes_by_handle(&self.handle)?;
        *self.is_app_exec_alias.lock() = info.is_app_execution_alias;
        util::file_info_to_stat(
            &self.state.fs_context,
            &mut info,
            &self.state.options,
            self.state.block_size,
        )
    }

    /// Returns whether the file was opened with write access (including append-only).
    fn is_writable(&self) -> bool {
        (self.access & (W32Fs::FILE_WRITE_DATA | W32Fs::FILE_APPEND_DATA)).0 != 0
    }

    pub fn set_attr(&self, mut attr: SetAttributes) -> lx::Result<()> {
        // ftruncate requires the descriptor to be writable, matching Linux where truncating an
        // O_RDONLY descriptor fails with EINVAL. This is enforced here because the truncate below
        // may run on a handle reopened with FILE_WRITE_DATA, which would otherwise allow truncating
        // a read-only descriptor.
        if attr.size.is_some() && !self.is_writable() {
            return Err(lx::Error::EINVAL);
        }

        util::set_attr_check_kill_priv(&self.handle, &self.state, &mut attr)?;

        let desired_access = util::permissions_for_set_attr(&attr, self.state.options.metadata);

        // Only reopen if there's an operation that requires it and we don't already have the
        // required permissions. A size change is included so a file opened O_APPEND (which lacks
        // FILE_WRITE_DATA) is reopened with the write access needed to truncate.
        let mut _file = None;
        let handle = if self.access & desired_access != desired_access
            && (attr.mode.is_some()
                || attr.uid.is_some()
                || attr.gid.is_some()
                || attr.size.is_some()
                || !attr.atime.is_omit()
                || !attr.mtime.is_omit()
                || !attr.ctime.is_omit())
        {
            let handle = util::reopen_file(&self.handle, desired_access)?;
            _file = Some(handle);
            _file.as_ref().unwrap()
        } else {
            &self.handle
        };

        util::set_attr_core(handle, &self.state, &attr)?;

        if self.state.options.metadata {
            if let Some(mode) = attr.mode {
                if mode & (lx::S_ISUID | lx::S_ISGID) != 0 {
                    self.kill_priv.swap(true, Ordering::AcqRel);
                }
            }
        }

        Ok(())
    }

    pub fn pread(&self, buffer: &mut [u8], offset: lx::off_t) -> lx::Result<usize> {
        if offset < 0 {
            return Err(lx::Error::EINVAL);
        }
        let mut iosb = Default::default();
        let buffer_ptr = buffer.as_mut_ptr().cast::<ffi::c_void>();
        let buffer_len: u32 = buffer.len().try_into().map_err(|_| lx::Error::EINVAL)?;

        if *self.is_app_exec_alias.lock() {
            Ok(fs::read_app_exec_link(offset, buffer))
        } else {
            unsafe {
                // TODO: Async I/O.
                let _ = util::check_status_rw(FileSystem::NtReadFile(
                    Foundation::HANDLE(self.handle.as_raw_handle()),
                    None,
                    None,
                    None,
                    &mut iosb,
                    buffer_ptr,
                    buffer_len,
                    Some(&offset),
                    None,
                ))?;

                Ok(iosb.Information)
            }
        }
    }

    pub fn pwrite(
        &self,
        buffer: &[u8],
        offset: lx::off_t,
        thread_uid: lx::uid_t,
    ) -> lx::Result<usize> {
        unsafe {
            // When metadata is enabled, the Linux behavior of clearing set-user-ID and set-group-ID
            // on write must be emulated. This should actually be based on CAP_FSETID, but that
            // information is not available so simply check for the root user instead.
            // For performance, this is not checked every time, so if the mode is changed external
            // to this LxFile, it will not be reset.
            if self.state.options.metadata
                && thread_uid != 0
                && self.kill_priv.swap(false, Ordering::AcqRel)
            {
                let stat = self.fstat()?;
                if stat.mode as u32 & (lx::S_ISUID | lx::S_ISGID) != 0 {
                    let mut attr = SetAttributes::default();
                    attr.mode = Some(stat.mode as u32 & !(lx::S_ISUID | lx::S_ISGID));
                    self.set_attr(attr)?;
                }
            }

            let mut iosb = Default::default();
            let buffer_ptr = buffer.as_ptr() as *mut ffi::c_void;
            let buffer_len = buffer.len().try_into().map_err(|_| lx::Error::EINVAL)?;

            let nt_offset: i64 = offset;

            // TODO: Async I/O.
            let _ = util::check_status_rw(FileSystem::NtWriteFile(
                Foundation::HANDLE(self.handle.as_raw_handle()),
                None,
                None,
                None,
                &mut iosb,
                buffer_ptr,
                buffer_len,
                Some(&nt_offset),
                None,
            ))?;

            Ok(iosb.Information)
        }
    }

    pub fn read_dir<F>(&mut self, offset: lx::off_t, mut callback: F) -> lx::Result<()>
    where
        F: FnMut(lx::DirEntry) -> lx::Result<bool>,
    {
        if self.enumerator.is_none() {
            self.enumerator = Some(readdir::DirectoryEnumerator::new(false)?);
        }

        let enumerator = self.enumerator.as_mut().unwrap();
        let mut local_offset = offset;

        // Write the . and .. entries, since `read_dir` doesn't return them.
        if !Self::process_dot_entries(&mut local_offset, &mut callback)? {
            return Ok(());
        }

        assert!(local_offset >= DOT_ENTRY_COUNT);

        let mut offset = local_offset - DOT_ENTRY_COUNT;
        enumerator.read_dir(
            &self.handle,
            &self.state.fs_context,
            &mut offset,
            &mut callback,
        )?;

        Ok(())
    }

    pub fn fsync(&self, data_only: bool) -> lx::Result<()> {
        // Linux allows using fsync on files that have been opened read-only, while
        // Windows does not, so reopen the file if necessary.
        let mut _reopened = None;
        let handle = if self.is_writable() {
            &self.handle
        } else {
            let file = match util::reopen_file(&self.handle, W32Fs::FILE_WRITE_DATA) {
                Ok(file) => file,
                // If FILE_WRITE_DATA failed, try again with FILE_APPEND_DATA
                Err(_) => match util::reopen_file(&self.handle, W32Fs::FILE_APPEND_DATA) {
                    Ok(file) => file,
                    Err(e) => {
                        // If this failed due to an access denied, just return success because on
                        // Linux this is supposed to succeed with read-only access and some
                        // applications break if it doesn't.
                        if e.value() == lx::EACCES {
                            return Ok(());
                        }

                        return Err(e);
                    }
                },
            };

            _reopened = Some(file);
            _reopened.as_ref().unwrap()
        };

        let flags = if data_only {
            W32Ss::FLUSH_FLAGS_FILE_DATA_ONLY
        } else {
            0
        };

        let mut iosb = Default::default();
        unsafe {
            let _ = util::check_status(FileSystem::NtFlushBuffersFileEx(
                Foundation::HANDLE(handle.as_raw_handle()),
                flags,
                ptr::null_mut(),
                0,
                &mut iosb,
            ))?;
        }

        Ok(())
    }

    // Helper to emit the . and .. entries.
    fn process_dot_entries<F>(offset: &mut lx::off_t, callback: &mut F) -> lx::Result<bool>
    where
        F: FnMut(lx::DirEntry) -> lx::Result<bool>,
    {
        if *offset == 0
            && !Self::process_dir_entry(offset, callback, 0, lx::LxString::from("."), lx::DT_DIR)?
        {
            return Ok(false);
        }

        if *offset == 1
            && !Self::process_dir_entry(offset, callback, 0, lx::LxString::from(".."), lx::DT_DIR)?
        {
            return Ok(false);
        }

        Ok(true)
    }

    // Helper to call the user's closure for a directory
    fn process_dir_entry<F>(
        offset: &mut lx::off_t,
        callback: &mut F,
        inode_nr: u64,
        name: lx::LxString,
        file_type: u8,
    ) -> lx::Result<bool>
    where
        F: FnMut(lx::DirEntry) -> lx::Result<bool>,
    {
        let entry = lx::DirEntry {
            name,
            inode_nr,
            offset: *offset + 1, // Pass the offset of the next entry.
            file_type,
        };

        let result = (callback)(entry)?;

        // Update the offset only if the user wants to continue.
        if result {
            *offset += 1;
        }

        Ok(result)
    }
}

// Symbolic link type.
pub enum SymlinkType {
    Lx,
    Nt(bool), // True for directory links; false for file links.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LxCreateOptions;
    use crate::LxVolumeOptions;
    use test_with_tracing::test;

    fn deny_read_ea(handle: &impl AsRawHandle) {
        deny_access(handle, W32Fs::FILE_READ_EA);
    }

    fn deny_access(handle: &impl AsRawHandle, access: W32Fs::FILE_ACCESS_RIGHTS) {
        let descriptor: pal::windows::security::LocalSecurityDescriptor =
            format!("D:P(D;;0x{:x};;;WD)(A;;FA;;;WD)", access.0)
                .parse()
                .unwrap();
        // SAFETY: The handle and security descriptor remain valid throughout the call.
        unsafe {
            windows::Win32::Security::SetKernelObjectSecurity(
                Foundation::HANDLE(handle.as_raw_handle()),
                windows::Win32::Security::DACL_SECURITY_INFORMATION,
                windows::Win32::Security::PSECURITY_DESCRIPTOR(descriptor.as_ptr()),
            )
            .unwrap();
        }
    }

    #[test]
    fn namespace_destinations_without_parent_read_attributes() {
        for force_legacy_rename in [false, true] {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("dir")).unwrap();
            std::fs::write(root.path().join("source"), b"data").unwrap();
            let mut options = LxVolumeOptions::new();
            options.sandbox(false);
            let mut volume = LxVolume::new(root.path(), &options).unwrap();
            if force_legacy_rename {
                Arc::get_mut(&mut volume.state)
                    .unwrap()
                    .fs_context
                    .compatibility_flags
                    .set_supports_posix_unlink_rename(false);
            }
            let open_directory = |path: &Path| {
                std::fs::OpenOptions::new()
                    .access_mode(W32Fs::WRITE_DAC.0)
                    .custom_flags(W32Fs::FILE_FLAG_BACKUP_SEMANTICS.0)
                    .open(path)
                    .unwrap()
            };
            let root_handle = open_directory(root.path());
            let parent_handle = open_directory(&root.path().join("dir"));
            deny_access(&parent_handle, W32Fs::FILE_READ_ATTRIBUTES);
            // Listing the root can otherwise permit reading its child's attributes.
            deny_access(&root_handle, W32Fs::FILE_LIST_DIRECTORY);

            let attribute_error = volume
                .open_file(
                    Path::new("dir"),
                    W32Fs::FILE_READ_ATTRIBUTES,
                    FileSystem::FILE_DIRECTORY_FILE,
                )
                .err();
            let link_result = volume.link(Path::new("source"), Path::new(r"dir\linked"));
            let rename_result = volume.rename(Path::new("source"), Path::new(r"dir\renamed"), 0);
            deny_access(&parent_handle, W32Fs::FILE_ACCESS_RIGHTS(0));
            deny_access(&root_handle, W32Fs::FILE_ACCESS_RIGHTS(0));

            assert_eq!(attribute_error, Some(lx::Error::EACCES));
            link_result.unwrap();
            rename_result.unwrap();
            for name in ["linked", "renamed"] {
                assert_eq!(
                    std::fs::read(root.path().join("dir").join(name)).unwrap(),
                    b"data"
                );
            }
            assert!(!root.path().join("source").exists());
        }
    }

    #[test]
    fn metadata_query_does_not_open_a_synchronous_handle() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        let volume = LxVolume::new(root.path(), &LxVolumeOptions::new()).unwrap();
        let handle = std::fs::OpenOptions::new()
            .access_mode(W32Fs::WRITE_DAC.0)
            .open(root.path().join("file"))
            .unwrap();
        deny_access(&handle, W32Fs::SYNCHRONIZE);
        let open_error = volume
            .open_file(
                Path::new("file"),
                W32Fs::FILE_READ_ATTRIBUTES,
                Default::default(),
            )
            .err();
        let stat = volume.lstat(Path::new("file"));
        deny_access(&handle, W32Fs::FILE_ACCESS_RIGHTS(0));
        assert_eq!(open_error, Some(lx::Error::EACCES));
        assert_eq!(stat.unwrap().file_size, 4);
    }

    #[test]
    fn no_follow_stat_without_read_ea() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        let relative_path = Path::new(r"dir\file");
        let mut options = LxVolumeOptions::new();
        options.metadata(true);
        let volume = LxVolume::new(root.path(), &options).unwrap();
        volume
            .open(
                relative_path,
                lx::O_CREAT | lx::O_WRONLY,
                Some(LxCreateOptions::new(0o640, 123, 456)),
            )
            .unwrap()
            .pwrite(b"data", 0, 0)
            .unwrap();
        let path = root.path().join(relative_path);
        let handle = std::fs::OpenOptions::new()
            .access_mode(W32Fs::WRITE_DAC.0)
            .open(&path)
            .unwrap();
        deny_read_ea(&handle);

        for metadata in [false, true] {
            let mut options = LxVolumeOptions::new();
            options.metadata(metadata);
            let volume = LxVolume::new(root.path(), &options).unwrap();
            assert_eq!(
                volume
                    .open_file(relative_path, W32Fs::FILE_READ_EA, Default::default())
                    .err()
                    .unwrap(),
                lx::Error::EACCES
            );
            let stat = volume.lstat(relative_path).unwrap();
            assert_eq!(stat.file_size, 4);
            assert!(lx::s_isreg(stat.mode.into()));
            if metadata {
                assert_eq!(stat.mode, (lx::S_IFREG | 0o640) as u16);
                assert_eq!(stat.uid, 123);
                assert_eq!(stat.gid, 456);
            }
        }
    }

    #[test]
    fn no_follow_symlink_stat_without_read_ea() {
        let root = tempfile::tempdir().unwrap();
        let mut options = LxVolumeOptions::new();
        options.metadata(true);
        let volume = LxVolume::new(root.path(), &options).unwrap();
        let path = Path::new("link");
        let target = "missing";
        volume
            .symlink(path, target.as_ref(), LxCreateOptions::new(0o777, 0, 0))
            .unwrap();
        let handle = volume
            .open_file(path, W32Fs::WRITE_DAC, Default::default())
            .unwrap();
        deny_read_ea(&handle);
        assert_eq!(
            volume
                .open_file(path, W32Fs::FILE_READ_EA, Default::default())
                .err()
                .unwrap(),
            lx::Error::EACCES
        );

        let stat = volume.lstat(path).unwrap();
        assert!(lx::s_islnk(stat.mode.into()));
        assert_eq!(stat.file_size, target.len() as u64);
        assert_eq!(volume.read_link(path).unwrap(), target);
    }

    #[test]
    fn no_follow_stat_fallbacks() {
        for (query_by_name, stat_info) in [(true, true), (false, true), (false, false)] {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("dir")).unwrap();
            std::fs::write(root.path().join(r"dir\file"), b"data").unwrap();
            let options = LxVolumeOptions::new();
            let mut volume = LxVolume::new(root.path(), &options).unwrap();
            let flags = &mut Arc::get_mut(&mut volume.state)
                .unwrap()
                .fs_context
                .compatibility_flags;
            if !query_by_name {
                flags.set_supports_query_by_name(false);
            }
            if !stat_info {
                flags.set_supports_stat_info(false);
                flags.set_supports_stat_lx_info(false);
            }

            let stat = volume.lstat(Path::new(r"dir\file")).unwrap();
            assert_eq!(stat.file_size, 4);
            assert!(lx::s_isreg(stat.mode.into()));
            assert!(lx::s_isdir(
                volume.lstat(Path::new("dir")).unwrap().mode.into()
            ));
        }
    }

    #[test]
    fn no_follow_normal_operations() {
        let root = tempfile::tempdir().unwrap();
        let mut options = LxVolumeOptions::new();
        options.metadata(true);
        let volume = LxVolume::new(root.path(), &options).unwrap();
        let create = || LxCreateOptions::new(0o777, 0, 0);
        volume.mkdir(Path::new("dir"), create()).unwrap();
        volume.mkdir(Path::new(r"dir\child"), create()).unwrap();
        volume
            .open(
                Path::new(r"dir\file"),
                lx::O_CREAT | lx::O_RDWR,
                Some(create()),
            )
            .unwrap()
            .pwrite(b"data", 0, 0)
            .unwrap();
        assert_eq!(volume.lstat(Path::new(r"dir\file")).unwrap().file_size, 4);
        volume
            .link(Path::new(r"dir\file"), Path::new(r"dir\child\link"))
            .unwrap();
        volume
            .rename(Path::new(r"dir\child\link"), Path::new("renamed"), 0)
            .unwrap();
        assert_eq!(std::fs::read(root.path().join("renamed")).unwrap(), b"data");
        volume
            .symlink(Path::new("symlink"), "missing".as_ref(), create())
            .unwrap();
        assert!(lx::s_islnk(
            volume.lstat(Path::new("symlink")).unwrap().mode.into()
        ));
        assert_eq!(volume.read_link(Path::new("symlink")).unwrap(), "missing");
        volume
            .rename(Path::new("symlink"), Path::new(r"dir\symlink"), 0)
            .unwrap();
        volume.unlink(Path::new(r"dir\symlink"), 0).unwrap();
        volume.unlink(Path::new("renamed"), 0).unwrap();

        for path in [r"..\file", r"dir\..\file", r".\file"] {
            assert_eq!(
                volume.lstat(Path::new(path)).unwrap_err(),
                lx::Error::EINVAL
            );
        }
    }

    #[test]
    fn rename_fallback_uses_captured_parent() {
        for sandbox in [false, true] {
            for replacement_is_dir in [false, true] {
                let root = tempfile::tempdir().unwrap();
                std::fs::create_dir(root.path().join("dir")).unwrap();
                std::fs::write(root.path().join("source"), b"source").unwrap();
                let original_target = root.path().join(r"dir\target");
                std::fs::write(&original_target, b"original").unwrap();
                let mut permissions = std::fs::metadata(&original_target).unwrap().permissions();
                permissions.set_readonly(true);
                std::fs::set_permissions(&original_target, permissions).unwrap();

                let mut options = LxVolumeOptions::new();
                options.sandbox(sandbox);
                let mut volume = LxVolume::new(root.path(), &options).unwrap();
                Arc::get_mut(&mut volume.state)
                    .unwrap()
                    .fs_context
                    .compatibility_flags
                    .set_supports_posix_unlink_rename(false);
                let source = volume
                    .open_file(
                        Path::new("source"),
                        W32Fs::FILE_READ_ATTRIBUTES | W32Fs::DELETE,
                        Default::default(),
                    )
                    .unwrap();
                let (parent, name) = volume
                    .open_parent(Path::new(r"dir\target"))
                    .unwrap()
                    .unwrap();
                let error = fs::rename(
                    &source,
                    &parent,
                    name,
                    &volume.state.fs_context,
                    fs::RenameFlags::default(),
                )
                .unwrap_err();
                assert!(matches!(error.value(), lx::EPERM | lx::EACCES | lx::EEXIST));

                std::fs::rename(root.path().join("dir"), root.path().join("original")).unwrap();
                std::fs::create_dir(root.path().join("dir")).unwrap();
                let replacement_target = root.path().join(r"dir\target");
                let replacement_file = if replacement_is_dir {
                    std::fs::create_dir(&replacement_target).unwrap();
                    replacement_target.join("child")
                } else {
                    replacement_target
                };
                std::fs::write(&replacement_file, b"replacement").unwrap();

                volume
                    .rename_file(
                        Path::new("source"),
                        Path::new(r"dir\target"),
                        &source,
                        &parent,
                        name,
                    )
                    .unwrap();
                assert_eq!(
                    std::fs::read(root.path().join(r"original\target")).unwrap(),
                    b"source"
                );
                assert_eq!(std::fs::read(replacement_file).unwrap(), b"replacement");
                assert!(!root.path().join("source").exists());
            }
        }
    }

    #[test]
    fn rename_checks_captured_target_attributes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("source"), b"source").unwrap();
        std::fs::write(root.path().join(r"dir\target"), b"original").unwrap();
        let mut options = LxVolumeOptions::new();
        options.sandbox(true);
        let mut volume = LxVolume::new(root.path(), &options).unwrap();
        Arc::get_mut(&mut volume.state)
            .unwrap()
            .fs_context
            .compatibility_flags
            .set_supports_posix_unlink_rename(false);
        let source = volume
            .open_file(
                Path::new("source"),
                W32Fs::FILE_READ_ATTRIBUTES | W32Fs::DELETE,
                Default::default(),
            )
            .unwrap();
        let (parent, name) = volume
            .open_parent(Path::new(r"dir\target"))
            .unwrap()
            .unwrap();
        std::fs::rename(root.path().join("dir"), root.path().join("original")).unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join(r"dir\target"), b"replacement").unwrap();
        let (target, _) = util::open_relative_file(
            Some(&parent),
            name,
            W32Fs::FILE_READ_ATTRIBUTES | W32Fs::FILE_WRITE_ATTRIBUTES,
            FileSystem::FILE_OPEN,
            util::OpenOptions {
                create_options: FileSystem::FILE_OPEN_REPARSE_POINT,
                ..Default::default()
            },
        )
        .unwrap();
        let mut info: FileSystem::FILE_BASIC_INFORMATION =
            util::query_information_file(&target).unwrap();
        info.FileAttributes |= W32Fs::FILE_ATTRIBUTE_HIDDEN.0;
        util::set_information_file(&target, &info).unwrap();

        assert_eq!(
            volume
                .rename_file(
                    Path::new("source"),
                    Path::new(r"dir\target"),
                    &source,
                    &parent,
                    name,
                )
                .unwrap_err(),
            lx::Error::EACCES
        );
        assert_eq!(
            std::fs::read(root.path().join("source")).unwrap(),
            b"source"
        );
        assert_eq!(
            std::fs::read(root.path().join(r"original\target")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(root.path().join(r"dir\target")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    #[ignore = "requires permission to create native Windows symlinks"]
    fn no_follow_native_directory_link() {
        check_no_follow_directory_link(|target, link| {
            std::os::windows::fs::symlink_dir(target, link).unwrap();
        });
    }

    #[test]
    fn no_follow_directory_junction() {
        check_no_follow_directory_link(|target, link| {
            std::fs::create_dir(link).unwrap();
            let handle = std::fs::OpenOptions::new()
                .access_mode(Foundation::GENERIC_WRITE.0)
                .custom_flags(
                    W32Fs::FILE_FLAG_BACKUP_SEMANTICS.0 | W32Fs::FILE_FLAG_OPEN_REPARSE_POINT.0,
                )
                .open(link)
                .unwrap();
            let mut substitute = ffi::OsString::from(r"\??\");
            substitute.push(target);
            let substitute: Vec<u16> = substitute.encode_wide().collect();
            let length = u16::try_from(substitute.len() * 2).unwrap();
            // Mount-point data has four name fields, followed by two terminated names.
            let mut reparse = Vec::new();
            reparse.extend_from_slice(&W32Ss::IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
            for value in [8 + length + 4, 0, 0, length, length + 2, 0] {
                reparse.extend_from_slice(&value.to_le_bytes());
            }
            for value in substitute.into_iter().chain([0, 0]) {
                reparse.extend_from_slice(&value.to_le_bytes());
            }
            util::set_reparse_point(&handle.into(), &reparse).unwrap();
        });
    }

    fn check_no_follow_directory_link(create_link: impl Fn(&Path, &Path)) {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("file"), b"unchanged").unwrap();
        std::fs::write(root.path().join("source"), b"source").unwrap();
        create_link(outside.path(), &root.path().join("link"));

        let options = LxVolumeOptions::new();
        let volume = LxVolume::new(root.path(), &options).unwrap();
        let path = Path::new(r"link\file");
        let create = || LxCreateOptions::new(0o777, 0, 0);
        assert!(lx::s_islnk(
            volume.lstat(Path::new("link")).unwrap().mode.into()
        ));
        assert_eq!(volume.lstat(path).unwrap_err(), lx::Error::ELOOP);
        assert_eq!(
            volume
                .open(
                    path,
                    lx::O_CREAT | lx::O_TRUNC | lx::O_WRONLY | lx::O_NOFOLLOW,
                    Some(create())
                )
                .err()
                .unwrap(),
            lx::Error::ELOOP
        );
        assert_eq!(volume.mkdir(path, create()).unwrap_err(), lx::Error::ELOOP);
        assert_eq!(
            volume
                .mknod(path, LxCreateOptions::new(lx::S_IFREG | 0o666, 0, 0), 0)
                .unwrap_err(),
            lx::Error::ELOOP
        );
        assert_eq!(
            volume
                .symlink(path, "source".as_ref(), create())
                .unwrap_err(),
            lx::Error::ELOOP
        );
        assert_eq!(volume.unlink(path, 0).unwrap_err(), lx::Error::ELOOP);
        assert_eq!(
            volume.rename(path, Path::new("target"), 0).unwrap_err(),
            lx::Error::ELOOP
        );
        assert_eq!(
            volume.rename(Path::new("source"), path, 0).unwrap_err(),
            lx::Error::ENOTDIR
        );
        assert_eq!(
            volume.link(Path::new("source"), path).unwrap_err(),
            lx::Error::ENOTDIR
        );
        assert_eq!(
            volume.set_attr(path, SetAttributes::default()).unwrap_err(),
            lx::Error::ELOOP
        );
        assert_eq!(volume.stat_fs(path).unwrap_err(), lx::Error::ELOOP);

        assert_eq!(
            volume.read_link(Path::new("link")).unwrap_err(),
            lx::Error::EPERM
        );
        volume
            .rename(Path::new("link"), Path::new("moved"), 0)
            .unwrap();
        assert_eq!(
            std::fs::read_link(root.path().join("moved")).unwrap(),
            outside.path()
        );
        volume.unlink(Path::new("moved"), 0).unwrap();
        assert_eq!(
            std::fs::read(outside.path().join("file")).unwrap(),
            b"unchanged"
        );
        assert_eq!(
            std::fs::read(root.path().join("source")).unwrap(),
            b"source"
        );

        volume.mkdir(Path::new("dir"), create()).unwrap();
        let (parent, name) = volume
            .open_parent(Path::new(r"dir\target"))
            .unwrap()
            .unwrap();
        std::fs::rename(root.path().join("dir"), root.path().join("original")).unwrap();
        create_link(outside.path(), &root.path().join("dir"));
        let source = volume
            .open_file(
                Path::new("source"),
                W32Fs::DELETE | W32Fs::FILE_WRITE_ATTRIBUTES,
                Default::default(),
            )
            .unwrap();
        fs::rename(
            &source,
            &parent,
            name,
            &volume.state.fs_context,
            fs::RenameFlags::default(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.path().join(r"original\target")).unwrap(),
            b"source"
        );
        assert!(!outside.path().join("target").exists());
        util::create_link(&source, &parent, Path::new("hard_link")).unwrap();
        assert_eq!(
            std::fs::read(root.path().join(r"original\hard_link")).unwrap(),
            b"source"
        );
        assert!(!outside.path().join("hard_link").exists());
    }

    #[test]
    #[ignore = "requires native Windows symlink creation privilege"]
    fn no_follow_preserves_native_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let options = LxVolumeOptions::new();
        let volume = LxVolume::new(root.path(), &options).unwrap();
        let create = || LxCreateOptions::new(0o777, 0, 0);
        volume.mkdir(Path::new("dir"), create()).unwrap();
        volume
            .symlink(Path::new("link"), "dir".as_ref(), create())
            .unwrap();
        let handle = volume
            .open_file(
                Path::new("link"),
                W32Fs::FILE_READ_ATTRIBUTES,
                Default::default(),
            )
            .unwrap();
        let tag: SystemServices::FILE_ATTRIBUTE_TAG_INFORMATION =
            util::query_information_file(&handle).unwrap();
        assert_eq!(tag.ReparseTag, W32Ss::IO_REPARSE_TAG_SYMLINK);
        assert_eq!(volume.read_link(Path::new("link")).unwrap(), "dir");
        volume
            .rename(Path::new("link"), Path::new(r"dir\link"), 0)
            .unwrap();
        assert_eq!(volume.read_link(Path::new(r"dir\link")).unwrap(), "dir");
    }
}
