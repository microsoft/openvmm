// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Async Unix domain socket I/O with SCM_RIGHTS fd passing for vhost-user.

use crate::protocol::VHOST_USER_MAX_FDS;
use crate::protocol::VhostUserMsgHeader;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::PolledSocket;
use std::future::poll_fn;
use std::io;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use thiserror::Error;
use unix_socket::UnixStream;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub enum SocketError {
    #[error("i/o error")]
    Io(#[source] io::Error),
    #[error("connection closed")]
    Closed,
    #[error("payload too large: {0} bytes")]
    PayloadTooLarge(u32),
}

impl From<io::Error> for SocketError {
    fn from(e: io::Error) -> Self {
        SocketError::Io(e)
    }
}

/// Maximum payload size to accept (4 MB, generous upper bound).
const MAX_PAYLOAD_SIZE: u32 = 4 * 1024 * 1024;

/// CMSG_ALIGN: align `len` up to pointer-size boundary.
///
/// Matches the kernel's CMSG_ALIGN on both glibc and musl.
const fn cmsg_align(len: usize) -> usize {
    (len + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

/// CMSG_SPACE: total ancillary buffer space needed for `data_len` bytes of
/// control data.
const fn cmsg_space(data_len: usize) -> usize {
    cmsg_align(size_of::<libc::cmsghdr>()) + cmsg_align(data_len)
}

/// CMSG_LEN: value to store in `cmsg_len` for `data_len` bytes of control data.
const fn cmsg_len(data_len: usize) -> usize {
    cmsg_align(size_of::<libc::cmsghdr>()) + data_len
}

#[repr(C)]
struct CmsgScmRights {
    hdr: libc::cmsghdr,
    fds: [RawFd; VHOST_USER_MAX_FDS],
}

/// Async vhost-user socket for sending and receiving protocol messages.
pub struct VhostUserSocket {
    socket: parking_lot::Mutex<PolledSocket<UnixStream>>,
}

impl VhostUserSocket {
    /// Wrap a connected `UnixStream` in an async vhost-user socket.
    pub fn new(socket: PolledSocket<UnixStream>) -> Self {
        Self {
            socket: parking_lot::Mutex::new(socket),
        }
    }

    /// Receive a vhost-user message (header + payload + optional fds).
    ///
    /// Returns the parsed header, payload bytes, and any received file descriptors.
    pub async fn recv_message(
        &self,
    ) -> Result<(VhostUserMsgHeader, Vec<u8>, Vec<OwnedFd>), SocketError> {
        // Read header + ancillary data (fds come with the first recvmsg).
        let mut hdr_buf = [0u8; size_of::<VhostUserMsgHeader>()];
        let mut fds = Vec::new();
        let n = self.recv_exact(&mut hdr_buf, &mut fds).await?;
        if n == 0 {
            return Err(SocketError::Closed);
        }

        let hdr = VhostUserMsgHeader::read_from_bytes(&hdr_buf)
            .expect("hdr_buf is exactly the right size");

        // Read payload if any.
        let payload = if hdr.size > 0 {
            if hdr.size > MAX_PAYLOAD_SIZE {
                return Err(SocketError::PayloadTooLarge(hdr.size));
            }
            let mut payload = vec![0u8; hdr.size as usize];
            self.recv_exact_no_fds(&mut payload).await?;
            payload
        } else {
            Vec::new()
        };

        Ok((hdr, payload, fds))
    }

    /// Send a vhost-user message (header + payload + optional fds).
    pub async fn send_message(
        &self,
        header: &VhostUserMsgHeader,
        payload: &[u8],
        fds: &[impl AsFd],
    ) -> Result<(), SocketError> {
        let hdr_bytes = header.as_bytes();
        let iov = [IoSlice::new(hdr_bytes), IoSlice::new(payload)];
        self.send_with_fds(&iov, fds).await
    }

    /// Receive exactly `buf.len()` bytes, collecting any fds from the first recvmsg.
    async fn recv_exact(
        &self,
        buf: &mut [u8],
        fds: &mut Vec<OwnedFd>,
    ) -> Result<usize, SocketError> {
        let mut read = 0;
        while read < buf.len() {
            let n = self
                .recv_raw(&mut buf[read..], if read == 0 { Some(fds) } else { None })
                .await?;
            if n == 0 {
                if read == 0 {
                    return Ok(0);
                }
                return Err(SocketError::Closed);
            }
            read += n;
        }
        Ok(read)
    }

    /// Receive exactly `buf.len()` bytes, ignoring any ancillary data.
    async fn recv_exact_no_fds(&self, buf: &mut [u8]) -> Result<(), SocketError> {
        let mut read = 0;
        while read < buf.len() {
            let n = self.recv_raw(&mut buf[read..], None).await?;
            if n == 0 {
                return Err(SocketError::Closed);
            }
            read += n;
        }
        Ok(())
    }

    /// Low-level async recv with optional fd collection.
    ///
    /// Waits until the socket is readable, then performs the recv.
    /// On spurious readiness (WouldBlock), re-polls automatically.
    async fn recv_raw(
        &self,
        buf: &mut [u8],
        fds: Option<&mut Vec<OwnedFd>>,
    ) -> Result<usize, SocketError> {
        // Stash received fds in a Mutex so we can extract them from the
        // poll_io closure (which can't borrow fds directly). Using
        // parking_lot::Mutex rather than RefCell so the future is Send.
        let received_fds = parking_lot::Mutex::new(Vec::new());
        let want_fds = fds.is_some();

        let n = poll_fn(|cx| {
            self.socket
                .lock()
                .poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                    let mut tmp_fds = Vec::new();
                    let fd_arg = if want_fds { Some(&mut tmp_fds) } else { None };
                    let n = try_recv(socket.get(), buf, fd_arg)?;
                    if want_fds && !tmp_fds.is_empty() {
                        received_fds.lock().extend(tmp_fds);
                    }
                    Ok(n)
                })
        })
        .await?;

        if let Some(fds) = fds {
            fds.extend(received_fds.into_inner());
        }
        Ok(n)
    }

    /// Low-level async send with optional fds.
    async fn send_with_fds(
        &self,
        iov: &[IoSlice<'_>],
        fds: &[impl AsFd],
    ) -> Result<(), SocketError> {
        let raw_fds: Vec<RawFd> = fds.iter().map(|f| f.as_fd().as_raw_fd()).collect();
        let mut sent = 0;
        let total: usize = iov.iter().map(|s| s.len()).sum();

        // Send all data. Fds are only attached to the first sendmsg.
        while sent < total {
            let remaining_iov = build_remaining_iov(iov, sent);
            let send_fds: &[RawFd] = if sent == 0 { &raw_fds } else { &[] };

            let n = poll_fn(|cx| {
                self.socket
                    .lock()
                    .poll_io(cx, InterestSlot::Write, PollEvents::OUT, |socket| {
                        try_send(socket.get(), &remaining_iov, send_fds)
                    })
            })
            .await?;
            sent += n;
        }
        Ok(())
    }
}

/// Build IoSlice entries for the remaining unsent bytes.
fn build_remaining_iov<'a>(original: &'a [IoSlice<'a>], skip: usize) -> Vec<IoSlice<'a>> {
    let mut remaining = skip;
    let mut result = Vec::new();
    for slice in original {
        if remaining >= slice.len() {
            remaining -= slice.len();
        } else {
            result.push(IoSlice::new(&slice[remaining..]));
            remaining = 0;
        }
    }
    result
}

/// Send data with optional file descriptors via sendmsg. May return WouldBlock.
#[allow(
    clippy::needless_update,
    clippy::useless_conversion,
    reason = "libc cmsghdr field types and as-conversions differ between musl and glibc"
)]
fn try_send(socket: &UnixStream, msg: &[IoSlice<'_>], fds: &[RawFd]) -> io::Result<usize> {
    assert!(
        fds.len() <= VHOST_USER_MAX_FDS,
        "too many fds: {} > {}",
        fds.len(),
        VHOST_USER_MAX_FDS
    );
    let fds_data_len = size_of_val(fds);
    let mut cmsg = CmsgScmRights {
        hdr: libc::cmsghdr {
            cmsg_level: libc::SOL_SOCKET,
            cmsg_type: libc::SCM_RIGHTS,
            cmsg_len: cmsg_len(fds_data_len) as _,
            ..{
                // SAFETY: type has no invariants
                unsafe { std::mem::zeroed() }
            }
        },
        fds: [0; VHOST_USER_MAX_FDS],
    };
    for (src, dst) in fds.iter().zip(cmsg.fds.iter_mut()) {
        *dst = *src;
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
    hdr.msg_controllen = if fds.is_empty() {
        0
    } else {
        cmsg_space(fds_data_len) as _
    };

    // SAFETY: calling with appropriately initialized buffers.
    let n = unsafe { libc::sendmsg(socket.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Receive data and optional file descriptors via recvmsg. May return WouldBlock.
///
/// Always provides a cmsg buffer so that any file descriptors sent by the peer
/// are properly received and closed, even when the caller doesn't expect them.
/// This prevents fd leaks from misbehaving or malicious peers.
fn try_recv(
    socket: &UnixStream,
    buf: &mut [u8],
    fds: Option<&mut Vec<OwnedFd>>,
) -> io::Result<usize> {
    assert!(!buf.is_empty());
    let mut iov = IoSliceMut::new(buf);

    // SAFETY: type has no invariants
    let mut cmsg: CmsgScmRights = unsafe { std::mem::zeroed() };
    // SAFETY: type has no invariants
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = std::ptr::from_mut(&mut iov).cast::<libc::iovec>();
    hdr.msg_iovlen = 1;

    // Always provide the cmsg buffer. If the peer sends fds when we don't
    // expect them, we need to receive them so they can be properly closed
    // (via OwnedFd drop) rather than leaked.
    hdr.msg_control = std::ptr::from_mut(&mut cmsg).cast::<libc::c_void>();
    hdr.msg_controllen = size_of_val(&cmsg) as _;

    // SAFETY: calling with properly initialized buffers.
    let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut hdr, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Ok(0);
    }

    // Check for truncation before extracting fds — if control data was
    // truncated, some fds may have been lost by the kernel.
    if hdr.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        // Close any fds that were successfully received before returning
        // the error — they are in cmsg.fds as raw ints at this point.
        if hdr.msg_controllen > 0
            && cmsg.hdr.cmsg_level == libc::SOL_SOCKET
            && cmsg.hdr.cmsg_type == libc::SCM_RIGHTS
        {
            #[allow(
                clippy::unnecessary_cast,
                reason = "cmsg_len type differs between musl and glibc"
            )]
            let fd_count = ((cmsg.hdr.cmsg_len as usize).saturating_sub(size_of_val(&cmsg.hdr))
                / size_of::<RawFd>())
            .min(VHOST_USER_MAX_FDS);
            for &raw_fd in &cmsg.fds[..fd_count] {
                // SAFETY: the kernel transferred ownership of these fds to us.
                drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            }
        }
        return Err(io::Error::from_raw_os_error(libc::EMSGSIZE));
    }

    // Extract fds from ancillary data.
    if hdr.msg_controllen > 0 {
        if cmsg.hdr.cmsg_level != libc::SOL_SOCKET || cmsg.hdr.cmsg_type != libc::SCM_RIGHTS {
            // Unexpected ancillary data type — no SCM_RIGHTS fds to close.
            return Err(io::ErrorKind::InvalidData.into());
        }
        #[allow(
            clippy::unnecessary_cast,
            reason = "cmsg_len type differs between musl and glibc"
        )]
        let fd_count = ((cmsg.hdr.cmsg_len as usize).saturating_sub(size_of_val(&cmsg.hdr))
            / size_of::<RawFd>())
        .min(VHOST_USER_MAX_FDS);
        if let Some(fds) = fds {
            fds.extend(cmsg.fds[..fd_count].iter().map(|&raw_fd| {
                // SAFETY: the kernel transferred ownership of these fds to us.
                unsafe { OwnedFd::from_raw_fd(raw_fd) }
            }));
        } else {
            // Caller didn't want fds — close them so they don't leak.
            for &raw_fd in &cmsg.fds[..fd_count] {
                // SAFETY: the kernel transferred ownership of these fds to us.
                drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            }
        }
    }

    Ok(n as usize)
}
