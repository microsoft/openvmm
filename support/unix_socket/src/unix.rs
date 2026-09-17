// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Low-level `sendmsg`/`recvmsg` helpers with SCM_RIGHTS fd passing.
//!
//! These are transport-agnostic single-syscall wrappers: message framing is
//! the caller's responsibility.
//!
//! - [`send_with_fds`] performs a single `sendmsg`, attaching an iterator of
//!   borrowed descriptors via one `SCM_RIGHTS` control message.
//! - [`ScmReceiver`] performs `recvmsg`, owning a reusable control buffer and
//!   the descriptors it receives (which live in that buffer — there is no
//!   separate `Vec` of fds).

#![cfg(unix)]
// UNSAFETY: Calls to libc send/recvmsg fns and the work to prepare their inputs
// and handle their outputs (mem::zeroed, cmsg pointer math, from_raw_fds).
#![expect(unsafe_code)]

use std::io;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::marker::PhantomData;
use std::os::unix::prelude::*;

/// Sends a message over `sock`, attaching `fds` via a single `SCM_RIGHTS`
/// control message. May fail with [`io::ErrorKind::WouldBlock`].
///
/// This is a single `sendmsg` call; framing is the caller's responsibility. The
/// descriptors are written straight into the control buffer from the iterator
/// (whose length is known up front), so the only allocation is that control
/// buffer — and only when there is at least one descriptor. The common no-fd
/// path allocates nothing.
#[expect(clippy::allow_attributes)]
#[allow(
    clippy::useless_conversion,
    reason = "cmsg field types differ across libc targets (gnu vs musl, etc.)"
)]
pub fn send_with_fds<'a>(
    sock: BorrowedFd<'_>,
    bufs: &[IoSlice<'_>],
    fds: impl IntoIterator<Item = BorrowedFd<'a>, IntoIter: ExactSizeIterator>,
) -> io::Result<usize> {
    let fds = fds.into_iter();
    let count = fds.len();

    // SAFETY: `msghdr` has no validity invariants; a zeroed value is valid.
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = bufs.as_ptr() as *mut libc::iovec;
    hdr.msg_iovlen = bufs.len().try_into().unwrap();

    // Control buffer, allocated only when passing fds. `Vec<u64>` guarantees
    // 8-byte alignment, which satisfies `cmsghdr`'s alignment requirement.
    let mut control;
    if count == 0 {
        hdr.msg_control = std::ptr::null_mut();
        hdr.msg_controllen = 0;
    } else {
        let data_len = count * size_of::<RawFd>();
        // SAFETY: `CMSG_SPACE`/`CMSG_LEN` are pure size calculations.
        let space = unsafe { libc::CMSG_SPACE(data_len as _) } as usize;
        control = vec![0u64; space.div_ceil(size_of::<u64>())];
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = space.try_into().unwrap();

        // SAFETY: `msg_control` points to `space` writable, aligned bytes, so
        // `CMSG_FIRSTHDR` returns a valid header pointer.
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&hdr) };
        assert!(!cmsg.is_null());
        // SAFETY: `cmsg` points into the control buffer, which has room for the
        // header followed by `count` descriptors.
        let data = unsafe {
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            libc::CMSG_DATA(cmsg).cast::<RawFd>()
        };

        // Write each descriptor straight into the payload. `take(count)` bounds
        // the writes by the allocated capacity so a misbehaving
        // `ExactSizeIterator` (one yielding more than it reported) cannot
        // overflow the buffer; the real length is derived from the number
        // actually written.
        let mut written = 0;
        for fd in fds.take(count) {
            // SAFETY: `written < count`, so this slot is within the payload.
            unsafe { data.add(written).write(fd.as_raw_fd()) };
            written += 1;
        }

        let data_len = written * size_of::<RawFd>();
        // SAFETY: `cmsg` is the header located above; set its final length, and
        // shrink `msg_controllen` to match if fewer descriptors were written
        // than `count`.
        unsafe {
            (*cmsg).cmsg_len = (libc::CMSG_LEN(data_len as _) as usize).try_into().unwrap();
        }
        // SAFETY: `CMSG_SPACE` is a pure size calculation.
        let used = unsafe { libc::CMSG_SPACE(data_len as _) } as usize;
        hdr.msg_controllen = used.try_into().unwrap();
    }

    // Suppress SIGPIPE when the peer has closed, so callers get an `EPIPE`
    // error instead of a signal.
    // SAFETY: `hdr` references valid iov and control buffers for this call.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// A reusable receiver for messages carrying `SCM_RIGHTS` descriptors.
