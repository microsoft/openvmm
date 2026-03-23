// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generic MeshPayload-over-Unix-socket transport.
//!
//! This module provides functions to send and receive [`MeshPayload`] values
//! over a Unix stream socket, transferring OS resources (file descriptors) via
//! SCM_RIGHTS ancillary data.

#![cfg(unix)]

use crate::unix_common::try_recv;
use crate::unix_common::try_send;
use mesh_node::message::MeshPayload;
use mesh_node::resource::OsResource;
use mesh_node::resource::Resource;
use mesh_protobuf::SerializedMessage;
use pal_async::driver::Driver;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::PolledSocket;
use std::future::poll_fn;
use std::io;
use std::io::IoSlice;
use std::os::unix::prelude::*;
use std::path::Path;
use thiserror::Error;
use unix_socket::UnixListener;
use unix_socket::UnixStream;

/// Error returned by [`send_payload`].
#[derive(Debug, Error)]
pub enum SendPayloadError {
    /// The message contained a Port resource, which cannot be transferred
    /// over a payload socket.
    #[error("cannot send Port resources over a payload socket")]
    PortResourceNotSupported,
    /// An I/O error occurred while sending.
    #[error("failed to send payload")]
    Io(#[source] io::Error),
}

/// Error returned by [`recv_payload`].
#[derive(Debug, Error)]
pub enum RecvPayloadError {
    /// The payload data length exceeded the maximum allowed size.
    #[error("payload data too large ({len} bytes, max {max})")]
    DataTooLarge { len: usize, max: usize },
    /// The header declared more file descriptors than allowed.
    #[error("too many file descriptors in payload header ({count}, max {max})")]
    TooManyFds { count: usize, max: usize },
    /// The number of file descriptors received did not match the header.
    #[error("expected {expected} file descriptors, received {actual}")]
    FdCountMismatch { expected: usize, actual: usize },
    /// The received payload could not be deserialized into the expected type.
    #[error("failed to deserialize payload")]
    Deserialize(#[source] mesh_protobuf::Error),
    /// An I/O error occurred while receiving.
    #[error("failed to receive payload")]
    Io(#[source] io::Error),
}

/// Wire format header for payload messages.
///
/// ```text
/// [4 bytes LE: data_len][4 bytes LE: fd_count][data_len bytes: protobuf data]
/// + SCM_RIGHTS ancillary data carrying fd_count file descriptors
/// ```
const HEADER_LEN: usize = 8;

/// Send a [`MeshPayload`] value over a Unix stream, transferring OS resources
/// via SCM_RIGHTS.
pub async fn send_payload<T: MeshPayload>(
    stream: &mut PolledSocket<UnixStream>,
    value: T,
) -> Result<(), SendPayloadError> {
    let msg: SerializedMessage<Resource> = SerializedMessage::from_message(value);
    send_serialized(stream, msg).await
}

/// Non-generic inner function for [`send_payload`].
async fn send_serialized(
    stream: &mut PolledSocket<UnixStream>,
    msg: SerializedMessage<Resource>,
) -> Result<(), SendPayloadError> {
    let mut fds = Vec::new();
    for resource in msg.resources {
        match resource {
            Resource::Os(os) => fds.push(os),
            Resource::Port(_) => {
                return Err(SendPayloadError::PortResourceNotSupported);
            }
        }
    }

    let data_len = msg.data.len() as u32;
    let fd_count = fds.len() as u32;
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&data_len.to_le_bytes());
    header[4..8].copy_from_slice(&fd_count.to_le_bytes());

    // Send the entire message (header + data + fds) in a single sendmsg call.
    // For stream sockets, we may need to retry on partial writes.
    let total_len = header.len() + msg.data.len();
    let mut sent = 0usize;
    let mut fds_sent = false;
    while sent < total_len {
        let n = poll_fn(|cx| {
            stream.poll_io(cx, InterestSlot::Write, PollEvents::OUT, |stream| {
                // Build iov slices for the remaining data.
                let header_remaining = if sent < HEADER_LEN {
                    &header[sent..]
                } else {
                    &[]
                };
                let data_offset = sent.saturating_sub(HEADER_LEN);
                let data_remaining = &msg.data[data_offset..];
                let bufs = [IoSlice::new(header_remaining), IoSlice::new(data_remaining)];
                let send_fds = if fds_sent { &[] } else { &fds[..] };
                try_send(stream.get().as_fd(), &bufs, send_fds)
            })
        })
        .await
        .map_err(SendPayloadError::Io)?;
        if !fds_sent {
            fds_sent = true;
        }
        sent += n;
    }

    Ok(())
}

/// Receive a [`MeshPayload`] value from a Unix stream, receiving OS resources
/// via SCM_RIGHTS.
pub async fn recv_payload<T: MeshPayload>(
    stream: &mut PolledSocket<UnixStream>,
) -> Result<T, RecvPayloadError> {
    let msg = recv_serialized(stream).await?;
    msg.into_message().map_err(RecvPayloadError::Deserialize)
}

/// Non-generic inner function for [`recv_payload`].
async fn recv_serialized(
    stream: &mut PolledSocket<UnixStream>,
) -> Result<SerializedMessage<Resource>, RecvPayloadError> {
    // Fds may arrive with any recvmsg call — collect them across all reads.
    let mut fds = Vec::new();

    // Read the header.
    let mut header = [0u8; HEADER_LEN];
    recv_exact_with_fds(stream, &mut header, &mut fds)
        .await
        .map_err(RecvPayloadError::Io)?;

    let data_len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let fd_count = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;

    // Validate sizes to prevent DoS.
    const MAX_DATA_LEN: usize = 1024 * 1024; // 1 MiB
    const MAX_FD_COUNT: usize = 64;
    if data_len > MAX_DATA_LEN {
        return Err(RecvPayloadError::DataTooLarge {
            len: data_len,
            max: MAX_DATA_LEN,
        });
    }
    if fd_count > MAX_FD_COUNT {
        return Err(RecvPayloadError::TooManyFds {
            count: fd_count,
            max: MAX_FD_COUNT,
        });
    }

    // Read the data (fds may also arrive here if the header read was split).
    let mut data = vec![0u8; data_len];
    if !data.is_empty() {
        recv_exact_with_fds(stream, &mut data, &mut fds)
            .await
            .map_err(RecvPayloadError::Io)?;
    }

    if fds.len() != fd_count {
        return Err(RecvPayloadError::FdCountMismatch {
            expected: fd_count,
            actual: fds.len(),
        });
    }

    // Convert OsResource fds back to Resource.
    let resources: Vec<Resource> = fds.into_iter().map(Resource::Os).collect();

    Ok(SerializedMessage { data, resources })
}

/// Read exactly `buf.len()` bytes from the stream, collecting any fds received.
async fn recv_exact_with_fds(
    stream: &mut PolledSocket<UnixStream>,
    buf: &mut [u8],
    fds: &mut Vec<OsResource>,
) -> io::Result<()> {
    let mut read = 0;
    while read < buf.len() {
        let n = poll_fn(|cx| {
            stream.poll_io(cx, InterestSlot::Read, PollEvents::IN, |stream| {
                try_recv(stream.get().as_fd(), &mut buf[read..], fds)
            })
        })
        .await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        read += n;
    }
    Ok(())
}

/// A listener that binds a Unix socket and accepts connections, returning
/// polled streams ready for [`send_payload`] / [`recv_payload`].
pub struct UnixPayloadListener {
    listener: PolledSocket<UnixListener>,
}

impl UnixPayloadListener {
    /// Bind to a Unix socket path.
    ///
    /// The caller is responsible for removing any existing socket file at
    /// `path` before calling this. This function will return an error if the
    /// path already exists.
    pub fn bind(driver: &(impl Driver + ?Sized), path: &Path) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        let listener = PolledSocket::new(driver, listener)?;
        Ok(Self { listener })
    }

