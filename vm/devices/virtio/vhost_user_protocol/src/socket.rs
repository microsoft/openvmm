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
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;
use thiserror::Error;
use unix_socket::ScmReceiver;
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
    /// The caller provides a [`ScmReceiver`] (typically reused across the
    /// connection) to hold the control buffer used for fd passing, avoiding a
    /// per-message allocation. Returns the parsed header, payload bytes, and
    /// any received file descriptors.
    pub async fn recv_message(
        &self,
        receiver: &mut ScmReceiver,
    ) -> Result<(VhostUserMsgHeader, Vec<u8>, Vec<OwnedFd>), SocketError> {
        // Read header + ancillary data (fds come with the first recvmsg).
        let mut hdr_buf = [0u8; size_of::<VhostUserMsgHeader>()];
        let mut fds = Vec::new();
        let n = self.recv_exact(receiver, &mut hdr_buf, &mut fds).await?;
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
            self.recv_exact_no_fds(receiver, &mut payload).await?;
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
        receiver: &mut ScmReceiver,
        buf: &mut [u8],
        fds: &mut Vec<OwnedFd>,
    ) -> Result<usize, SocketError> {
        let mut read = 0;
        while read < buf.len() {
            let n = self
                .recv_raw(
                    receiver,
                    &mut buf[read..],
                    if read == 0 { Some(fds) } else { None },
                )
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
    async fn recv_exact_no_fds(
        &self,
        receiver: &mut ScmReceiver,
        buf: &mut [u8],
    ) -> Result<(), SocketError> {
        let mut read = 0;
        while read < buf.len() {
            let n = self.recv_raw(receiver, &mut buf[read..], None).await?;
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
    ///
    /// The `receiver` is drained (into `fds`) or cleared before returning, so
    /// it is always empty on exit and therefore empty on the next entry.
    async fn recv_raw(
        &self,
        receiver: &mut ScmReceiver,
        buf: &mut [u8],
        fds: Option<&mut Vec<OwnedFd>>,
    ) -> Result<usize, SocketError> {
        let result = poll_fn(|cx| {
            self.socket
                .lock()
                .poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                    receiver.recv(socket.get().as_fd(), buf)
                })
        })
        .await;

        // Hand received fds to the caller, or drop them (closing any stray
        // descriptors a peer sent unexpectedly). Either way the receiver ends
        // up empty, ready for the next call.
        match fds {
            Some(fds) => fds.extend(receiver.drain()),
            None => receiver.clear(),
        }

        Ok(result?)
    }

    /// Low-level async send with optional fds.
    async fn send_with_fds(
        &self,
        iov: &[IoSlice<'_>],
        fds: &[impl AsFd],
    ) -> Result<(), SocketError> {
        assert!(
            fds.len() <= VHOST_USER_MAX_FDS,
            "too many fds: {} > {}",
            fds.len(),
            VHOST_USER_MAX_FDS
        );
        let mut sent = 0;
        let total: usize = iov.iter().map(|s| s.len()).sum();

        // Send all data. Fds are only attached to the first sendmsg, since
        // SCM_RIGHTS delivers them alongside the first byte of the message.
        while sent < total {
            let remaining_iov = build_remaining_iov(iov, sent);
            let attach_fds = sent == 0;

            let n = poll_fn(|cx| {
                self.socket
                    .lock()
                    .poll_io(cx, InterestSlot::Write, PollEvents::OUT, |socket| {
                        if attach_fds {
                            unix_socket::send_with_fds(
                                socket.get().as_fd(),
                                &remaining_iov,
                                fds.iter().map(|f| f.as_fd()),
                            )
                        } else {
                            unix_socket::send_with_fds(socket.get().as_fd(), &remaining_iov, [])
                        }
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
