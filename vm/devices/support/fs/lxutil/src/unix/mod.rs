// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Calling libc file APIs.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks)]

pub(crate) mod path;
mod util;

use crate::SetAttributes;
use std::ffi;
use std::mem;
use std::os::unix::prelude::*;
use std::path::Path;

// Unix implementation of LxVolume.
// See crate::LxVolume for more detailed comments.
pub struct LxVolume {
    root: std::fs::File,
}

impl LxVolume {
    pub fn new(root_path: &Path, _options: &super::LxVolumeOptions) -> lx::Result<Self> {
        let path = util::path_to_cstr(root_path)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            // Open a file descriptor to the root to use with "*at" functions.
            let fd = util::check_lx_errno(libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY,
            ))?;

            Ok(Self {
                root: std::fs::File::from_raw_fd(fd),
            })
        }
    }

    pub fn supports_stable_file_id(&self) -> bool {
        true
    }

    pub fn lstat(&self, path: &Path) -> lx::Result<lx::Stat> {
        assert!(path.is_relative());
        let path = util::path_to_cstr(path)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        let stat = unsafe {
            let mut stat = mem::zeroed();
            util::check_lx_errno(libc::fstatat(
                self.root.as_raw_fd(),
                path.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH,
            ))?;
            stat
        };

        Ok(util::libc_stat_to_lx_stat(stat))
    }

    pub fn set_attr(&self, path: &Path, attr: SetAttributes) -> lx::Result<()> {
        util::set_attr(&self.root, Some(path), &attr)
    }

    pub fn set_attr_stat(&self, path: &Path, attr: SetAttributes) -> lx::Result<lx::Stat> {
        util::set_attr(&self.root, Some(path), &attr)?;
        self.lstat(path)
    }

    pub fn open(
        &self,
        path: &Path,
        flags: i32,
        options: Option<super::LxCreateOptions>,
    ) -> lx::Result<LxFile> {
        assert!(path.is_relative());

        let fd = util::openat(&self.root, path, flags, options)?;

        Ok(LxFile {
            fd,
            enumerator: None,
        })
    }

    pub fn mkdir(&self, path: &Path, options: super::LxCreateOptions) -> lx::Result<()> {
        assert!(path.is_relative());

        let path = util::path_to_cstr(path)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::mkdirat(
                self.root.as_raw_fd(),
                path.as_ptr(),
                options.mode,
            ))?;
        }

        Ok(())
    }

    pub fn mkdir_stat(&self, path: &Path, options: super::LxCreateOptions) -> lx::Result<lx::Stat> {
        self.mkdir(path, options)?;
        self.lstat(path)
    }

    // The options are entirely ignored on Unix, because uid/gid are never used, and mode isn't
    // used for symlinks.
    pub fn symlink(
        &self,
        path: &Path,
        target: &lx::LxStr,
        _: super::LxCreateOptions,
    ) -> lx::Result<()> {
        assert!(path.is_relative());

        let path = util::path_to_cstr(path)?;
        let target = util::create_cstr(target.as_bytes())?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::symlinkat(
                target.as_ptr(),
                self.root.as_raw_fd(),
                path.as_ptr(),
            ))?;
        }

        Ok(())
    }

    pub fn symlink_stat(
        &self,
        path: &Path,
        target: &lx::LxStr,
        options: super::LxCreateOptions,
    ) -> lx::Result<lx::Stat> {
        self.symlink(path, target, options)?;
        self.lstat(path)
    }

    pub fn read_link(&self, path: &Path) -> lx::Result<lx::LxString> {
        assert!(path.is_relative());

        let mut buffer = [0u8; libc::PATH_MAX as usize];
        let path = util::path_to_cstr(path)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        let size = unsafe {
            util::check_lx_errno(libc::readlinkat(
                self.root.as_raw_fd(),
                path.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            ))?
        };

        // Size is guaranteed to be positive after check_lx_errno.
        Ok(lx::LxString::from_vec(Vec::from(&buffer[..size as usize])))
    }

    pub fn unlink(&self, path: &Path, flags: i32) -> lx::Result<()> {
        assert!(path.is_relative());

        let path = util::path_to_cstr(path)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::unlinkat(self.root.as_raw_fd(), path.as_ptr(), flags))?;
        }

        Ok(())
    }

    pub fn mknod(
        &self,
        path: &Path,
        options: super::LxCreateOptions,
        device_id: lx::dev_t,
    ) -> lx::Result<()> {
        assert!(path.is_relative());

        let path = util::path_to_cstr(path)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::mknodat(
                self.root.as_raw_fd(),
                path.as_ptr(),
                options.mode,
                device_id as u64,
            ))?;
        }

        Ok(())
    }

    pub fn mknod_stat(
        &self,
        path: &Path,
        options: super::LxCreateOptions,
        device_id: lx::dev_t,
    ) -> lx::Result<lx::Stat> {
        self.mknod(path, options, device_id)?;
        self.lstat(path)
    }

    pub fn rename(&self, path: &Path, new_path: &Path, flags: u32) -> lx::Result<()> {
        assert!(path.is_relative());
        assert!(new_path.is_relative());
        let path = util::path_to_cstr(path)?;
        let new_path = util::path_to_cstr(new_path)?;

        // renameat2 does not have a wrapper in musl, though it does in glibc. Call it using syscall directly instead.
        // SAFETY: Our arguments are valid for this syscall. We are passing arguments in the
        // correct order and as the correct types. We are casting the return value to the correct type.
        unsafe {
            util::check_lx_errno(libc::syscall(
                libc::SYS_renameat2,
                self.root.as_raw_fd(),
                path.as_ptr(),
                self.root.as_raw_fd(),
                new_path.as_ptr(),
                flags,
            ) as libc::c_int)?;
        }

        Ok(())
    }

    pub fn link(&self, path: &Path, new_path: &Path) -> lx::Result<()> {
        assert!(path.is_relative());
        assert!(new_path.is_relative());
        let path = util::path_to_cstr(path)?;
        let new_path = util::path_to_cstr(new_path)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::linkat(
                self.root.as_raw_fd(),
                path.as_ptr(),
                self.root.as_raw_fd(),
                new_path.as_ptr(),
                0,
            ))?;
        }

        Ok(())
    }

    pub fn link_stat(&self, path: &Path, new_path: &Path) -> lx::Result<lx::Stat> {
        self.link(path, new_path)?;
        self.lstat(new_path)
    }

    pub fn stat_fs(&self, path: &Path) -> lx::Result<lx::StatFs> {
        assert!(path.is_relative());
        let path = self.full_path(path)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        let stat_fs = unsafe {
            let mut stat_fs = mem::zeroed();
            util::check_lx_errno(libc::statfs(path.as_ptr(), &mut stat_fs))?;
            stat_fs
        };

        Ok(util::libc_stat_fs_to_lx_stat_fs(stat_fs))
    }

    pub fn set_xattr(
        &self,
        path: &Path,
        name: &lx::LxStr,
        value: &[u8],
        flags: i32,
    ) -> lx::Result<()> {
        assert!(path.is_relative());

        // There is no *at version of the xattr APIs.
        let path = self.full_path(path)?;
        let name = util::create_cstr(name.as_bytes())?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast::<ffi::c_void>(),
                value.len(),
                flags,
            ))?;
        }

        Ok(())
    }

    pub fn get_xattr(
        &self,
        path: &Path,
        name: &lx::LxStr,
        value: Option<&mut [u8]>,
    ) -> lx::Result<usize> {
        assert!(path.is_relative());

        // There is no *at version of the xattr APIs.
        let path = self.full_path(path)?;
        let name = util::create_cstr(name.as_bytes())?;

        // Set the pointer to NULL if no value buffer is provided, to query the attribute's size.
        let (value_ptr, size) = if let Some(value) = value {
            (value.as_mut_ptr(), value.len())
        } else {
            (std::ptr::null_mut(), 0)
        };

        // SAFETY: Calling C API as documented, with no special requirements.
        let size = unsafe {
            util::check_lx_errno(libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value_ptr.cast::<ffi::c_void>(),
                size,
            ))?
        };

        // Size is guaranteed positive after the check.
        Ok(size as usize)
    }

    pub fn list_xattr(&self, path: &Path, list: Option<&mut [u8]>) -> lx::Result<usize> {
        assert!(path.is_relative());

        // There is no *at version of the xattr APIs.
        let path = self.full_path(path)?;

        // Set the list pointer to NULL if no list buffer was provided, to query the size.
        let (list_ptr, size) = if let Some(list) = list {
            (list.as_mut_ptr(), list.len())
        } else {
            (std::ptr::null_mut(), 0)
        };

        // SAFETY: Calling C API as documented, with no special requirements.
        let size = unsafe {
            util::check_lx_errno(libc::llistxattr(path.as_ptr(), list_ptr.cast(), size))?
        };

        // Size is guaranteed positive after the check.
        Ok(size as usize)
    }

    pub fn remove_xattr(&self, path: &Path, name: &lx::LxStr) -> lx::Result<()> {
        assert!(path.is_relative());

        // There is no *at version of the xattr APIs.
        let path = self.full_path(path)?;
        let name = util::create_cstr(name.as_bytes())?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::lremovexattr(path.as_ptr(), name.as_ptr()))?;
        }

        Ok(())
    }

    fn full_path(&self, path: &Path) -> lx::Result<ffi::CString> {
        let mut full_path = util::get_fd_path(&self.root)?;
        full_path.push(path);
        util::path_to_cstr(&full_path)
    }
}

