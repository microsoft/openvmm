// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Calling SockAddr::try_init.
#![expect(unsafe_code)]

use socket2::SockAddr;
use socket2::Socket;
use std::io;

pub(crate) fn bind_kobject_uevent_socket() -> io::Result<Socket> {
    let socket = Socket::new(
        libc::PF_NETLINK.into(),
        socket2::Type::DGRAM,
        Some(libc::NETLINK_KOBJECT_UEVENT.into()),
    )?;

    // SAFETY: Address family (AF_NETLINK) and length matches the type of storage (sockaddr_nl).
    let ((), sockaddr) = unsafe {
        SockAddr::try_init(|storage, len| {
            let mut address = std::mem::MaybeUninit::<libc::sockaddr_nl>::uninit();
            // SAFETY: Initialize the structure properly by zeroing it first, then setting specific fields
            let address = {
                std::ptr::write_bytes(address.as_mut_ptr(), 0, 1);
                let addr_ptr = address.as_mut_ptr();
                (*addr_ptr).nl_family = libc::AF_NETLINK as _;
                (*addr_ptr).nl_pid = 0;
                (*addr_ptr).nl_groups = 1;
                address.assume_init()
            };
            storage.cast::<libc::sockaddr_nl>().write(address);
            len.write(size_of::<libc::sockaddr_nl>() as u32);
            Ok(())
        })?
    };

    socket.bind(&sockaddr)?;

    Ok(socket)
}
