// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]
// UNSAFETY: Calling Win32 APIs to set TCP initial RTO, to configure UDP GSO
// via setsockopt, and to list the host's addresses.
#![expect(unsafe_code)]

use socket2::Socket;
use std::mem::size_of;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::os::windows::io::AsRawSocket;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::NetworkManagement::IpHelper::MIB_UNICASTIPADDRESS_TABLE;
use windows_sys::Win32::Networking::WinSock;
use windows_sys::Win32::Networking::WinSock::AF_INET;
use windows_sys::Win32::Networking::WinSock::AF_INET6;
use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;
use windows_sys::Win32::Networking::WinSock::UDP_SEND_MSG_SIZE;

pub fn disable_connection_retries(sock: &Socket) -> Result<(), i32> {
    const TCP_INITIAL_RTO_UNSPECIFIED_RTT: u16 = 0xffff;
    const TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS: u8 = 0xfe;
    let rto_params = WinSock::TCP_INITIAL_RTO_PARAMETERS {
        Rtt: TCP_INITIAL_RTO_UNSPECIFIED_RTT,
        MaxSynRetransmissions: TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS,
    };

    let mut bytes_returned = 0;
    // SAFETY: Calling function according to documentation.
    unsafe {
        let result = WinSock::WSAIoctl(
            sock.as_raw_socket() as WinSock::SOCKET,
            WinSock::SIO_TCP_INITIAL_RTO,
            std::ptr::from_ref(&rto_params).cast::<core::ffi::c_void>(),
            size_of::<WinSock::TCP_INITIAL_RTO_PARAMETERS>() as u32,
            null_mut(),
            0,
            &mut bytes_returned,
            null_mut(),
            None,
        );
        if result == WinSock::SOCKET_ERROR {
            Err(WinSock::WSAGetLastError())
        } else {
            Ok(())
        }
    }
}

pal::delayload! {"Iphlpapi.dll" {
    fn GetUnicastIpAddressTable(
        family: u16,
        table: *mut *mut MIB_UNICASTIPADDRESS_TABLE,
    ) -> i32;

    fn FreeMibTable(
        memory: *const core::ffi::c_void,
    ) -> ();
}}

/// Checks whether the host has at least one non-link-local, non-loopback
/// IPv6 unicast address assigned.
pub fn host_has_ipv6_address() -> Result<bool, std::io::Error> {
    Ok(host_unicast_addresses()?
        .iter()
        .any(|address| matches!(address, IpAddr::V6(address) if super::is_routable_ipv6(address))))
}

/// Lists the host's unicast addresses, loopback included.
pub fn host_unicast_addresses() -> Result<Vec<IpAddr>, std::io::Error> {
    let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = null_mut();

    // SAFETY: Calling delay-loaded GetUnicastIpAddressTable with a valid
    // output pointer, then walking the returned MIB table entries.
    // The table is freed with FreeMibTable after inspection.
    let result = unsafe { GetUnicastIpAddressTable(AF_UNSPEC, &mut table) };
    if result as u32 != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(result));
    }
    let mut addresses = Vec::new();
    if !table.is_null() {
        // SAFETY: On success, GetUnicastIpAddressTable returns a valid table
        // pointer. We read NumEntries and build a slice over Table[0..NumEntries],
        // which are all within the allocated buffer.
        let entries = unsafe {
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize)
        };

        for row in entries {
            // SAFETY: Accessing union fields of SOCKADDR_INET, whose
            // si_family selects the IPv4 or IPv6 variant that the row holds,
            // and of IN_ADDR and IN6_ADDR as bytes.
            let address = unsafe {
                match row.Address.si_family {
                    AF_INET => Some(IpAddr::V4(Ipv4Addr::from(
                        row.Address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes(),
                    ))),
                    AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(
                        row.Address.Ipv6.sin6_addr.u.Byte,
                    ))),
                    _ => None,
                }
            };
            addresses.extend(address);
        }

        // SAFETY: FreeMibTable frees the table allocated by
        // GetUnicastIpAddressTable.
        unsafe { FreeMibTable(table.cast()) };
    }

    Ok(addresses)
}

/// Configure the `UDP_SEND_MSG_SIZE` socket option on `socket`.
pub fn set_udp_gso_size(socket: &UdpSocket, size: u16) -> std::io::Result<()> {
    let raw = socket.as_raw_socket() as WinSock::SOCKET;
    let size_dword = size as u32;
    // SAFETY: setsockopt with a valid DWORD optval per MSDN documentation for
    // UDP_SEND_MSG_SIZE.
    let ret = unsafe {
        WinSock::setsockopt(
            raw,
            WinSock::IPPROTO_UDP,
            UDP_SEND_MSG_SIZE,
            std::ptr::from_ref(&size_dword).cast::<u8>(),
            size_of::<u32>() as i32,
        )
    };
    if ret == WinSock::SOCKET_ERROR {
        return Err(std::io::Error::from_raw_os_error(
            // SAFETY: WSAGetLastError is safe to call after a socket error.
            unsafe { WinSock::WSAGetLastError() } as i32,
        ));
    }
    Ok(())
}

/// Send `data` to `dst` via `socket`.
pub fn send_to(
    socket: &UdpSocket,
    data: &[u8],
    dst: &SocketAddr,
    _: Option<u16>,
) -> std::io::Result<usize> {
    socket.send_to(data, *dst)
}
