// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AF_VSOCK support.

// UNSAFETY: Calling libc functions on a raw socket fd.
#![allow(unsafe_code)]

use crate::VmListener;
use crate::VmSocket;
use crate::VmStream;
use mesh::payload::os_resource;
use socket2::Domain;
use socket2::SockAddr;
use socket2::Socket;
use socket2::Type;
use std::io;
use std::os::unix::prelude::*;
use std::time::Duration;

#[derive(Debug)]
pub struct Address {
    pub(crate) cid: u32,
    pub(crate) port: u32,
}

impl Address {
    pub fn new(cid: u32, port: u32) -> Self {
        Self { cid, port }
    }

    pub fn vsock_any(port: u32) -> Self {
        Self::new(!0, port)
    }

    pub fn vsock_host(port: u32) -> Self {
        Self::new(2, port)
    }

    pub fn into_sock_addr(self) -> SockAddr {
        SockAddr::vsock(self.cid, self.port)
    }

    pub fn try_from_sock_addr(addr: &SockAddr) -> Option<Self> {
        let (cid, port) = addr.as_vsock_address()?;
        Some(Self::new(cid, port))
    }
}

impl VmSocket {
    pub(crate) fn new_inner() -> io::Result<Self> {
        Ok(Self(Socket::new(Domain::VSOCK, Type::STREAM, None)?))
    }

    /// Sets the connection timeout for this socket.
    pub fn set_connect_timeout(&self, duration: Duration) -> io::Result<()> {
        let timeout = libc::timeval {
            tv_sec: duration
                .as_secs()
                .try_into()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
            tv_usec: duration.subsec_micros().into(),
        };

        // SAFETY: Calling a VSOCK-specific option on a VSOCK socket,
        // and passing a valid pointer to a timeval struct.
        unsafe {
            if libc::setsockopt(
                self.as_fd().as_raw_fd(),
                libc::AF_VSOCK,
                6, // SO_VM_SOCKETS_CONNECT_TIMEOUT
                std::ptr::from_ref(&timeout).cast(),
                size_of_val(&timeout) as u32,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

impl AsFd for VmSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl From<VmSocket> for OwnedFd {
    fn from(fd: VmSocket) -> Self {
        fd.0.into()
    }
}

impl From<OwnedFd> for VmSocket {
    fn from(fd: OwnedFd) -> Self {
        Self(fd.into())
    }
}

impl AsFd for VmListener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl From<VmListener> for OwnedFd {
    fn from(fd: VmListener) -> Self {
        fd.0.into()
    }
}

impl From<OwnedFd> for VmListener {
    fn from(fd: OwnedFd) -> Self {
        Self(fd.into())
    }
}

impl AsFd for VmStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl From<VmStream> for OwnedFd {
    fn from(fd: VmStream) -> Self {
        fd.0.into()
    }
}

impl From<OwnedFd> for VmStream {
    fn from(fd: OwnedFd) -> Self {
        Self(fd.into())
    }
}

os_resource!(VmSocket, OwnedFd);
os_resource!(VmStream, OwnedFd);
os_resource!(VmListener, OwnedFd);
