// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]
// UNSAFETY: Calling (u)mount.
#![expect(unsafe_code)]

use super::Fuse;
use crate::reply::ReplySender;
use crate::request::*;
use crate::session::Session;
use crate::util;
use std::ffi;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::unix::prelude::*;
use std::path::Path;

/// A simple driver for a FUSE session using `/dev/fuse`.
///
/// Since this library is primarily intended for virtio-fs, `/dev/fuse` support is for testing
/// purposes only, and the functionality is limited.
pub struct Connection {
    fuse_dev: fs::File,
}

impl Connection {
    /// Creates a new `Connection` by mounting a file system.
    pub fn mount(mount_point: impl AsRef<Path>) -> lx::Result<Self> {
        // Open an fd to /dev/fuse.
        let fuse_dev = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")?;

        // Set up the mount options (currently, these can't be customized)
        let options = format!(
            "fd={},rootmode=40000,user_id=0,group_id=0",
            fuse_dev.as_raw_fd()
        );

        // Perform the mount.
        let options = util::create_cstr(options)?;
        let target = util::create_cstr(mount_point.as_ref().as_os_str().as_bytes())?;

        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            check_lx_errno(libc::mount(
                util::create_cstr("none")?.as_ptr(),
                target.as_ptr(),
                util::create_cstr("fuse.test")?.as_ptr(),
                0,
                options.as_ptr().cast::<ffi::c_void>(),
            ))?;
        }

        Ok(Self { fuse_dev })
    }

    // Unmount a file system.
    pub fn unmount(mount_point: impl AsRef<Path>, flags: i32) -> lx::Result<()> {
        // SAFETY: Calling C API as documented, with no special requirements.
        unsafe {
            check_lx_errno(libc::umount2(
                util::create_cstr(mount_point.as_ref().as_os_str().as_bytes())?.as_ptr(),
                flags,
            ))?;
        }

        Ok(())
    }

    /// Create a FUSE session and run it until the file system is unmounted.
    pub fn run<T: 'static + Fuse + Send + Sync>(&mut self, fs: T) -> lx::Result<()> {
        // TODO: the size of the buffer should be adjusted based on max_write.
        let mut buffer = vec![0u8; 1028 * 1024];
        let session = Session::new(fs);

        let mut size = self.read(&mut buffer);
        while size > 0 {
            let request = Request::new(&buffer[..size])?;
            session.dispatch(request, self, None);

            size = self.read(&mut buffer);
        }

        session.destroy();
        Ok(())
    }

    /// Read a message from `/dev/fuse`. An error is assumed to mean that the file system was
    /// unmounted.
    fn read(&mut self, buffer: &mut [u8]) -> usize {
        match self.fuse_dev.read(buffer) {
            Ok(size) => size,
            Err(e) => {
                tracing::warn!(
                    len = buffer.len(),
                    error = &e as &dyn std::error::Error,
                    "/dev/fuse read failed",
                );
                0
            }
        }
    }
}

impl ReplySender for Connection {
    fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let size = self.fuse_dev.write_vectored(bufs)?;
        if size < bufs.iter().map(|s| s.len()).sum() {
            return Err(io::Error::other("Failed to write all data"));
        }

        Ok(())
    }
}

// Return an lx::Result if a libc return value is negative. Otherwise, return the value.
fn check_lx_errno<T: PartialOrd<T> + Default>(result: T) -> lx::Result<T> {
    if result < Default::default() {
        Err(lx::Error::last_os_error())
    } else {
        Ok(result)
    }
}
