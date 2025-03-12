// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AF_HYPERV support.

// UNSAFETY: Creating and managing a raw socket.
#![expect(unsafe_code)]

use crate::VmListener;
use crate::VmSocket;
use crate::VmStream;
use guid::Guid;
use mesh::payload::os_resource;
use socket2::SockAddr;
use socket2::Socket;
use socket2::Type;
use std::io;
use std::os::raw::c_int;
use std::os::windows::prelude::*;
use std::time::Duration;
use windows_sys::Win32::Networking::WinSock::AF_HYPERV;
use windows_sys::Win32::Networking::WinSock::SOCKET_ERROR;
use windows_sys::Win32::Networking::WinSock::WSAGetLastError;
use windows_sys::Win32::Networking::WinSock::setsockopt;
use windows_sys::Win32::System::Hypervisor::HV_GUID_PARENT;
use windows_sys::Win32::System::Hypervisor::HV_GUID_ZERO;
use windows_sys::Win32::System::Hypervisor::HV_PROTOCOL_RAW;
use windows_sys::Win32::System::Hypervisor::HVSOCKET_CONNECT_TIMEOUT;
use windows_sys::Win32::System::Hypervisor::SOCKADDR_HV;

/// Creates an AF_HYPERV service ID from an AF_VSOCK port.
fn service_id_from_vsock_port(port: u32) -> Guid {
    Guid {
        data1: port,
        .."00000000-facb-11e6-bd58-64006a7986d3".parse().unwrap()
    }
}

#[derive(Debug)]
pub struct Address {
    pub(crate) vm_id: Guid,
    pub(crate) service_id: Guid,
}

impl Address {
    pub fn new(vm_id: Guid, service_id: Guid) -> Self {
        Self { vm_id, service_id }
    }

    pub fn vsock(vm_id: Guid, port: u32) -> Self {
        Self::new(vm_id, service_id_from_vsock_port(port))
    }

    pub fn hyperv_any(service_id: Guid) -> Self {
        Self::new(HV_GUID_ZERO.into(), service_id)
    }

    pub fn hyperv_host(service_id: Guid) -> Self {
        Self::new(HV_GUID_PARENT.into(), service_id)
    }

    pub fn vsock_any(port: u32) -> Self {
        Self::hyperv_any(service_id_from_vsock_port(port))
    }

    pub fn vsock_host(port: u32) -> Self {
        Self::hyperv_host(service_id_from_vsock_port(port))
    }

    pub fn into_sock_addr(self) -> SockAddr {
        let address = SOCKADDR_HV {
            Family: AF_HYPERV,
            Reserved: 0,
            VmId: self.vm_id.into(),
            ServiceId: self.service_id.into(),
        };

        // SAFETY: initializing storage as documented.
        let (_, address) = unsafe {
            SockAddr::try_init(|storage, len| {
                assert!(*len as usize >= size_of_val(&address));
                let storage: &mut SOCKADDR_HV = &mut *storage.cast();
                *storage = address;
                *len = size_of_val(&address) as i32;
                Ok(())
            })
            .unwrap()
        };

        address
    }

    pub fn try_from_sock_addr(addr: &SockAddr) -> Option<Self> {
        if (addr.len() as usize) < size_of::<SOCKADDR_HV>() {
            return None;
        }
        // SAFETY: buffer is large enough.
        let addr = unsafe { &*addr.as_ptr().cast::<SOCKADDR_HV>() };
        if addr.Family != AF_HYPERV {
            return None;
        }
        Some(Self::new(addr.VmId.into(), addr.ServiceId.into()))
    }
}

impl VmSocket {
    pub(crate) fn new_inner() -> io::Result<Self> {
        Ok(Self(Socket::new(
            (AF_HYPERV as c_int).into(),
            Type::STREAM,
            Some((HV_PROTOCOL_RAW as c_int).into()),
        )?))
    }

    /// Sets the connection timeout for this socket.
    pub fn set_connect_timeout(&self, duration: Duration) -> io::Result<()> {
        let timeout = duration.as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: calling as documented
        unsafe {
            if setsockopt(
                self.as_socket().as_raw_socket() as _,
                HV_PROTOCOL_RAW as _,
                HVSOCKET_CONNECT_TIMEOUT as i32,
                std::ptr::from_ref::<u32>(&timeout) as *mut u8,
                size_of_val(&timeout) as _,
            ) == SOCKET_ERROR
            {
                return Err(io::Error::from_raw_os_error(WSAGetLastError()));
            }
            Ok(())
        }
    }

    /// Sets whether this socket targets a VM's high VTL.
    pub fn set_high_vtl(&self, high_vtl: bool) -> io::Result<()> {
        const HVSOCKET_HIGH_VTL: i32 = 8;
        let high_vtl: u32 = high_vtl.into();
        // SAFETY: calling as documented
        unsafe {
            if setsockopt(
                self.as_socket().as_raw_socket() as _,
                HV_PROTOCOL_RAW as _,
                HVSOCKET_HIGH_VTL,
                std::ptr::from_ref::<u32>(&high_vtl) as *mut u8,
                size_of_val(&high_vtl) as _,
            ) == SOCKET_ERROR
            {
                return Err(io::Error::from_raw_os_error(WSAGetLastError()));
            }
        }
        Ok(())
    }
}

impl From<OwnedSocket> for VmSocket {
    fn from(socket: OwnedSocket) -> Self {
        Self(socket.into())
    }
}

impl From<VmSocket> for OwnedSocket {
    fn from(socket: VmSocket) -> Self {
        socket.0.into()
    }
}

impl AsSocket for VmSocket {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        self.0.as_socket()
    }
}

impl From<OwnedSocket> for VmListener {
    fn from(socket: OwnedSocket) -> Self {
        Self(socket.into())
    }
}

impl From<VmListener> for OwnedSocket {
    fn from(socket: VmListener) -> Self {
        socket.0.into()
    }
}

impl AsSocket for VmListener {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        self.0.as_socket()
    }
}

impl From<OwnedSocket> for VmStream {
    fn from(socket: OwnedSocket) -> Self {
        Self(socket.into())
    }
}

impl From<VmStream> for OwnedSocket {
    fn from(socket: VmStream) -> Self {
        socket.0.into()
    }
}

impl AsSocket for VmStream {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        self.0.as_socket()
    }
}

os_resource!(VmSocket, OwnedSocket);
os_resource!(VmStream, OwnedSocket);
os_resource!(VmListener, OwnedSocket);
