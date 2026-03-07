// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::SetAttributes;
use crate::SetTime;
use std::ffi;
use std::mem;
use std::os::unix::prelude::*;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;

// Wrapper around a C DIR* used for Unix file enumeration.
pub struct DirectoryEnumerator {
    dir: ptr::NonNull<libc::DIR>,
}

unsafe impl Send for DirectoryEnumerator {}
unsafe impl Sync for DirectoryEnumerator {}

impl DirectoryEnumerator {
    // Create a new enumerator by taking ownership of a file descriptor.
    pub fn new(fd: impl Into<OwnedFd>) -> lx::Result<Self> {
        // SAFETY: By requiring an OwnedFd we take ownership of the fd and give it away to fdopendir, as per its
        // documented requirements.
        let dir = check_lx_ptr(unsafe { libc::fdopendir(fd.into().into_raw_fd()) })?;
        Ok(Self { dir })
    }

    // Seek to a specific offset.
    pub fn seek(&mut self, offset: lx::off_t) {
        // BUGBUG: offset can not be an arbitrary number, it must be a value returned from previous filesystem calls.
        unsafe {
            if offset == 0 {
                libc::rewinddir(self.dir.as_mut());
            } else {
                libc::seekdir(self.dir.as_mut(), offset);
            }
        }
    }
}

impl AsFd for DirectoryEnumerator {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: Safe because we are returning a borrowed fd, the DIR structure still owns it.
        unsafe { BorrowedFd::borrow_raw(libc::dirfd(self.dir.as_ptr())) }
    }
}

impl Iterator for DirectoryEnumerator {
    type Item = lx::Result<lx::DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        // SAFETY: Following the contract for readdir (setting errno to 0 before calling, validating return value).
        // We are only looking for our set errno value within this function, so thread-safety is not a concern.
        let entry = unsafe {
            // Errno must be reset so the end of enumeration can be detected.
            set_errno(0);
            let entry = libc::readdir(self.dir.as_mut());
            if entry.is_null() {
                // If errno is still zero, it's not an error but the end of the directory.
                let errno = lx::Error::last_os_error();
                if errno.value() == 0 {
                    return None;
                } else {
                    return Some(Err(errno));
                }
            }
            *entry
        };

        // Find the NULL terminator and convert into an LxString.
        let index = entry.d_name.iter().position(|c| *c == 0);
        let name = if let Some(index) = index {
            lx::LxString::from_vec(entry.d_name[..index].iter().map(|c| *c as _).collect())
        } else {
            // The name must have a NULL terminator.
            return Some(Err(lx::Error::EIO));
        };

        Some(Ok(lx::DirEntry {
            name,
            inode_nr: entry.d_ino,
            offset: entry.d_off,
            file_type: entry.d_type,
        }))
    }
}

impl Drop for DirectoryEnumerator {
    fn drop(&mut self) {
        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            libc::closedir(self.dir.as_mut());
        }
    }
}

// Helper to create a CString from a Path.
pub fn path_to_cstr(path: &Path) -> lx::Result<ffi::CString> {
    create_cstr(path.as_os_str().as_bytes())
}

// Helper to create a CString from a utf-8 vector and return an lx::Result if it fails.
pub fn create_cstr(value: impl Into<Vec<u8>>) -> lx::Result<ffi::CString> {
    ffi::CString::new(value).map_err(|_| lx::Error::EINVAL)
}

// Return an lx::Result if a libc return value is negative. Otherwise, return the value.
pub fn check_lx_errno<T: PartialOrd<T> + Default>(result: T) -> lx::Result<T> {
    if result < Default::default() {
        Err(lx::Error::last_os_error())
    } else {
        Ok(result)
    }
}

// Checks if a pointer returned from a libc function is NULL, and returns an lx::Result if it is.
pub fn check_lx_ptr<T>(result: *mut T) -> lx::Result<ptr::NonNull<T>> {
    ptr::NonNull::new(result).ok_or_else(lx::Error::last_os_error)
}

/// Change the value of errno.
/// # Safety
///
/// Errno is thread-local, the caller must be sure they are not attempting to observe their set value on a different thread.
pub unsafe fn set_errno(error: i32) {
    // SAFETY: Calling C API as documented, with no special requirements.
    unsafe {
        *libc::__errno_location() = error;
    }
}

pub fn libc_stat_fs_to_lx_stat_fs(stat_fs: libc::statfs) -> lx::StatFs {
    // SAFETY: lx::StatFs is identical to libc's version, and padding bytes are not exposed, so just transmute it.
    // N.B. This call won't compile if the two aren't the same size.
    unsafe { mem::transmute(stat_fs) }
}

// Wrapper around openat that allows reopening file descriptors by specifying an empty path.
pub fn openat(
    dirfd: &std::fs::File,
    path: &Path,
    flags: i32,
    options: Option<crate::LxCreateOptions>,
) -> lx::Result<std::fs::File> {
    if path.as_os_str().is_empty() {
        return reopen(dirfd, flags);
    }

    let mode = options.unwrap_or_default().mode;
    let path = path_to_cstr(path)?;

    // SAFETY: Calling C API as documented, with no special requirements.
    unsafe {
        let fd = check_lx_errno(libc::openat(dirfd.as_raw_fd(), path.as_ptr(), flags, mode))?;

        Ok(std::fs::File::from_raw_fd(fd))
    }
}

