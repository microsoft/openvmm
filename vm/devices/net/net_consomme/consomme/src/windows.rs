// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]
// UNSAFETY: Calling Win32 APIs to set TCP initial RTO.
#![allow(unsafe_code)]

use socket2::Socket;
use std::os::windows::io::AsRawSocket;
use windows_sys::Win32::Networking::WinSock;

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
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
            None,
        );
        if result == WinSock::SOCKET_ERROR {
            Err(WinSock::WSAGetLastError())
        } else {
            Ok(())
        }
    }
}
