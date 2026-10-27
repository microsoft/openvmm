// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Calling libc file APIs.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks)]

pub(crate) mod path;
mod util;

use crate::SetAttributes;
use lx::StatEx;
use std::ffi;
use std::os::unix::prelude::*;
use std::path::Path;

// Unix implementation of LxVolume.
// See crate::LxVolume for more detailed comments.
pub struct LxVolume {
    root: std::fs::File,
}

impl LxVolume {
    pub fn new(
        root_path: &Path,
        _options: &super::LxVolumeOptions,
    ) -> Result<Self, super::VolumeError> {
        let path = util::path_to_cstr(root_path)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        let root = unsafe {
            // Open a file descriptor to the root to use with "*at" functions.
            let fd = util::check_lx_errno(libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            ))?;

            std::fs::File::from_raw_fd(fd)
        };
        // Check the required syscall and resolution flags before exposing the volume.
        // A missing syscall or a seccomp denial must fail setup, not a later guest request.
        util::openat(&root, Path::new(""), libc::O_PATH | libc::O_DIRECTORY, None)
            .map_err(super::VolumeError::ConstrainedOpen)?;
        Ok(Self { root })
    }

    pub fn supports_stable_file_id(&self) -> bool {
        true
    }

    pub fn lstat(&self, path: &Path) -> lx::Result<StatEx> {
        let parent = util::Parent::new(&self.root, path)?;
        util::stat_at(parent.directory(), parent.name)
    }

    pub fn set_attr(&self, path: &Path, attr: SetAttributes) -> lx::Result<()> {
        let parent = util::Parent::new(&self.root, path)?;
        util::set_attr(
            parent.directory(),
            (!parent.name.as_os_str().is_empty()).then_some(parent.name),
            &attr,
        )
    }

    pub fn set_attr_stat(&self, path: &Path, attr: SetAttributes) -> lx::Result<lx::Stat> {
        let parent = util::Parent::new(&self.root, path)?;
        util::set_attr(
            parent.directory(),
            (!parent.name.as_os_str().is_empty()).then_some(parent.name),
            &attr,
        )?;
        util::stat_at(parent.directory(), parent.name).map(|x| x.into())
    }

    pub fn open(
        &self,
        path: &Path,
        flags: i32,
        options: Option<super::LxCreateOptions>,
    ) -> lx::Result<LxFile> {
        let fd = util::openat(&self.root, path, flags, options)?;

        Ok(LxFile {
            fd,
            enumerator: None,
        })
    }

    pub fn mkdir(&self, path: &Path, options: super::LxCreateOptions) -> lx::Result<()> {
        let parent = util::Parent::new(&self.root, path)?;
        Self::mkdir_at(&parent, options)
    }

    fn mkdir_at(parent: &util::Parent<'_>, options: super::LxCreateOptions) -> lx::Result<()> {
        let path = util::path_to_cstr(parent.name)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::mkdirat(
                parent.directory().as_raw_fd(),
                path.as_ptr(),
                options.mode,
            ))?;
        }

        Ok(())
    }

    pub fn mkdir_stat(&self, path: &Path, options: super::LxCreateOptions) -> lx::Result<lx::Stat> {
        let parent = util::Parent::new(&self.root, path)?;
        Self::mkdir_at(&parent, options)?;
        util::stat_at(parent.directory(), parent.name).map(|x| x.into())
    }

    // The options are entirely ignored on Unix, because uid/gid are never used, and mode isn't
    // used for symlinks.
    pub fn symlink(
        &self,
        path: &Path,
        target: &lx::LxStr,
        _: super::LxCreateOptions,
    ) -> lx::Result<()> {
        let parent = util::Parent::new(&self.root, path)?;
        Self::symlink_at(&parent, target)
    }

    fn symlink_at(parent: &util::Parent<'_>, target: &lx::LxStr) -> lx::Result<()> {
        let path = util::path_to_cstr(parent.name)?;
        let target = util::create_cstr(target.as_bytes())?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::symlinkat(
                target.as_ptr(),
                parent.directory().as_raw_fd(),
                path.as_ptr(),
            ))?;
        }

        Ok(())
    }

    pub fn symlink_stat(
        &self,
        path: &Path,
        target: &lx::LxStr,
        _options: super::LxCreateOptions,
    ) -> lx::Result<lx::Stat> {
        let parent = util::Parent::new(&self.root, path)?;
        Self::symlink_at(&parent, target)?;
        util::stat_at(parent.directory(), parent.name).map(|x| x.into())
    }

    pub fn read_link(&self, path: &Path) -> lx::Result<lx::LxString> {
        let parent = util::Parent::new(&self.root, path)?;
        let mut buffer = [0u8; libc::PATH_MAX as usize];
        let path = util::path_to_cstr(parent.name)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        let size = unsafe {
            util::check_lx_errno(libc::readlinkat(
                parent.directory().as_raw_fd(),
                path.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            ))?
        };

        // Size is guaranteed to be positive after check_lx_errno.
        Ok(lx::LxString::from_vec(Vec::from(&buffer[..size as usize])))
    }

    pub fn unlink(&self, path: &Path, flags: i32) -> lx::Result<()> {
        let parent = util::Parent::new(&self.root, path)?;
        let path = util::path_to_cstr(parent.name)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::unlinkat(
                parent.directory().as_raw_fd(),
                path.as_ptr(),
                flags,
            ))?;
        }

        Ok(())
    }

    pub fn mknod(
        &self,
        path: &Path,
        options: super::LxCreateOptions,
        device_id: lx::dev_t,
    ) -> lx::Result<()> {
        let parent = util::Parent::new(&self.root, path)?;
        Self::mknod_at(&parent, options, device_id)
    }

    fn mknod_at(
        parent: &util::Parent<'_>,
        options: super::LxCreateOptions,
        device_id: lx::dev_t,
    ) -> lx::Result<()> {
        let path = util::path_to_cstr(parent.name)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::mknodat(
                parent.directory().as_raw_fd(),
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
        let parent = util::Parent::new(&self.root, path)?;
        Self::mknod_at(&parent, options, device_id)?;
        util::stat_at(parent.directory(), parent.name).map(|x| x.into())
    }

    pub fn rename(&self, path: &Path, new_path: &Path, flags: u32) -> lx::Result<()> {
        let parent = util::Parent::new(&self.root, path)?;
        let new_parent = util::Parent::new(&self.root, new_path)?;
        Self::rename_at(&parent, &new_parent, flags)
    }

    fn rename_at(
        parent: &util::Parent<'_>,
        new_parent: &util::Parent<'_>,
        flags: u32,
    ) -> lx::Result<()> {
        let path = util::path_to_cstr(parent.name)?;
        let new_path = util::path_to_cstr(new_parent.name)?;

        // renameat2 does not have a wrapper in musl, though it does in glibc. Call it using syscall directly instead.
        // SAFETY: Our arguments are valid for this syscall. We are passing arguments in the
        // correct order and as the correct types. We are casting the return value to the correct type.
        unsafe {
            util::check_lx_errno(libc::syscall(
                libc::SYS_renameat2,
                parent.directory().as_raw_fd(),
                path.as_ptr(),
                new_parent.directory().as_raw_fd(),
                new_path.as_ptr(),
                flags,
            ) as libc::c_int)?;
        }

        Ok(())
    }

    pub fn link(&self, path: &Path, new_path: &Path) -> lx::Result<()> {
        let parent = util::Parent::new(&self.root, path)?;
        let new_parent = util::Parent::new(&self.root, new_path)?;
        Self::link_at(&parent, &new_parent)
    }

    fn link_at(parent: &util::Parent<'_>, new_parent: &util::Parent<'_>) -> lx::Result<()> {
        let path = util::path_to_cstr(parent.name)?;
        let new_path = util::path_to_cstr(new_parent.name)?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::linkat(
                parent.directory().as_raw_fd(),
                path.as_ptr(),
                new_parent.directory().as_raw_fd(),
                new_path.as_ptr(),
                0,
            ))?;
        }

        Ok(())
    }

    pub fn link_stat(&self, path: &Path, new_path: &Path) -> lx::Result<lx::Stat> {
        let parent = util::Parent::new(&self.root, path)?;
        let new_parent = util::Parent::new(&self.root, new_path)?;
        Self::link_at(&parent, &new_parent)?;
        util::stat_at(new_parent.directory(), new_parent.name).map(|x| x.into())
    }

    pub fn stat_fs(&self, path: &Path) -> lx::Result<lx::StatFs> {
        let file = if path.as_os_str().is_empty() {
            None
        } else {
            Some(util::openat(&self.root, path, libc::O_PATH, None)?)
        };
        let file = file.as_ref().unwrap_or(&self.root);

        // SAFETY: Calling C API as documented, with no special requirements.
        let stat_fs = unsafe {
            let mut stat_fs = std::mem::zeroed();
            util::check_lx_errno(libc::fstatfs(file.as_raw_fd(), &mut stat_fs))?;
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
        let parent = util::Parent::new(&self.root, path)?;
        let path = parent.proc_path()?;
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
        let parent = util::Parent::new(&self.root, path)?;
        let path = parent.proc_path()?;
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
        let parent = util::Parent::new(&self.root, path)?;
        let path = parent.proc_path()?;

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
        let parent = util::Parent::new(&self.root, path)?;
        let path = parent.proc_path()?;
        let name = util::create_cstr(name.as_bytes())?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            util::check_lx_errno(libc::lremovexattr(path.as_ptr(), name.as_ptr()))?;
        }

        Ok(())
    }
}

