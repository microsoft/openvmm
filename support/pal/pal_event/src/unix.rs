// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(unix)]

use crate::Event;
use std::os::unix::prelude::*;

pub type Inner = OwnedFd;

/// Runs f() until it stop failing with EINTR (as indicated by errno).
fn while_eintr<F, R>(mut f: F) -> std::io::Result<R>
where
    F: FnMut() -> std::io::Result<R>,
{
    loop {
        match f() {
            Err(err) if err.raw_os_error() == Some(libc::EINTR) => {}
            r => break r,
        }
    }
}

fn syscall_result<T: PartialOrd + Default>(result: T) -> std::io::Result<T> {
    if result >= T::default() {
        Ok(result)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
mod eventfd {
    use super::syscall_result;
    use super::while_eintr;
    use crate::Event;

    use std::os::unix::prelude::*;

    impl Event {
        pub(crate) fn new_inner() -> std::io::Result<Self> {
            // Create an event fd.
            // SAFETY: calling C APIs as documented, with no special requirements, and validating its
            // return value before passing it to from_raw_fd.
            let fd = unsafe {
                let fd = syscall_result(libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC))?;
                OwnedFd::from_raw_fd(fd)
            };
            Ok(Self(fd))
        }

        pub(crate) fn try_wait_inner(&self) -> bool {
            let mut c: u64 = 0;
            // SAFETY: fd holds a valid and open file descriptor.
            let n = while_eintr(|| unsafe {
                syscall_result(libc::read(
                    self.0.as_raw_fd(),
                    std::ptr::from_mut(&mut c).cast::<std::ffi::c_void>(),
                    size_of_val(&c),
                ))
            });
            match n {
                Ok(n) => {
                    assert!(n == size_of_val(&c) as isize);
                    true
                }
                Err(err) => {
                    assert_eq!(err.raw_os_error(), Some(libc::EAGAIN));
                    false
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod fifo {
    use super::syscall_result;
    use super::while_eintr;
    use crate::Event;
    use std::fs::File;

    use std::os::unix::prelude::*;

    impl Event {
        // Create an anonymous FIFO to emulate a Linux eventfd.
        pub(crate) fn new_inner() -> std::io::Result<Self> {
            // Create a random path.
            let mut path = std::env::temp_dir();
            let mut val = [0; 16];
            getrandom::getrandom(&mut val).unwrap();
            path.push(u128::from_ne_bytes(val).to_string());

            // Create the FIFO.
            let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: calling C APIs as documented, with no special requirements.
            syscall_result(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) })?;

            // Open the FIFO.
            let fifo = File::options()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)?;

            // Unlink it so that it can't be opened again.
            let _ = std::fs::remove_file(&path);
            Ok(Self(fifo.into()))
        }

        /// Consumes the ready state of an emulated eventfd on non-Linux platforms.
        ///
        /// Reads using a large buffer in case the underlying FIFO was signaled multiple
        /// times.
        pub(crate) fn try_wait_inner(&self) -> bool {
            let mut c = [0u8; 512];
            // SAFETY: fd holds a valid and open file descriptor.
            let n = while_eintr(|| unsafe {
                syscall_result(libc::read(
                    self.0.as_raw_fd(),
                    c.as_mut_ptr().cast::<std::ffi::c_void>(),
                    size_of_val(&c),
                ))
            });
            match n {
                Ok(_) => true,
                Err(err) => {
                    assert_eq!(err.raw_os_error(), Some(libc::EAGAIN));
                    false
                }
            }
        }
    }
}

impl Event {
    pub(crate) fn signal_inner(&self) {
        let c: u64 = 1;
        // SAFETY: fd holds a valid and open file descriptor.
        let r = unsafe {
            syscall_result(libc::write(
                self.0.as_raw_fd(),
                std::ptr::from_ref::<u64>(&c).cast::<libc::c_void>(),
                size_of_val(&c),
            ))
        };
        match r {
            Ok(n) if n == size_of_val(&c) as isize => {}
            Err(err) if err.raw_os_error() == Some(libc::EAGAIN) => {}
            r => {
                panic!("unexpected event write result: {:?}", r);
            }
        }
    }

    fn poll(&self) {
        let mut pollfds = [libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: fd holds a valid and open file descriptor.
        while_eintr(|| unsafe { syscall_result(libc::poll(pollfds.as_mut_ptr(), 1, !0)) }).unwrap();
    }

    pub(crate) fn wait_inner(&self) {
        while !self.try_wait_inner() {
            self.poll();
        }
    }
}

impl Clone for Event {
    fn clone(&self) -> Self {
        Self(self.0.try_clone().expect("out of resources dup eventfd"))
    }
}

impl From<OwnedFd> for Event {
    fn from(fd: OwnedFd) -> Self {
        Self(fd)
    }
}

impl From<Event> for OwnedFd {
    fn from(event: Event) -> Self {
        event.0
    }
}

impl AsFd for Event {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(feature = "mesh")]
mesh_protobuf::os_resource!(Event, OwnedFd);