// Unix implementation of LxFile.
// See crate::LxFile for more detailed comments.
pub struct LxFile {
    fd: std::fs::File,
    enumerator: Option<util::DirectoryEnumerator>,
}

impl LxFile {
    pub fn fstat(&self) -> lx::Result<lx::StatEx> {
        // SAFETY: Calling C API as documented, with no special requirements.
        let statx = unsafe {
            let mut statx = mem::zeroed();
            util::check_lx_errno(libc::statx(
                self.fd.as_raw_fd(),
                "",
                libc::AT_EMPTY_PATH,
                libc::STATX_BASIC_STATS | libc::STATX_BTIME,
                &mut statx,
            ))?;
            statx
        };

        Ok(util::libc_statx_to_lx_statex(statx))
    }

    pub fn set_attr(&self, attr: SetAttributes) -> lx::Result<()> {
        util::set_attr(&self.fd, None, &attr)
    }

    pub fn pread(&self, buffer: &mut [u8], offset: lx::off_t) -> lx::Result<usize> {
        // SAFETY: Calling C API as documented, with no special requirements.
        let size = unsafe {
            util::check_lx_errno(libc::pread(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast::<ffi::c_void>(),
                buffer.len(),
                offset,
            ))?
        };

        // After checking for error, size is guaranteed positive.
        Ok(size as usize)
    }