// Unix implementation of LxFile.
// See crate::LxFile for more detailed comments.
pub struct LxFile {
    fd: std::fs::File,
    enumerator: Option<util::DirectoryEnumerator>,
}

impl LxFile {
    pub fn fstat(&self) -> lx::Result<StatEx> {
        util::stat_at(&self.fd, Path::new(""))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use test_with_tracing::test;

    #[test]
    fn volume_creation_checks_openat2() {
        const CHILD_ERRNO: &str = "LXUTIL_TEST_OPENAT2_ERRNO";
        if let Some(errno) = std::env::var_os(CHILD_ERRNO) {
            let errno: u32 = errno.to_str().unwrap().parse().unwrap();
            let root = tempfile::tempdir().unwrap();
            let filter = [
                libc::sock_filter {
                    code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                    jt: 0,
                    jf: 0,
                    k: 0, // seccomp_data.nr.
                },
                libc::sock_filter {
                    code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                    jt: 0,
                    jf: 1,
                    k: libc::SYS_openat2 as u32,
                },
                libc::sock_filter {
                    code: (libc::BPF_RET | libc::BPF_K) as u16,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ERRNO | errno,
                },
                libc::sock_filter {
                    code: (libc::BPF_RET | libc::BPF_K) as u16,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ALLOW,
                },
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr().cast_mut(),
            };
            // SAFETY: In this child only, install a valid filter on the current thread.
            // It allows all syscalls except openat2, which returns the selected errno.
            unsafe {
                util::check_lx_errno(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)).unwrap();
                util::check_lx_errno(libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &program,
                ))
                .unwrap();
            }
            for result in [
                super::super::LxVolume::new(root.path()),
                super::super::LxVolumeOptions::new().new_volume(root.path()),
            ] {
                let error = result
                    .err()
                    .expect("volume setup must reject unavailable constrained opens");
                assert!(matches!(
                    error,
                    super::super::VolumeError::ConstrainedOpen(_)
                ));
                assert!(error.to_string().contains("openat2"));
                assert!(error.to_string().contains("seccomp"));
                let cause = std::error::Error::source(&error)
                    .unwrap()
                    .downcast_ref::<lx::Error>()
                    .expect("the setup error must retain its errno source");
                assert_eq!(cause.value(), errno as i32);
            }
            return;
        }