///
/// Owns a heap control buffer sized (at construction) for up to `max_fds`
/// descriptors, plus the descriptors received by the most recent
/// [`recv`](Self::recv) — which live *in* that control buffer as their raw
/// values, not in a separate allocation. Reuse one [`ScmReceiver`] across many
/// messages to avoid re-allocating the buffer.
///
/// Each [`recv`](Self::recv) starts fresh: it first closes any descriptors
/// still held from the previous call, then receives. Its behavior therefore
/// does not depend on whether the caller remembered to consume the prior
/// descriptors. **Consume the descriptors — via [`fds`](Self::fds) (borrow in
/// place) or [`drain`](Self::drain) (take ownership) — before calling
/// [`recv`](Self::recv) again**, or they are dropped.
///
/// Each `recv` exposes only the descriptors delivered by that one `recvmsg`.
/// To gather descriptors across several `recv` calls, drain after each and
/// accumulate them yourself.
///
/// The receiver owns the descriptors it holds and closes any that are still
/// unconsumed when it is dropped, cleared, or reused.
pub struct ScmReceiver {
    /// Aligned control buffer, reused across `recv` calls.
    control: Vec<u64>,
    /// Byte length of the control buffer.
    control_len: usize,
    /// Byte offset within `control` of the received `RawFd` array. Meaningful
    /// only when `fd_count > 0`.
    fd_offset: usize,
    /// Number of valid, owned descriptors currently held in `control`.
    fd_count: usize,
}

impl ScmReceiver {
    /// Creates a receiver whose control buffer can hold up to `max_fds`
    /// descriptors per message. A message carrying more than `max_fds`
    /// descriptors is rejected (the excess is truncated by the kernel; see
    /// [`recv`](Self::recv)).
    pub fn new(max_fds: usize) -> Self {
        let control_len = if max_fds == 0 {
            0
        } else {
            // SAFETY: `CMSG_SPACE` is a pure size calculation.
            unsafe { libc::CMSG_SPACE((max_fds * size_of::<RawFd>()) as _) as usize }
        };
        Self {
            control: vec![0u64; control_len.div_ceil(size_of::<u64>())],
            control_len,
            fd_offset: 0,
            fd_count: 0,
        }
    }