    pub fn pwrite(&self, buffer: &[u8], offset: lx::off_t, _: lx::uid_t) -> lx::Result<usize> {
        // Linux will clear the set-user-ID and set-group-ID version on write, so unlike the
        // Windows version, that doesn't need to be done here explicitly.
        // SAFETY: Calling C API as documented, with no special requirements.
        let size = unsafe {
            util::check_lx_errno(libc::pwrite(
                self.fd.as_raw_fd(),
                buffer.as_ptr() as *mut ffi::c_void,
                buffer.len(),
                offset,
            ))?
        };

        // After checking for error, size is guaranteed positive.
        Ok(size as usize)
    }

    pub fn read_dir<F>(&mut self, offset: lx::off_t, mut callback: F) -> lx::Result<()>
    where
        F: FnMut(lx::DirEntry) -> lx::Result<bool>,
    {
        if self.enumerator.is_none() {
            // The fd must be cloned because fdopendir takes ownership of the fd. This could be
            // avoided by moving the fd out of self.fd.
            // TODO: Use an enum with an "invalid" state to allow this.
            self.enumerator = Some(util::DirectoryEnumerator::new(self.fd.try_clone()?)?);
        }

        let enumerator = self.enumerator.as_mut().unwrap();
        enumerator.seek(offset);
        for entry in enumerator {
            let result = callback(entry?)?;
            if !result {
                break;
            }
        }

        Ok(())
    }

    pub fn fsync(&self, data_only: bool) -> lx::Result<()> {
        // SAFETY: Calling C APIs as documented, with no special requirements.
        unsafe {
            if data_only {
                util::check_lx_errno(libc::fdatasync(self.fd.as_raw_fd()))?;
            } else {
                util::check_lx_errno(libc::fsync(self.fd.as_raw_fd()))?;
            }
        }

        Ok(())
    }
}
