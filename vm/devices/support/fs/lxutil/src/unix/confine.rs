// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolution of volume-relative paths that never follows a symbolic link.
//!
//! A confined volume opens the parent directory of every path strictly beneath its root, refusing
//! symbolic links in every component, and then applies the requested operation to the final
//! component through that pinned directory. A concurrent rename or symbolic link therefore cannot
//! redirect an operation outside the volume.

use super::util;
use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::mem::MaybeUninit;
use std::os::unix::prelude::*;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Flags for the `O_PATH` handle of an intermediate directory.
const DIRECTORY_FLAGS: i32 = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC;

/// The version 0 layout of `struct open_how` from `linux/openat2.h`.
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// How the parent directory of a confined path is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolver {
    /// One `openat2` call that rejects symbolic links and paths that leave the root.
    Openat2,
    /// One `openat(O_NOFOLLOW)` call per component, for kernels or sandboxes without `openat2`.
    Walk,
}

impl Resolver {
    /// Selects `openat2` when the kernel and any seccomp policy allow it.
    pub(crate) fn probe(root: &File) -> Self {
        match openat2_directory(root.as_raw_fd(), c".") {
            Ok(_) => Self::Openat2,
            Err(error) => {
                tracing::debug!(
                    error = error.value(),
                    "openat2 is unavailable; resolving confined paths one component at a time"
                );
                Self::Walk
            }
        }
    }
}

/// The directory that holds the final component of a confined path.
pub(crate) enum ParentDirectory<'a> {
    /// The volume root, for single-component paths and the root itself.
    Root(&'a File),
    /// An `O_PATH` handle opened without following any symbolic link.
    Opened(OwnedFd),
}

impl AsRawFd for ParentDirectory<'_> {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Root(root) => root.as_raw_fd(),
            Self::Opened(directory) => directory.as_raw_fd(),
        }
    }
}

/// A path split into a pinned directory and the name of its final component.
pub(crate) struct AtPath<'a> {
    /// The directory that holds `name`.
    pub(crate) directory: ParentDirectory<'a>,
    /// The final component, or an empty string for the volume root itself.
    pub(crate) name: CString,
}

impl AtPath<'_> {
    /// Returns whether this refers to the volume root rather than an entry in a directory.
    pub(crate) fn is_root(&self) -> bool {
        self.name.is_empty()
    }

    /// Returns a `/proc` path that reaches the final component through the pinned directory.
    ///
    /// Path-based system calls that do not follow a final symbolic link (such as the `l*xattr`
    /// family) can use it without resolving any ancestor from the host root again. The returned
    /// path is only valid while `self` keeps the directory open.
    pub(crate) fn proc_path(&self) -> lx::Result<CString> {
        let mut path = format!("/proc/self/fd/{}/", self.directory.as_raw_fd()).into_bytes();
        if self.is_root() {
            path.push(b'.');
        } else {
            path.extend_from_slice(self.name.as_bytes());
        }
        util::create_cstr(path)
    }
}

/// Splits a volume-relative `path` and opens its parent directory beneath `root` without following
/// any symbolic link.
///
/// A path that crosses a symbolic link fails with `ELOOP`. The final component is not inspected.
pub(crate) fn resolve<'a>(
    root: &'a File,
    resolver: Resolver,
    path: &Path,
) -> lx::Result<AtPath<'a>> {
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(lx::Error::EINVAL);
        };
        components.push(component);
    }

    let Some(name) = components.pop() else {
        return Ok(AtPath {
            directory: ParentDirectory::Root(root),
            name: CString::default(),
        });
    };

    let name = util::create_cstr(name.as_bytes())?;
    let Some((first, rest)) = components.split_first() else {
        return Ok(AtPath {
            directory: ParentDirectory::Root(root),
            name,
        });
    };

    let directory = match resolver {
        Resolver::Openat2 => {
            let parent: PathBuf = components.iter().collect();
            openat2_directory(root.as_raw_fd(), &util::path_to_cstr(&parent)?)?
        }
        Resolver::Walk => {
            let mut directory = open_component(root.as_raw_fd(), first)?;
            for component in rest {
                directory = open_component(directory.as_raw_fd(), component)?;
            }
            directory
        }
    };

    Ok(AtPath {
        directory: ParentDirectory::Opened(directory),
        name,
    })
}

/// Opens `path` beneath `directory` as an `O_PATH` directory handle, rejecting every symbolic
/// link and any escape from `directory`.
fn openat2_directory(directory: RawFd, path: &CStr) -> lx::Result<OwnedFd> {
    let how = OpenHow {
        flags: DIRECTORY_FLAGS as u64,
        mode: 0,
        resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS,
    };

    // SAFETY: `path` is NUL-terminated and `how` uses the kernel's version 0 layout, whose size
    // is passed explicitly.
    let result = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory,
            path.as_ptr(),
            std::ptr::from_ref(&how),
            size_of::<OpenHow>(),
        )
    };

    let fd = util::check_lx_errno(result)?;
    let fd = RawFd::try_from(fd).map_err(|_| lx::Error::EIO)?;
    // SAFETY: The kernel returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Opens one directory component without following a symbolic link.
fn open_component(directory: RawFd, name: &OsStr) -> lx::Result<OwnedFd> {
    let name = util::create_cstr(name.as_bytes())?;

    // SAFETY: Calling C API as documented, with no special requirements.
    let result =
        unsafe { libc::openat(directory, name.as_ptr(), DIRECTORY_FLAGS | libc::O_NOFOLLOW) };

    match util::check_lx_errno(result) {
        // SAFETY: The kernel returned a new descriptor that nothing else owns.
        Ok(fd) => Ok(unsafe { OwnedFd::from_raw_fd(fd) }),
        // O_NOFOLLOW reports a symbolic link as "not a directory"; report it like openat2 does.
        Err(error) if error.value() == lx::ENOTDIR && is_symlink(directory, &name) => {
            Err(lx::Error::ELOOP)
        }
        Err(error) => Err(error),
    }
}

/// Returns whether `name` in `directory` is a symbolic link.
fn is_symlink(directory: RawFd, name: &CStr) -> bool {
    let mut stat = MaybeUninit::<libc::stat>::uninit();

    // SAFETY: `stat` is valid for writes and `name` is NUL-terminated.
    let result = unsafe {
        libc::fstatat(
            directory,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };

    // SAFETY: fstatat initialized `stat` because it succeeded.
    result == 0 && unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFLNK
}
