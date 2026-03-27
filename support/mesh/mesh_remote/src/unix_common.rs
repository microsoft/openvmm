// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Low-level `sendmsg`/`recvmsg` helpers with SCM_RIGHTS fd passing.
//!
//! These are shared by [`crate::unix_node`] (seqpacket mesh transport) and
//! [`crate::unix_listener`] (stream-based listener/handshake transport).

#![cfg(unix)]
// UNSAFETY: Calls to libc send/recvmsg fns and the work to prepare their inputs
// and handle their outputs (mem::zeroed, transmutes, from_raw_fds).
#![expect(unsafe_code)]

use mesh_node::resource::OsResource;
use std::io;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::os::unix::prelude::*;

#[repr(C)]
struct CmsgScmRights {
    hdr: libc::cmsghdr,
    fds: [RawFd; MAX_FDS_PER_MSG],
}

/// Maximum number of file descriptors that can be sent/received in a single
/// message. Limited by the fixed-size cmsg buffer.
const MAX_FDS_PER_MSG: usize = 64;

/// Sends a packet, including the specified file descriptors. May fail with
/// ErrorKind::WouldBlock.
#[allow(
    clippy::needless_update,
    clippy::useless_conversion,
    reason = "libc::cmsghdr has different type defs on different platforms"
)]
pub(crate) fn try_send(
    fd: BorrowedFd<'_>,
    msg: &[IoSlice<'_>],
    fds: &[OsResource],
) -> io::Result<usize> {
    if fds.len() > MAX_FDS_PER_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "too many file descriptors ({}, max {})",
                fds.len(),
                MAX_FDS_PER_MSG
            ),
        ));
    }
    let mut cmsg = CmsgScmRights {
        hdr: libc::cmsghdr {
            cmsg_level: libc::SOL_SOCKET,
            cmsg_type: libc::SCM_RIGHTS,
            cmsg_len: (size_of::<libc::cmsghdr>() + fds.len() * size_of::<RawFd>())
                .try_into()
                .unwrap(),

            ..{
                // SAFETY: type has no invariants
                unsafe { std::mem::zeroed() }
            }
        },
        fds: [0; MAX_FDS_PER_MSG],
    };
    for (fdi, fdo) in fds.iter().zip(cmsg.fds.iter_mut()) {
        *fdo = match fdi {
            OsResource::Fd(fd) => fd.as_raw_fd(),
        }
    }

    // SAFETY: type has no invariants
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = msg.as_ptr() as *mut libc::iovec;
    hdr.msg_iovlen = msg.len().try_into().unwrap();
    hdr.msg_control = if fds.is_empty() {
        std::ptr::null_mut()
    } else {
        std::ptr::from_mut(&mut cmsg).cast::<libc::c_void>()
    };
    hdr.msg_controllen = if fds.is_empty() { 0 } else { cmsg.hdr.cmsg_len };
    // SAFETY: calling with appropriately initialized buffers.
    let n = unsafe { libc::sendmsg(fd.as_raw_fd(), &hdr, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Receives the next packet. Returns the number of bytes read and any file
/// descriptors that were associated with the packet. May fail with
/// ErrorKind::WouldBlock.
pub(crate) fn try_recv(
    fd: BorrowedFd<'_>,
    buf: &mut [u8],
    fds: &mut Vec<OsResource>,
) -> io::Result<usize> {
    assert!(!buf.is_empty());
    let mut iov = IoSliceMut::new(buf);
    // SAFETY: type has no invariants
    let mut cmsg_buf: CmsgScmRights = unsafe { std::mem::zeroed() };
    // SAFETY: type has no invariants
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = std::ptr::from_mut(&mut iov).cast::<libc::iovec>();
    hdr.msg_iovlen = 1;
    hdr.msg_control = std::ptr::from_mut(&mut cmsg_buf).cast::<libc::c_void>();
    hdr.msg_controllen = size_of_val(&cmsg_buf) as _;

    // On Linux, automatically set O_CLOEXEC on incoming fds.
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;

    // SAFETY: calling with properly initialized buffers.
    let n = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut hdr, flags) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        assert_eq!(hdr.msg_controllen, 0);
        return Ok(0);
    }

    // Iterate through all control messages, collecting fds from SCM_RIGHTS.
    // Non-SCM_RIGHTS messages are ignored (they don't carry fds).
    let start = fds.len();
    // SAFETY: hdr was populated by recvmsg.
    let mut cmsg_ptr = unsafe { libc::CMSG_FIRSTHDR(&hdr) };
    while !cmsg_ptr.is_null() {
        // SAFETY: cmsg_ptr is valid per CMSG_FIRSTHDR/CMSG_NXTHDR contract.
        let cmsg = unsafe { &*cmsg_ptr };
        if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
            #[allow(clippy::unnecessary_cast)] // cmsg_len is u32 on musl and usize on gnu.
            let data_len = cmsg.cmsg_len as usize - size_of::<libc::cmsghdr>();
            let fd_count = data_len / size_of::<RawFd>();
            // SAFETY: CMSG_DATA returns a pointer to the cmsg payload, which
            // for SCM_RIGHTS contains fd_count contiguous RawFds.
            let data_ptr = unsafe { libc::CMSG_DATA(cmsg_ptr) };
            // SAFETY: data_ptr points to fd_count RawFds within the cmsg buffer
            // that was populated by recvmsg.
            let raw_fds = unsafe { std::slice::from_raw_parts(data_ptr.cast::<RawFd>(), fd_count) };
            fds.extend(raw_fds.iter().map(|&raw_fd| {
                // SAFETY: SCM_RIGHTS delivers file descriptors that the
                // receiving process now owns (see unix(7) / cmsg(3)). Each
                // fd returned by recvmsg is a fresh descriptor in our table
                // and must be closed exactly once.
                OsResource::Fd(unsafe { OwnedFd::from_raw_fd(raw_fd) })
            }));
        }
        // SAFETY: iterating per CMSG_NXTHDR contract.
        cmsg_ptr = unsafe { libc::CMSG_NXTHDR(&hdr, cmsg_ptr) };
    }

    // Set O_CLOEXEC on all received fds on platforms that don't support
    // MSG_CMSG_CLOEXEC (set above).
    if !cfg!(target_os = "linux") {
        for OsResource::Fd(fd) in &fds[start..] {
            set_cloexec(fd);
        }
    }

    // Check for truncation only after taking ownership of the fds that did
    // fit. If MSG_CTRUNC is set, the kernel truncated ancillary data — some
    // fds were discarded. Close the fds we did receive (they're useless
    // without the full set) and report an error.
    if hdr.msg_flags & libc::MSG_CTRUNC != 0 {
        fds.drain(start..);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control message truncated: sender sent too many file descriptors",
        ));
    }
    // MSG_TRUNC: the data portion of the message was larger than the
    // receive buffer (applicable to seqpacket/datagram sockets).
    if hdr.msg_flags & libc::MSG_TRUNC != 0 {
        fds.drain(start..);
        return Err(io::Error::from_raw_os_error(libc::EMSGSIZE));
    }
    Ok(n as usize)
}