        for errno in [libc::ENOSYS, libc::EPERM] {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "unix::tests::volume_creation_checks_openat2",
                    "--nocapture",
                ])
                .env(CHILD_ERRNO, errno.to_string())
                .status()
                .unwrap();
            assert!(status.success(), "openat2 errno {errno}: {status}");
        }
    }

    #[test]
    fn namespace_operations_keep_captured_parents() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("source"), b"rename").unwrap();
        std::fs::write(root.path().join("link_source"), b"link").unwrap();
        std::fs::write(root.path().join("dir/file"), b"inside").unwrap();
        let volume = LxVolume::new(root.path(), &super::super::LxVolumeOptions::new()).unwrap();
        let source = util::Parent::new(&volume.root, Path::new("source")).unwrap();
        let link_source = util::Parent::new(&volume.root, Path::new("link_source")).unwrap();
        let destination = util::Parent::new(&volume.root, Path::new("dir/renamed")).unwrap();
        let link = util::Parent::new(&volume.root, Path::new("dir/linked")).unwrap();
        let directory = util::Parent::new(&volume.root, Path::new("dir/newdir")).unwrap();
        let file = util::Parent::new(&volume.root, Path::new("dir/file")).unwrap();
        std::fs::rename(root.path().join("dir"), root.path().join("original")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("dir")).unwrap();

        LxVolume::rename_at(&source, &destination, 0).unwrap();
        LxVolume::link_at(&link_source, &link).unwrap();
        LxVolume::mkdir_at(&directory, super::super::LxCreateOptions::default()).unwrap();
        let path = file.proc_path().unwrap();
        // SAFETY: All pathname, attribute-name, and value buffers are valid for the call.
        unsafe {
            util::check_lx_errno(libc::lsetxattr(
                path.as_ptr(),
                c"user.test".as_ptr(),
                b"inside".as_ptr().cast(),
                6,
                0,
            ))
            .unwrap();
        }
        assert_eq!(
            std::fs::read(root.path().join("original/renamed")).unwrap(),
            b"rename"
        );
        assert_eq!(
            std::fs::read(root.path().join("original/linked")).unwrap(),
            b"link"
        );
        assert!(root.path().join("original/newdir").is_dir());
        assert_eq!(
            volume
                .get_xattr(Path::new("original/file"), "user.test".as_ref(), None)
                .unwrap(),
            6
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn chmod_does_not_follow_final_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("file");
        std::fs::write(&target, b"outside").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("link")).unwrap();
        let volume = LxVolume::new(root.path(), &super::super::LxVolumeOptions::new()).unwrap();
        match volume.set_attr_stat(
            Path::new("link"),
            SetAttributes {
                mode: Some(0o400),
                ..Default::default()
            },
        ) {
            Ok(stat) => assert!(lx::s_islnk(stat.mode)),
            Err(error) => assert_eq!(error, lx::Error::ENOTSUP),
        }
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn renamed_root_is_not_reopened_by_host_path() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        std::fs::write(share.join("file"), b"inside").unwrap();
        std::fs::write(outside.path().join("file"), b"outside").unwrap();
        let volume = LxVolume::new(&share, &super::super::LxVolumeOptions::new()).unwrap();
        std::fs::rename(&share, root.path().join("original")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &share).unwrap();
        let original_inode = volume.lstat(Path::new("")).unwrap().inode_id;
        let reopened = volume
            .open(Path::new(""), lx::O_RDONLY | lx::O_DIRECTORY, None)
            .unwrap();
        assert_eq!(reopened.fstat().unwrap().inode_id, original_inode);
        let file = volume.open(Path::new("file"), lx::O_RDONLY, None).unwrap();
        let mut buffer = [0; 6];
        assert_eq!(file.pread(&mut buffer, 0).unwrap(), 6);
        assert_eq!(&buffer, b"inside");
        volume
            .set_xattr(Path::new(""), "user.test".as_ref(), b"root", 0)
            .unwrap();
        assert_eq!(
            volume
                .get_xattr(Path::new(""), "user.test".as_ref(), None)
                .unwrap(),
            4
        );
    }
}
