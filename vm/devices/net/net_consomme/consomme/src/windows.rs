// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]
// UNSAFETY: Calling Win32 APIs to set TCP initial RTO and to check host IPv6
// addresses.
#![expect(unsafe_code)]

use socket2::Socket;
use std::net::Ipv6Addr;
use std::os::windows::io::AsRawSocket;
use std::ptr::null_mut;
use winapi as _;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::NetworkManagement::IpHelper::MIB_UNICASTIPADDRESS_TABLE;
use windows_sys::Win32::Networking::WinSock;
use windows_sys::Win32::Networking::WinSock::AF_INET6;

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
    let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = null_mut();

    // SAFETY: Calling delay-loaded GetUnicastIpAddressTable with a valid
    // output pointer, then walking the returned MIB table entries.
    // The table is freed with FreeMibTable after inspection.
    let result = unsafe { GetUnicastIpAddressTable(AF_INET6, &mut table) };
    if result as u32 != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(result));
    }
    let has_ipv6 = if !table.is_null() {
        // SAFETY: On success, GetUnicastIpAddressTable returns a valid table
        // pointer. We read NumEntries and build a slice over Table[0..NumEntries],
        // which are all within the allocated buffer.
        let entries = unsafe {
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize)
        };

        let found = entries.iter().any(|row| {
            // SAFETY: Accessing union fields of SOCKADDR_INET (Ipv6 variant)
            // and IN6_ADDR (Byte variant). We know these are IPv6 entries
            // because we queried with AF_INET6.
            let bytes = unsafe { row.Address.Ipv6.sin6_addr.u.Byte };
            let ipv6_addr = Ipv6Addr::from(bytes);
            super::is_routable_ipv6(&ipv6_addr)
        });

        // SAFETY: FreeMibTable frees the table allocated by
        // GetUnicastIpAddressTable.
        unsafe { FreeMibTable(table.cast()) };
        found
    } else {
        false
    };

    Ok(has_ipv6)
}