// Reopen an existing file descriptor with new flags.
pub fn reopen(fd: &std::fs::File, flags: i32) -> lx::Result<std::fs::File> {
    // If O_NOFOLLOW is not set, open the /proc/self/fd link directly. Otherwise, resolve the
    // target and open that.
    let path = if flags & lx::O_NOFOLLOW != 0 {
        get_fd_path(fd)?
    } else {
        get_proc_fd_path(fd)
    };

    let path = path_to_cstr(&path)?;

    // SAFETY: Calling C API as documented, with no special requirements.
    unsafe {
        let fd = check_lx_errno(libc::open(path.as_ptr(), flags))?;
        Ok(std::fs::File::from_raw_fd(fd))
    }
}

// Get the path opened by a file descriptor.
pub fn get_fd_path(fd: &std::fs::File) -> lx::Result<PathBuf> {
    let fd_path = get_proc_fd_path(fd);
    let target = std::fs::read_link(fd_path)?;
    Ok(target)
}

// Get the path to a file descriptor in /proc/self/fd.
fn get_proc_fd_path(fd: &std::fs::File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// Apply attributes either to the fd, or to a subpath of the fd.
///
/// Linux will remove the set-user-ID and set-group-ID bits as appropriate, so unlike the Windows
/// version this doesn't need to be done explicitly.
pub fn set_attr(fd: &std::fs::File, path: Option<&Path>, attr: &SetAttributes) -> lx::Result<()> {
    unsafe {
        // Ctime is updated by most of the operations below, so don't explicitly
        // update it if not needed.
        let mut need_ctime_update = !attr.ctime.is_omit();

        if let Some(size) = attr.size {
            if let Some(path) = path {
                // We must open the file since truncate always follows symlinks.
                let file = openat(fd, path, libc::O_WRONLY | libc::O_NOFOLLOW, None)?;
                check_lx_errno(libc::ftruncate(file.as_raw_fd(), size))?;
            } else {
                check_lx_errno(libc::ftruncate(fd.as_raw_fd(), size))?;
            }

            need_ctime_update = false;
        }

        let cpath = if let Some(path) = path {
            path_to_cstr(path)?
        } else {
            ffi::CString::default()
        };

        if let Some(mode) = attr.mode {
            if path.is_none() {
                check_lx_errno(libc::fchmod(fd.as_raw_fd(), mode))?;
            } else {
                // TODO: This will follow symlinks, but AT_SYMLINK_NOFOLLOW isn't supported.
                // There isn't a good way around this. Fchmod doesn't work on files opened with O_PATH,
                // so opening the file would require permissions that are normally not needed for chmod.
                // Checking whether it's a symlink beforehand is still susceptible to TOCTOU attack.
                check_lx_errno(libc::fchmodat(fd.as_raw_fd(), cpath.as_ptr(), mode, 0))?;
            }

            need_ctime_update = false;
        }

        if attr.uid.is_some() || attr.gid.is_some() {
            let uid = attr.uid.unwrap_or(lx::UID_INVALID);
            let gid = attr.gid.unwrap_or(lx::GID_INVALID);
            if path.is_none() {
                check_lx_errno(libc::fchown(fd.as_raw_fd(), uid, gid))?;
            } else {
                check_lx_errno(libc::fchownat(
                    fd.as_raw_fd(),
                    cpath.as_ptr(),
                    uid,
                    gid,
                    libc::AT_SYMLINK_NOFOLLOW,
                ))?;
            }

            need_ctime_update = false;
        }

        if !attr.atime.is_omit() || !attr.mtime.is_omit() {
            let times = [
                set_time_to_timespec(&attr.atime),
                set_time_to_timespec(&attr.mtime),
            ];

            if path.is_none() {
                check_lx_errno(libc::futimens(fd.as_raw_fd(), times.as_ptr()))?;
            } else {
                check_lx_errno(libc::utimensat(
                    fd.as_raw_fd(),
                    cpath.as_ptr(),
                    times.as_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                ))?;
            }

            need_ctime_update = false;
        }

        // If a ctime update was requested and didn't already happen, perform a no-op operation that
        // has a ctime update as a side-effect.
        // N.B. There is no way to explicitly set ctime, so any SetTime::Some also sets the value to
        //      the current time.
        if need_ctime_update {
            if path.is_none() {
                check_lx_errno(libc::fchown(
                    fd.as_raw_fd(),
                    lx::UID_INVALID,
                    lx::GID_INVALID,
                ))?;
            } else {
                check_lx_errno(libc::fchownat(
                    fd.as_raw_fd(),
                    cpath.as_ptr(),
                    lx::UID_INVALID,
                    lx::GID_INVALID,
                    libc::AT_SYMLINK_NOFOLLOW,
                ))?;
            }
        }

        Ok(())
    }
}

/// Create a timespec with either omit, now or a value.
fn set_time_to_timespec(time: &SetTime) -> libc::timespec {
    match time {
        SetTime::Omit => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        SetTime::Set(duration) => libc::timespec {
            tv_sec: duration.as_secs() as _,
            tv_nsec: duration.subsec_nanos() as _,
        },
        SetTime::Now => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
    }
}