    /// Performs a single `recvmsg`, writing data into `buf` and taking
    /// ownership of any descriptors that arrive. May fail with
    /// [`io::ErrorKind::WouldBlock`]. Returns the number of bytes read (0 on
    /// EOF).
    ///
    /// Any descriptors still held from a previous `recv` are closed first (see
    /// the type-level docs), so consume them before calling again. If the
    /// control buffer is too small for the descriptors in a message the
    /// received descriptors are closed and an error is returned.
    pub fn recv(&mut self, sock: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<usize> {
        assert!(!buf.is_empty());
        // Close any descriptors left unconsumed from a previous `recv`. Each
        // call starts fresh and offers the control buffer below, so held fds
        // would otherwise be clobbered by the kernel; the caller is expected to
        // consume them (via `fds`/`drain`) before calling again.
        self.clear();

        let mut iov = IoSliceMut::new(buf);
        // SAFETY: `msghdr` has no validity invariants; a zeroed value is valid.
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = std::ptr::from_mut(&mut iov).cast();
        hdr.msg_iovlen = 1;

        // Always offer the control buffer so descriptors are received on every
        // call; there are none held to protect, since we just cleared.
        let offer_control = self.control_len != 0;
        if offer_control {
            hdr.msg_control = self.control.as_mut_ptr().cast();
            hdr.msg_controllen = self.control_len as _;
        }

        // On Linux, atomically set O_CLOEXEC on incoming descriptors.
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;

        // SAFETY: calling `recvmsg` with valid, initialized buffers.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut hdr, flags) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }

        if offer_control {
            // Locate the (single) SCM_RIGHTS control message. The kernel merges
            // all passed descriptors into one such message.
            // SAFETY: `hdr` was populated by `recvmsg`.
            let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&hdr) };
            while !cmsg.is_null() {
                // SAFETY: `cmsg` is valid per the `CMSG_*HDR` contract.
                let c = unsafe { &*cmsg };
                if c.cmsg_level == libc::SOL_SOCKET && c.cmsg_type == libc::SCM_RIGHTS {
                    // SAFETY: `CMSG_DATA` points at the fd array within `control`.
                    let data = unsafe { libc::CMSG_DATA(cmsg) };
                    // The fd array runs from `CMSG_DATA` to the end of the
                    // control message, so its length is `cmsg_len` minus the
                    // header (plus any alignment padding before the data). The
                    // kernel always reports a `cmsg_len` at least as large as
                    // that offset, so this subtraction never underflows; use
                    // `checked_sub` to fail loudly rather than silently if that
                    // assumption is ever violated.
                    let data_len = (c.cmsg_len as usize)
                        .checked_sub(data as usize - cmsg as usize)
                        .unwrap();
                    self.fd_offset = data as usize - self.control.as_ptr() as usize;
                    self.fd_count = data_len / size_of::<RawFd>();
                    break;
                }
                // SAFETY: iterating per the `CMSG_NXTHDR` contract.
                cmsg = unsafe { libc::CMSG_NXTHDR(&hdr, cmsg) };
            }

            // On platforms without MSG_CMSG_CLOEXEC, set O_CLOEXEC by hand.
            #[cfg(not(target_os = "linux"))]
            for fd in self.fds() {
                set_cloexec(fd);
            }

            // If ancillary data was truncated the kernel discarded the
            // descriptors that did not fit; the ones we did get are useless
            // without the full set, so close them and fail.
            if hdr.msg_flags & libc::MSG_CTRUNC != 0 {
                self.clear();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control message truncated: sender sent too many file descriptors",
                ));
            }
        }

        if n == 0 {
            return Ok(0);
        }

        // MSG_TRUNC: the data portion was larger than `buf` (seqpacket/datagram).
        if hdr.msg_flags & libc::MSG_TRUNC != 0 {
            self.clear();
            return Err(io::Error::from_raw_os_error(libc::EMSGSIZE));
        }

        Ok(n as usize)
    }

    /// The descriptors currently held, borrowed in place.
    pub fn fds(&self) -> &[OwnedFd] {
        if self.fd_count == 0 {
            return &[];
        }
        // SAFETY: `OwnedFd` is `repr(transparent)` over `RawFd` and never holds
        // `-1`; the `fd_count` descriptors at `fd_offset` were delivered by
        // `recvmsg` (each a valid, non-negative fd), so the region is a valid
        // `[OwnedFd]`. The returned borrow keeps ownership with `self`, so the
        // descriptors are not closed while it is live.
        unsafe {
            let ptr = self
                .control
                .as_ptr()
                .cast::<u8>()
                .add(self.fd_offset)
                .cast::<OwnedFd>();
            std::slice::from_raw_parts(ptr, self.fd_count)
        }
    }

    /// Takes ownership of the held descriptors, yielding each as an [`OwnedFd`]
    /// and leaving the receiver empty (ready for reuse). Descriptors not
    /// consumed from the returned iterator are closed when it is dropped.
    pub fn drain(&mut self) -> ScmDrainIter<'_> {
        let count = self.fd_count;
        // Logically transfer ownership to the iterator so `Drop`/reuse won't
        // also close these descriptors.
        self.fd_count = 0;
        ScmDrainIter {
            base: self.control.as_ptr().cast::<u8>(),
            offset: self.fd_offset,
            pos: 0,
            count,
            _receiver: PhantomData,
        }
    }

    /// Closes any held descriptors and resets the receiver for reuse.
    pub fn clear(&mut self) {
        // Draining and dropping each descriptor closes it.
        self.drain().for_each(drop);
    }
}

impl Drop for ScmReceiver {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Iterator returned by [`ScmReceiver::drain`], yielding the received
/// descriptors as owned values.
pub struct ScmDrainIter<'a> {
    base: *const u8,
    offset: usize,
    pos: usize,
    count: usize,
    // Borrows the receiver mutably for the iterator's lifetime, so the control
    // buffer cannot move or be otherwise accessed while draining.
    _receiver: PhantomData<&'a mut ScmReceiver>,
}

impl Iterator for ScmDrainIter<'_> {
    type Item = OwnedFd;

    fn next(&mut self) -> Option<OwnedFd> {
        if self.pos >= self.count {
            return None;
        }
        // SAFETY: the `pos`-th slot holds a valid `RawFd` delivered by
        // `recvmsg`; advancing `pos` ensures each is taken (and thus closed)
        // exactly once.
        let fd = unsafe {
            let p = self.base.add(self.offset).cast::<RawFd>().add(self.pos);
            OwnedFd::from_raw_fd(p.read())
        };
        self.pos += 1;
        Some(fd)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.count - self.pos;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ScmDrainIter<'_> {}

impl Drop for ScmDrainIter<'_> {
    fn drop(&mut self) {
        // Close any descriptors not taken by the caller.
        self.for_each(drop);
    }
}