fn set_cloexec(fd: impl AsFd) {
    // SAFETY: using fcntl as documented.
    unsafe {
        let flags = libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_GETFD);
        assert!(flags >= 0);
        let r = libc::fcntl(
            fd.as_fd().as_raw_fd(),
            libc::F_SETFD,
            flags | libc::FD_CLOEXEC,
        );
        assert!(r >= 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::IoSlice;
    use std::io::Read;
    use std::io::Write;
    use test_with_tracing::test;

    /// Create a non-blocking Unix SOCK_STREAM socketpair.
    fn socketpair() -> (socket2::Socket, socket2::Socket) {
        let (a, b) =
            socket2::Socket::pair(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        (a, b)
    }

    /// Create a pipe via libc, returning (read_fd, write_fd).
    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: calling pipe with valid buffer.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe returns two new owned fds.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn test_try_send_recv_data_only() {
        let (a, b) = socketpair();
        let msg = b"hello";
        let n = try_send(a.as_fd(), &[IoSlice::new(msg)], &[]).unwrap();
        assert_eq!(n, 5);

        let mut buf = [0u8; 64];
        let mut fds = Vec::new();
        let n = try_recv(b.as_fd(), &mut buf, &mut fds).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..5], b"hello");
        assert!(fds.is_empty());
    }

    #[test]
    fn test_try_send_recv_with_fds() {
        let (a, b) = socketpair();

        // Create two pipes to get fd pairs we can verify after transfer.
        let (r1, w1) = pipe();
        let (r2, w2) = pipe();

        // Write test data through the write ends so we can verify the
        // received read fds work.
        std::fs::File::from(w1).write_all(b"pipe1").unwrap();
        std::fs::File::from(w2).write_all(b"pipe2").unwrap();

        let os_fds = vec![
            OsResource::Fd(r1.try_clone().unwrap()),
            OsResource::Fd(r2.try_clone().unwrap()),
        ];
        drop(r1);
        drop(r2);

        let msg = b"data";
        try_send(a.as_fd(), &[IoSlice::new(msg)], &os_fds).unwrap();
        drop(os_fds);

        let mut buf = [0u8; 64];
        let mut recv_fds = Vec::new();
        let n = try_recv(b.as_fd(), &mut buf, &mut recv_fds).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf[..4], b"data");
        assert_eq!(recv_fds.len(), 2);

        // Verify received fds are usable — read from them.
        let mut pipe_buf = Vec::new();
        let OsResource::Fd(ref fd1) = recv_fds[0];
        std::fs::File::from(fd1.try_clone().unwrap())
            .read_to_end(&mut pipe_buf)
            .unwrap();
        assert_eq!(pipe_buf, b"pipe1");

        pipe_buf.clear();
        let OsResource::Fd(ref fd2) = recv_fds[1];
        std::fs::File::from(fd2.try_clone().unwrap())
            .read_to_end(&mut pipe_buf)
            .unwrap();
        assert_eq!(pipe_buf, b"pipe2");
    }

    #[test]
    fn test_try_send_rejects_too_many_fds() {
        let (a, _b) = socketpair();

        // Build MAX_FDS_PER_MSG + 1 fds.
        let fds: Vec<OsResource> = (0..MAX_FDS_PER_MSG + 1)
            .map(|_| {
                let (r, _w) = pipe();
                OsResource::Fd(r)
            })
            .collect();

        let err = try_send(a.as_fd(), &[IoSlice::new(b"x")], &fds).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// Test that the receiver properly detects and handles MSG_CTRUNC when
    /// the sender sends more fds than the receiver's cmsg buffer can hold.
    #[test]
    fn test_try_recv_ctrunc_cleans_up_fds() {
        let (a, b) = socketpair();

        // We need to send more fds than CmsgScmRights can hold (64).
        // Since try_send rejects >64, we craft a raw sendmsg with 65 fds.
        let fd_count = MAX_FDS_PER_MSG + 1;
        let pipes: Vec<(OwnedFd, OwnedFd)> = (0..fd_count).map(|_| pipe()).collect();
        let raw_fds: Vec<RawFd> = pipes.iter().map(|(r, _)| r.as_raw_fd()).collect();

        // Build an oversized cmsg buffer on the stack.
        let cmsg_data_len = fd_count * size_of::<RawFd>();
        let cmsg_len = size_of::<libc::cmsghdr>() + cmsg_data_len;
        let total_len = cmsg_len + 64; // padding
        let mut cmsg_storage = vec![0u8; total_len];

        // SAFETY: writing a valid cmsghdr + fd array into the buffer.
        unsafe {
            let hdr_ptr = cmsg_storage.as_mut_ptr().cast::<libc::cmsghdr>();
            (*hdr_ptr).cmsg_level = libc::SOL_SOCKET;
            (*hdr_ptr).cmsg_type = libc::SCM_RIGHTS;
            (*hdr_ptr).cmsg_len = cmsg_len as _;
            let data_ptr = libc::CMSG_DATA(hdr_ptr);
            std::ptr::copy_nonoverlapping(raw_fds.as_ptr().cast::<u8>(), data_ptr, cmsg_data_len);
        }

        let msg_data = b"x";
        let iov = libc::iovec {
            iov_base: std::ptr::from_ref(msg_data).cast_mut().cast(),
            iov_len: msg_data.len(),
        };

        // SAFETY: calling sendmsg with valid buffers.
        let sent = unsafe {
            let mut hdr: libc::msghdr = std::mem::zeroed();
            hdr.msg_iov = std::ptr::from_ref(&iov).cast_mut();
            hdr.msg_iovlen = 1;
            hdr.msg_control = cmsg_storage.as_mut_ptr().cast();
            hdr.msg_controllen = cmsg_len as _;
            libc::sendmsg(a.as_raw_fd(), &hdr, 0)
        };
        assert!(sent > 0, "sendmsg failed: {}", io::Error::last_os_error());

        // Now receive — the receiver's buffer only fits 64 fds, so
        // MSG_CTRUNC should be set.
        let mut buf = [0u8; 64];
        let mut recv_fds = Vec::new();
        let err = try_recv(b.as_fd(), &mut buf, &mut recv_fds).unwrap_err();

        // Should get our truncation error.
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("truncated"),
            "unexpected error: {}",
            err
        );

        // The fds vec should be empty — any partially received fds must
        // have been cleaned up.
        assert!(
            recv_fds.is_empty(),
            "expected no fds after truncation, got {}",
            recv_fds.len()
        );
    }
}
