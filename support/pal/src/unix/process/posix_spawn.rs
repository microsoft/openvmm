// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process spawning with posix_spawn.

use super::Builder;
use super::Child;
use super::FdOp;
use std::ffi::CString;
use std::io;
use std::ptr::null;
use std::ptr::null_mut;

struct FileActions(Option<libc::posix_spawn_file_actions_t>);

impl FileActions {
    fn new() -> Self {
        Self(None)
    }

    fn init(&mut self) -> io::Result<()> {
        assert!(self.0.is_none());
        let actions = self.0.insert(null_mut());
        // SAFETY: calling the libc function as the documentation
        // requires. No memory aliasing is possible, the data is
        // initialized with a valid bit pattern.
        let r = unsafe { libc::posix_spawn_file_actions_init(actions) };
        if r != 0 {
            self.0 = None;
            return Err(io::Error::from_raw_os_error(r));
        }
        Ok(())
    }

    fn add_close(&mut self, fd: i32) -> io::Result<()> {
        // SAFETY: actions was initialized in init().
        let r = unsafe { libc::posix_spawn_file_actions_addclose(self.0.as_mut().unwrap(), fd) };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r));
        }
        Ok(())
    }

    fn add_dup2(&mut self, fd: i32, newfd: i32) -> io::Result<()> {
        // SAFETY: actions was initialized in init().
        let r =
            unsafe { libc::posix_spawn_file_actions_adddup2(self.0.as_mut().unwrap(), fd, newfd) };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r));
        }
        Ok(())
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        if let Some(actions) = &mut self.0 {
            // SAFETY: initialized in init(), no more references remain
            unsafe {
                libc::posix_spawn_file_actions_destroy(actions);
            }
        }
    }
}

impl Builder<'_> {
    pub(super) fn spawn_internal(
        &self,
        envp: &[CString],
        fd_ops: &mut [(i32, FdOp)],
    ) -> io::Result<Child> {
        let mut pid = 0;

        let mut actions = FileActions::new();
        actions.init()?;
        for (fd, op) in fd_ops.iter().copied() {
            match op {
                FdOp::Close => actions.add_close(fd)?,
                FdOp::Dup(oldfd) => actions.add_dup2(oldfd, fd)?,
            }
        }

        // Build the null-terminated arrays for spawn.
        let argv = super::c_slice_to_pointers(&self.argv);
        let envp = super::c_slice_to_pointers(envp);

        // SAFETY: calling the libc function as the documentation
        // requires. The Rust function takes `&self`, and memory
        // aliasing is possible for the few fields of `self` that
        // are exposed to the unsafe context. This is safe as no
        // other part of the code is working on these fields
        // concurrently with this unsafe block thereby all the
        // compiler invariants and the code correctness will be
        // upheld. The kernel does not modify the data buffers
        // passed to that call.
        //
        // The data is initialized with valid bit patterns, too.
        let r = unsafe {
            libc::posix_spawn(
                &mut pid,
                self.executable.as_ptr(),
                actions.0.as_ref().map_or(null(), |p| p),
                null(),
                argv.as_ptr().cast(),
                envp.as_ptr().cast(),
            )
        };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r));
        }
        Ok(Child { pid, status: None })
    }
}