    /// Accept a connection. Returns a polled stream ready for
    /// [`send_payload`] / [`recv_payload`].
    pub async fn accept(
        &mut self,
        driver: &(impl Driver + ?Sized),
    ) -> io::Result<PolledSocket<UnixStream>> {
        let (stream, _addr) = self.listener.accept().await?;
        let stream = PolledSocket::new(driver, stream)?;
        Ok(stream)
    }
}

/// Connect to a Unix socket and return a polled stream ready for
/// [`send_payload`] / [`recv_payload`].
pub async fn unix_payload_connect(
    driver: &(impl Driver + ?Sized),
    path: &Path,
) -> io::Result<PolledSocket<UnixStream>> {
    PolledSocket::connect_unix(driver, path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;

    #[derive(Debug, PartialEq, mesh_protobuf::Protobuf)]
    struct SimplePayload {
        value: u64,
        text: String,
    }

    #[async_test]
    async fn test_send_recv_simple_payload(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test.sock");

        let mut listener = UnixPayloadListener::bind(&driver, &sock_path).unwrap();

        let connect_driver = driver.clone();
        let connect_path = sock_path.clone();
        let client_task = pal_async::task::Spawn::spawn(&driver, "client", async move {
            let mut stream = unix_payload_connect(&connect_driver, &connect_path)
                .await
                .unwrap();
            send_payload(
                &mut stream,
                SimplePayload {
                    value: 42,
                    text: "hello mesh".to_string(),
                },
            )
            .await
            .unwrap();
        });

        let (stream, _) = listener.listener.accept().await.unwrap();
        let stream = &mut PolledSocket::new(&driver, stream).unwrap();
        let received: SimplePayload = recv_payload(stream).await.unwrap();
        assert_eq!(received.value, 42);
        assert_eq!(received.text, "hello mesh");

        client_task.await;
    }

    #[async_test]
    async fn test_send_recv_payload_with_fd(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test_fd.sock");

        // Create a temp file to send as an fd.
        let file_path = dir.path().join("test_data.txt");
        let test_data = b"fd transfer works!";
        std::fs::write(&file_path, test_data).unwrap();

        let mut listener = UnixPayloadListener::bind(&driver, &sock_path).unwrap();

        let connect_driver = driver.clone();
        let connect_path = sock_path.clone();
        let client_task = pal_async::task::Spawn::spawn(&driver, "client", async move {
            let mut stream = unix_payload_connect(&connect_driver, &connect_path)
                .await
                .unwrap();

            // Receive a payload containing an fd.
            let received: crate::unix_node::Invitation = recv_payload(&mut stream).await.unwrap();
            // The invitation contains an fd — verify it's valid by checking
            // the address fields exist.
            assert_ne!(
                received.address.local_addr.node,
                received.address.remote_addr.node
            );
        });

        let mut server_stream = listener.accept(&driver).await.unwrap();

        // Create a UnixNode and generate an invitation to send.
        let node = crate::unix_node::UnixNode::new(driver.clone());
        let (send, _recv) = mesh_channel::channel::<u32>();
        let invitation = node.invite(send.into()).await.unwrap();
        send_payload(&mut server_stream, invitation).await.unwrap();

        client_task.await;
        node.shutdown().await;
    }
}