#[cfg(not(target_os = "linux"))]
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

    /// Create a blocking Unix SOCK_STREAM socketpair.
    fn socketpair() -> (socket2::Socket, socket2::Socket) {
        socket2::Socket::pair(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap()
    }

    /// Create a pipe via libc, returning (read_fd, write_fd).
    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: calling pipe with a valid buffer.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe returns two new owned fds.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn data_only() {
        let (a, b) = socketpair();
        let n = send_with_fds(a.as_fd(), &[IoSlice::new(b"hello")], []).unwrap();
        assert_eq!(n, 5);

        let mut rx = ScmReceiver::new(4);
        let mut buf = [0u8; 64];
        let n = rx.recv(b.as_fd(), &mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..5], b"hello");
        assert!(rx.fds().is_empty());
    }

    #[test]
    fn with_fds() {
        let (a, b) = socketpair();

        // Two pipes whose read ends we transfer; write test data so we can
        // verify the received read ends work.
        let (r1, w1) = pipe();
        let (r2, w2) = pipe();
        std::fs::File::from(w1).write_all(b"pipe1").unwrap();
        std::fs::File::from(w2).write_all(b"pipe2").unwrap();

        send_with_fds(
            a.as_fd(),
            &[IoSlice::new(b"data")],
            [r1.as_fd(), r2.as_fd()],
        )
        .unwrap();
        drop(r1);
        drop(r2);

        let mut rx = ScmReceiver::new(4);
        let mut buf = [0u8; 64];
        let n = rx.recv(b.as_fd(), &mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf[..4], b"data");
        assert_eq!(rx.fds().len(), 2);

        // Borrow in place: the first received fd reads "pipe1".
        let mut pb = Vec::new();
        std::fs::File::from(rx.fds()[0].try_clone().unwrap())
            .read_to_end(&mut pb)
            .unwrap();
        assert_eq!(pb, b"pipe1");

        // Drain takes ownership; the second fd reads "pipe2".
        let fds: Vec<OwnedFd> = rx.drain().collect();
        assert_eq!(fds.len(), 2);
        pb.clear();
        std::fs::File::from(fds[1].try_clone().unwrap())
            .read_to_end(&mut pb)
            .unwrap();
        assert_eq!(pb, b"pipe2");

        // Nothing remains after draining.
        assert!(rx.fds().is_empty());
    }

    #[test]
    fn drain_between_reads() {
        // All of a message's fds arrive with the first read; the caller drains
        // them into its own collection before the next read.
        let (a, b) = socketpair();
        let (r1, _w1) = pipe();
        send_with_fds(a.as_fd(), &[IoSlice::new(b"AABB")], [r1.as_fd()]).unwrap();
        drop(r1);

        let mut rx = ScmReceiver::new(2);
        let mut buf = [0u8; 2];
        let mut fds = Vec::new();

        // First read gets the fd alongside the first two bytes; drain it.
        let n = rx.recv(b.as_fd(), &mut buf).unwrap();
        assert_eq!(n, 2);
        fds.extend(rx.drain());
        assert_eq!(fds.len(), 1);

        // Second read gets the remaining bytes and no new fds.
        let n = rx.recv(b.as_fd(), &mut buf).unwrap();
        assert_eq!(n, 2);
        fds.extend(rx.drain());
        assert_eq!(fds.len(), 1);
    }

    #[test]
    fn recv_clears_unconsumed_fds() {
        // If the caller forgets to consume the fds, the next `recv` closes them
        // rather than silently retaining them.
        let (a, b) = socketpair();
        let (r1, _w1) = pipe();
        send_with_fds(a.as_fd(), &[IoSlice::new(b"AABB")], [r1.as_fd()]).unwrap();
        drop(r1);

        let mut rx = ScmReceiver::new(2);
        let mut buf = [0u8; 2];

        let n = rx.recv(b.as_fd(), &mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(rx.fds().len(), 1);

        // Second read starts fresh: the previously-held fd is closed.
        let n = rx.recv(b.as_fd(), &mut buf).unwrap();
        assert_eq!(n, 2);
        assert!(rx.fds().is_empty());
    }

    #[test]
    fn ctrunc_cleans_up_fds() {
        let (a, b) = socketpair();
        let (r1, _w1) = pipe();
        let (r2, _w2) = pipe();
        let (r3, _w3) = pipe();

        // The receiver has room for only two descriptors; send three.
        send_with_fds(
            a.as_fd(),
            &[IoSlice::new(b"x")],
            [r1.as_fd(), r2.as_fd(), r3.as_fd()],
        )
        .unwrap();

        let mut rx = ScmReceiver::new(2);
        let mut buf = [0u8; 64];
        let err = rx.recv(b.as_fd(), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // All partially-received descriptors must have been dropped.
        assert!(rx.fds().is_empty());
    }
}
