// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(unix)]
//! Unix platform helpers for consomme.
//!
//! - IPv6 address detection via `getifaddrs()`.
//! - UDP GSO batch send:
//!   - Linux: `sendmsg(2)` + `UDP_SEGMENT` cmsg (kernel segmentation).
//!   - macOS: `sendmsg_x()` private API (user-space segments, one syscall).
//!   - Other Unix: software loop over `send_to()`.

// UNSAFETY: getifaddrs/freeifaddrs; sendmsg with a manually built msghdr;
// sendmsg_x (private Apple API) with a manually built msghdr_x array.
#![expect(unsafe_code)]

use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::UdpSocket;

/// Send `data` as a UDP GSO batch, splitting into datagrams of `seg_size`
/// bytes each.
///
/// - **Linux**: one `sendmsg(2)` call with a `UDP_SEGMENT` control message;
///   the kernel (or NIC driver) performs the segmentation.
/// - **macOS**: one `sendmsg_x()` call (private Apple API) with one
///   `msghdr_x` entry per segment; user-space segments but a single syscall.
/// - **Other Unix**: software loop — one `send_to()` call per segment.
#[cfg(target_os = "linux")]
pub fn send_udp_with_gso(
    socket: &UdpSocket,
    data: &[u8],
    dst: &SocketAddr,
    seg_size: u16,
) -> std::io::Result<usize> {
    use std::mem::size_of;
    use std::os::unix::io::AsRawFd;

    let sockaddr = socket2::SockAddr::from(*dst);
    let iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let cmsg_space =
        // SAFETY: computing the buffer size for a single u16 cmsg.
        unsafe { libc::CMSG_SPACE(size_of::<u16>() as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];
    // Use zeroed() + field assignment rather than struct literal syntax:
    // musl's msghdr has private padding fields on 64-bit targets that make
    // struct literal initialization fail to compile.
    // SAFETY: all-zero is a valid initializer for msghdr.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = sockaddr.as_ptr() as *mut libc::c_void;
    msg.msg_namelen = sockaddr.len();
    msg.msg_iov = &iov as *const libc::iovec as *mut libc::iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space as _;
    // SAFETY: msg_control and msg_controllen point to our allocated buffer,
    // which is large enough for a single u16 UDP_SEGMENT control message.
    let cmsg = unsafe { &mut *libc::CMSG_FIRSTHDR(&msg) };
    cmsg.cmsg_level = libc::IPPROTO_UDP;
    cmsg.cmsg_type = libc::UDP_SEGMENT;
    cmsg.cmsg_len =
        // SAFETY: computing the cmsg_len for a single u16 data field.
        unsafe { libc::CMSG_LEN(size_of::<u16>() as u32) as _ };
    // SAFETY: writing a u16 into the CMSG data area, which is correctly
    // sized for a u16 payload.
    unsafe { *(libc::CMSG_DATA(cmsg) as *mut u16) = seg_size };

    // SAFETY: calling sendmsg(2) with a correctly constructed msghdr.
    let ret = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, 0) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

/// macOS batch send using the private `sendmsg_x()` API.
///
/// `sendmsg_x()` and `msghdr_x` are undocumented Apple extensions (present
/// since macOS 10.11) that allow sending multiple datagrams in a single
/// syscall. `msghdr_x` is identical to the standard `msghdr` except for an
/// extra `msg_datalen` field that records the byte count for each entry.
/// `sendmsg_x` returns the number of messages queued, not bytes.
///
/// This gives us user-space segmentation with a single syscall, rather than
/// one syscall per segment.
#[cfg(target_os = "macos")]
pub fn send_udp_with_gso(
    socket: &UdpSocket,
    data: &[u8],
    dst: &SocketAddr,
    seg_size: u16,
) -> std::io::Result<usize> {
    use std::os::unix::io::AsRawFd;

    // Private Apple extension of msghdr: identical layout up to msg_flags,
    // then an extra msg_datalen field that holds the per-entry byte count.
    #[repr(C)]
    struct MsghdrX {
        msg_name: *mut libc::c_void,
        msg_namelen: libc::socklen_t,
        msg_iov: *mut libc::iovec,
        msg_iovlen: libc::c_int,
        msg_control: *mut libc::c_void,
        msg_controllen: libc::socklen_t,
        msg_flags: libc::c_int,
        msg_datalen: libc::size_t,
    }

    unsafe extern "C" {
        /// Batch-send `cnt` datagrams described by `msgp[0..cnt]`.
        /// Returns the number of messages queued, or -1 on error.
        fn sendmsg_x(
            s: libc::c_int,
            msgp: *const MsghdrX,
            cnt: libc::c_uint,
            flags: libc::c_int,
        ) -> isize;
    }

    let sockaddr = socket2::SockAddr::from(*dst);
    let seg_size = seg_size as usize;

    // Build one iovec per segment. Collected up front so the Vec's heap
    // allocation is stable before we take raw pointers into it.
    let iovecs: Vec<libc::iovec> = data
        .chunks(seg_size)
        .map(|chunk| libc::iovec {
            iov_base: chunk.as_ptr() as *mut libc::c_void,
            iov_len: chunk.len(),
        })
        .collect();

    // Build a matching msghdr_x per segment. Each entry shares the same
    // destination address and points to its own iovec.
    let hdrs: Vec<MsghdrX> = iovecs
        .iter()
        .map(|iov| MsghdrX {
            msg_name: sockaddr.as_ptr() as *mut libc::c_void,
            msg_namelen: sockaddr.len(),
            msg_iov: iov as *const libc::iovec as *mut libc::iovec,
            msg_iovlen: 1,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
            msg_datalen: iov.iov_len,
        })
        .collect();

    // SAFETY: sendmsg_x reads hdrs[0..hdrs.len()]. Each entry holds a valid
    // pointer into iovecs (stable for the duration of this call) and a
    // borrowed pointer to sockaddr (also live for this call). hdrs is passed
    // as a non-null, correctly sized slice.
    let sent = unsafe {
        sendmsg_x(
            socket.as_raw_fd(),
            hdrs.as_ptr(),
            hdrs.len() as libc::c_uint,
            0,
        )
    };

    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // sendmsg_x returns the number of messages queued. Sum the byte counts of
    // the successfully sent entries to produce the total byte count.
    Ok(iovecs[..sent as usize].iter().map(|iov| iov.iov_len).sum())
}


/// Checks whether the host has at least one non-link-local, non-loopback
/// IPv6 unicast address assigned.
pub fn host_has_ipv6_address() -> Result<bool, std::io::Error> {
    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();

    // SAFETY: Calling getifaddrs according to its API contract. The function
    // allocates memory and populates a linked list of interface addresses.
    let result = unsafe { libc::getifaddrs(&mut addrs) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut found = false;
    let mut current = addrs;

    while !current.is_null() {
        // SAFETY: `current` is a valid node in the linked list allocated by
        // getifaddrs. We dereference it to read ifa_addr and ifa_next.
        // When ifa_addr is a non-null AF_INET6 sockaddr, we cast to
        // sockaddr_in6 to extract the address bytes.
        let (ipv6_addr, next) = unsafe {
            let ifa = &*current;
            let addr =
                if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET6 {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    Some(Ipv6Addr::from(sin6.sin6_addr.s6_addr))
                } else {
                    None
                };
            (addr, ifa.ifa_next)
        };

        if let Some(addr) = ipv6_addr {
            if super::is_routable_ipv6(&addr) {
                found = true;
                break;
            }
        }

        current = next;
    }

    // SAFETY: Freeing the linked list allocated by getifaddrs.
    unsafe { libc::freeifaddrs(addrs) };

    Ok(found)
}
