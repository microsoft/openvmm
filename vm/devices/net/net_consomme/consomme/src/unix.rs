// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(unix)]
//! Unix platform helpers for consomme.
//!
//! - IPv6 address detection via `getifaddrs()`.
//! - UDP GSO send:
//!   - Linux: `setsockopt(IPPROTO_UDP, UDP_SEGMENT)` sets the segment size
//!     once per connection
//!   - macOS: `sendmsg_x()` private API.

// UNSAFETY: getifaddrs/freeifaddrs; setsockopt for UDP_SEGMENT (Linux);
// sendmsg_x (private Apple API) with a manually built msghdr_x array.
#![expect(unsafe_code)]

use std::mem::size_of;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::os::unix::io::AsRawFd;

/// Configure the UDP GSO segment size on `socket`.
///
/// On Linux this calls `setsockopt(IPPROTO_UDP, UDP_SEGMENT, size)`, which
/// persists for the lifetime of the connection. When `size` is 0 the option
/// is cleared and normal (non-GSO) sends resume.
#[cfg(target_os = "linux")]
pub fn set_udp_gso_size(socket: &UdpSocket, size: u16) -> std::io::Result<()> {
    // SAFETY: setsockopt with a valid u16 optval per Linux udp(7) documentation
    // for UDP_SEGMENT.
    let ret = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_UDP,
            libc::UDP_SEGMENT,
            std::ptr::from_ref(&size).cast::<libc::c_void>(),
            size_of::<u16>() as libc::socklen_t,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Configure the UDP GSO segment size on `socket`.
///
/// On macOS the segment size is conveyed per-send via `sendmsg_x`, so there
/// is nothing to configure on the socket itself. This is a no-op.
#[cfg(target_os = "macos")]
pub fn set_udp_gso_size(_socket: &UdpSocket, _size: u16) -> std::io::Result<()> {
    Ok(())
}

/// Send `data` to `dst` via `socket`, using UDP GSO if `gso` is `Some`.
///
/// On Linux, `UDP_SEGMENT` must already be configured on the socket via
/// [`set_udp_gso_size`]. The kernel then automatically splits the outgoing
/// buffer into datagrams of that size, so this is just a plain `send_to`
/// regardless of the `gso` value.
#[cfg(target_os = "linux")]
pub fn send_to(
    socket: &UdpSocket,
    data: &[u8],
    dst: &SocketAddr,
    _: Option<u16>,
) -> std::io::Result<usize> {
    socket.send_to(data, *dst)
}

/// Send `data` to `dst` via `socket`, using UDP GSO if `gso` is `Some`.
///
/// When `gso` is `None`, this is a plain `send_to`. When `gso` is
/// `Some(seg_size)`, macOS uses the private `sendmsg_x()` API to batch
/// multiple datagrams in a single syscall (user-space segmentation).
///
/// `sendmsg_x()` and `msghdr_x` are undocumented Apple extensions (present
/// since macOS 10.11). `msghdr_x` is identical to the standard `msghdr`
/// except for an extra `msg_datalen` field that records the byte count for
/// each entry. `sendmsg_x` returns the number of messages queued, not bytes.
#[cfg(target_os = "macos")]
pub fn send_to(
    socket: &UdpSocket,
    data: &[u8],
    dst: &SocketAddr,
    gso: Option<u16>,
) -> std::io::Result<usize> {
    let Some(seg_size) = gso else {
        return socket.send_to(data, *dst);
    };

    // Private Apple extension of msghdr
    // Adapted from: https://github.com/apple-oss-distributions/xnu/blob/8d741a5de7ff4191bf97d57b9f54c2f6d4a15585/bsd/sys/socket_private.h
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

    // Build one iovec per segment.
    let iovecs: Vec<libc::iovec> = data
        .chunks(seg_size)
        .map(|chunk| libc::iovec {
            iov_base: chunk.as_ptr() as *mut libc::c_void,
            iov_len: chunk.len(),
        })
        .collect();

    // Build a matching msghdr_x per segment.
    let hdrs: Vec<MsghdrX> = iovecs
        .iter()
        .map(|iov| MsghdrX {
            msg_name: sockaddr.as_ptr() as *mut libc::c_void,
            msg_namelen: sockaddr.len(),
            msg_iov: std::ptr::from_ref(iov).cast_mut(),
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
