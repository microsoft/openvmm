// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for `AF_HYPERV` (on Windows) and `AF_VSOCK` (on Linux) socket families.
//!
//! This crate abstracts over the differences between these and provides unified
//! [`VmStream`] and [`VmListener`] types.

#![cfg(any(windows, target_os = "linux"))]

cfg_if::cfg_if! {
    if #[cfg(windows)] {
        mod af_hyperv;
        use af_hyperv as sys;
    } else if #[cfg(unix)] {
        mod af_vsock;
        use af_vsock as sys;
    }
}

use socket2::SockAddr;
use socket2::Socket;
use std::io;
use std::io::Read;
use std::io::Write;

/// A VM socket address.
#[derive(Debug)]
pub struct VmAddress(sys::Address);

impl std::fmt::Display for VmAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(windows)]
        write!(f, "{}:{}", self.vm_id(), self.service_id())?;
        #[cfg(unix)]
        write!(f, "{}:{}", self.cid(), self.port())?;
        Ok(())
    }
}

impl VmAddress {
    /// Creates a new AF_VSOCK address from `cid` and `port`.
    #[cfg(unix)]
    pub fn vsock(cid: u32, port: u32) -> Self {
        Self(sys::Address::new(cid, port))
    }

    /// Creates a new AF_HYPERV address from `vm_id` and `service_id`.
    #[cfg(windows)]
    pub fn hyperv(vm_id: guid::Guid, service_id: guid::Guid) -> Self {
        Self(sys::Address::new(vm_id, service_id))
    }

    /// Creates a new AF_HYPERV address from `vm_id` and VSOCK `port`.
    #[cfg(windows)]
    pub fn hyperv_vsock(vm_id: guid::Guid, port: u32) -> Self {
        Self(sys::Address::vsock(vm_id, port))
    }

    /// Creates a new AF_HYPERV address referring to any VM and the specified
    /// service ID.
    #[cfg(windows)]
    pub fn hyperv_any(service_id: guid::Guid) -> Self {
        Self(sys::Address::hyperv_any(service_id))
    }

    /// Creates a new AF_HYPERV address referring to the parent VM and the
    /// specified service ID.
    #[cfg(windows)]
    pub fn hyperv_host(service_id: guid::Guid) -> Self {
        Self(sys::Address::hyperv_host(service_id))
    }

    /// Creates a new address referring to any VM, with the specified VSOCK
    /// port.
    pub fn vsock_any(port: u32) -> Self {
        Self(sys::Address::vsock_any(port))
    }

    /// Creates a new address referring to the host, with the specified VSOCK
    /// port.
    pub fn vsock_host(port: u32) -> Self {
        Self(sys::Address::vsock_host(port))
    }

    /// Creates a new address from the specified [`SockAddr`] when the address
    /// is an `AF_HYPERV` (on Windows) or `AF_VSOCK` (on Linux) address.
    pub fn try_from_sock_addr(addr: &SockAddr) -> Option<Self> {
        Some(Self(sys::Address::try_from_sock_addr(addr)?))
    }

    /// Gets the VSOCK CID.
    #[cfg(unix)]
    pub fn cid(&self) -> u32 {
        self.0.cid
    }

    /// Gets the VSOCK port.
    #[cfg(unix)]
    pub fn port(&self) -> u32 {
        self.0.port
    }

    /// Gets the VM ID.
    #[cfg(windows)]
    pub fn vm_id(&self) -> guid::Guid {
        self.0.vm_id
    }

    /// Gets the service ID.
    #[cfg(windows)]
    pub fn service_id(&self) -> guid::Guid {
        self.0.service_id
    }
}

impl From<VmAddress> for SockAddr {
    fn from(address: VmAddress) -> Self {
        address.0.into_sock_addr()
    }
}

/// A VM socket that has not yet been bound.
pub struct VmSocket(Socket);

impl VmSocket {
    /// Creates a new stream socket not bound or connected to anything.
    pub fn new() -> io::Result<Self> {
        Self::new_inner()
    }

    /// Binds the socket to `address`.
    pub fn bind(&mut self, address: VmAddress) -> io::Result<()> {
        self.0.bind(&address.into())?;
        Ok(())
    }

    /// Listens for connections, returning a [`VmListener`].
    pub fn listen(self, backlog: i32) -> io::Result<VmListener> {
        self.0.listen(backlog)?;
        Ok(VmListener(self.0))
    }

    /// Connects to `address`, returning a [`VmStream`].
    pub fn connect(self, address: VmAddress) -> io::Result<VmStream> {
        self.0.connect(&address.into())?;
        Ok(VmStream(self.0))
    }
}

impl From<Socket> for VmSocket {
    fn from(s: Socket) -> Self {
        Self(s)
    }
}

impl From<VmSocket> for Socket {
    fn from(s: VmSocket) -> Self {
        s.0
    }
}

/// A VM socket listener.
#[derive(Debug)]
pub struct VmListener(Socket);

impl VmListener {
    /// Creates a new socket bound to the specified address.
    pub fn bind(address: VmAddress) -> io::Result<Self> {
        let mut s = VmSocket::new()?;
        s.bind(address)?;
        s.listen(4)
    }

    /// Accepts the next connection.
    pub fn accept(&self) -> io::Result<(VmStream, VmAddress)> {
        let (s, addr) = self.0.accept()?;
        Ok((VmStream(s), VmAddress::try_from_sock_addr(&addr).unwrap()))
    }

    /// Retrieves the address that the listener is bound to.
    pub fn local_addr(&self) -> io::Result<VmAddress> {
        Ok(VmAddress::try_from_sock_addr(&self.0.local_addr()?).unwrap())
    }
}

impl pal_async::socket::Listener for VmListener {
    type Socket = VmStream;
    type Address = VmAddress;

    fn accept(&self) -> io::Result<(Self::Socket, Self::Address)> {
        self.accept()
    }

    fn local_addr(&self) -> io::Result<Self::Address> {
        self.local_addr()
    }
}

impl From<Socket> for VmListener {
    fn from(s: Socket) -> Self {
        Self(s)
    }
}

impl From<VmListener> for Socket {
    fn from(s: VmListener) -> Self {
        s.0
    }
}

/// A VM stream socket.
#[derive(Debug)]
pub struct VmStream(Socket);

impl VmStream {
    /// Connects to the specified address.
    pub fn connect(addr: VmAddress) -> io::Result<Self> {
        VmSocket::new()?.connect(addr)
    }

    /// Attempts to clone the underlying socket.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }
}

impl From<Socket> for VmStream {
    fn from(s: Socket) -> Self {
        Self(s)
    }
}

impl From<VmStream> for Socket {
    fn from(s: VmStream) -> Self {
        s.0
    }
}

impl Read for VmStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for VmStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Read for &'_ VmStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(buf)
    }
}

impl Write for &'_ VmStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.0).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.0).flush()
    }
}
