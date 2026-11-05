// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The OpenVMM fd-passing protocol.
//!
//! This is a small, bespoke protocol that lets a client hand pre-opened file
//! descriptors to the management server, each under a client-chosen name. A
//! name can then be referenced from the ordinary ttrpc/gRPC API (for example a
//! `TapBackend` may give an `fd_name` instead of a device `name`).
//!
//! It runs over the same `AF_UNIX` stream socket as ttrpc and gRPC, selected by
//! a distinct magic first byte, and carries descriptors via `SCM_RIGHTS`. See
//! `openvmm_ttrpc_vmservice/src/resource_passing.md` for the wire specification.
//!
//! This module implements only how descriptors arrive; the handshake, framing,
//! request semantics, and registry are shared with the Windows handle-passing
//! protocol in `resource_passing`.
//!
//! The entire protocol is UNIX-only (it relies on `SCM_RIGHTS`), so this whole
//! module is compiled only on `cfg(unix)`.

#![cfg(unix)]

use super::resource_passing::Resource;
use super::resource_passing::Transport;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::AsSockRef;
use pal_async::socket::PolledSocket;
use std::future::poll_fn;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::io::OwnedFd;
use unix_socket::UnixStream;

/// The wire format of the fd-passing protocol: the on-the-socket constants that
/// define the framing, independent of the implementation that reads and writes
/// them. See `openvmm_ttrpc_vmservice/src/resource_passing.md` for the full
/// specification.
mod protocol {
    /// The 4-byte handshake magic: `0xFD 'F' 'D' 0x01`. Encodes both protocol and
    /// revision.
    pub(super) const HANDSHAKE_MAGIC: [u8; 4] = [0xFD, b'F', b'D', 0x01];

    /// The number of descriptors a single connection's receiver can hold. Every
    /// message in this protocol carries at most one descriptor (a `Register`
    /// carries exactly one; nothing else carries any). A message with zero is
    /// rejected with a failure response; a message attaching more than one is a
    /// protocol violation that trips truncation and closes the connection.
    pub(super) const MAX_MESSAGE_FDS: usize = 1;
}

impl Resource for OwnedFd {
    const KIND: &'static str = "file descriptor";

    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }
}

/// An fd-passing connection, holding the single descriptor received via
/// `SCM_RIGHTS` with the current message.
pub(super) struct Connection {
    conn: PolledSocket<UnixStream>,
    receiver: unix_socket::ScmReceiver,
    /// The descriptor received with the message currently being read, if any.
    /// Every message carries at most one (`MAX_MESSAGE_FDS == 1`), and the
    /// kernel never merges descriptors across a sender's message boundary, so
    /// at most one is ever held: each message consumes or discards it before
    /// the next message's header is read.
    fd: Option<OwnedFd>,
}

impl Connection {
    pub(super) fn new(conn: PolledSocket<UnixStream>) -> Self {
        Self {
            conn,
            receiver: unix_socket::ScmReceiver::new(protocol::MAX_MESSAGE_FDS),
            fd: None,
        }
    }
}

impl Transport for Connection {
    type Resource = OwnedFd;
    const MAGIC: [u8; 4] = protocol::HANDSHAKE_MAGIC;
    /// Descriptors travel out of band, via `SCM_RIGHTS`.
    const INLINE_RESOURCE_LEN: usize = 0;

    /// Performs a single `recvmsg`, taking the at-most-one descriptor into
    /// `fd`.
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = poll_fn(|cx| {
            self.conn
                .poll_io(cx, InterestSlot::Read, PollEvents::IN, |this| {
                    let sock = this.get().as_sock_ref();
                    self.receiver.recv(sock.as_fd(), buf)
                })
        })
        .await?;
        // A conforming client attaches a descriptor only to a `Register` and
        // reads the response before sending anything else, so the previous
        // message's descriptor has already been consumed and `fd` is empty
        // here. A misbehaving client can still attach descriptors to other
        // bytes; since the protocol requires the server never panic on any
        // input, treat an unconsumed descriptor as a protocol violation and
        // error out (dropping both descriptors and closing the connection)
        // rather than asserting.
        if let Some(fd) = self.receiver.drain().next() {
            if self.fd.replace(fd).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "received a descriptor before the previous one was consumed",
                ));
            }
        }
        Ok(n)
    }

    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        use futures::AsyncWriteExt;
        self.conn.write_all(buf).await
    }

    fn take_received(&mut self) -> Option<OwnedFd> {
        self.fd.take()
    }
}

#[cfg(test)]
mod tests {
    use super::Connection;
    use super::protocol::HANDSHAKE_MAGIC;
    use crate::ttrpc::ResourceRegistry;
    use crate::ttrpc::resource_passing;
    use crate::ttrpc::resource_passing::wire::OPCODE_DEREGISTER;
    use crate::ttrpc::resource_passing::wire::OPCODE_REGISTER;
    use pal_async::DefaultPool;
    use pal_async::socket::PolledSocket;
    use pal_async::task::Spawn;
    use socket2::Domain;
    use socket2::Socket;
    use socket2::Type;
    use std::io::IoSlice;
    use std::io::Read as _;
    use std::io::Write as _;
    use std::os::fd::AsFd;
    use std::os::fd::BorrowedFd;
    use std::os::unix::net::UnixStream;

    /// Creates a connected AF_UNIX stream socket pair.
    fn socket_pair() -> (Socket, Socket) {
        Socket::pair(Domain::UNIX, Type::STREAM, None).unwrap()
    }

    /// A blocking client for the fd-passing protocol, used to drive the server
    /// over a real socket in tests.
    struct TestClient {
        sock: Socket,
    }

    impl TestClient {
        fn handshake(&mut self) {
            self.sock
                .write_all(&[0xFD, b'F', b'D', 0x01, 0, 0, 0, 0])
                .unwrap();
            let mut buf = [0u8; 8];
            self.sock.read_exact(&mut buf).unwrap();
            assert_eq!(buf[..4], HANDSHAKE_MAGIC);
        }

        fn register(&mut self, name: &str, fd: BorrowedFd<'_>) -> (u8, String) {
            let mut frame = vec![OPCODE_REGISTER, name.len() as u8];
            frame.extend_from_slice(name.as_bytes());
            unix_socket::send_with_fds(self.sock.as_fd(), &[IoSlice::new(&frame)], [fd]).unwrap();
            self.read_response()
        }

        fn register_without_fd(&mut self, name: &str) -> (u8, String) {
            let mut frame = vec![OPCODE_REGISTER, name.len() as u8];
            frame.extend_from_slice(name.as_bytes());
            self.sock.write_all(&frame).unwrap();
            self.read_response()
        }

        fn deregister(&mut self, name: &str) -> (u8, String) {
            let mut frame = vec![OPCODE_DEREGISTER, name.len() as u8];
            frame.extend_from_slice(name.as_bytes());
            self.sock.write_all(&frame).unwrap();
            self.read_response()
        }

        fn deregister_with_fd(&mut self, name: &str, fd: BorrowedFd<'_>) -> (u8, String) {
            let mut frame = vec![OPCODE_DEREGISTER, name.len() as u8];
            frame.extend_from_slice(name.as_bytes());
            unix_socket::send_with_fds(self.sock.as_fd(), &[IoSlice::new(&frame)], [fd]).unwrap();
            self.read_response()
        }

        fn read_response(&mut self) -> (u8, String) {
            let mut hdr = [0u8; 3];
            self.sock.read_exact(&mut hdr).unwrap();
            let status = hdr[0];
            let msg_len = u16::from_le_bytes([hdr[1], hdr[2]]) as usize;
            let mut msg = vec![0u8; msg_len];
            self.sock.read_exact(&mut msg).unwrap();
            (status, String::from_utf8(msg).unwrap())
        }
    }

    #[test]
    fn registry_semantics() {
        let registry = ResourceRegistry::default();
        let (a, mut b) = socket_pair();
        registry.register("x".to_owned(), a.into()).unwrap();

        // Registering an already-registered name fails.
        let (c, _d) = socket_pair();
        assert!(registry.register("x".to_owned(), c.into()).is_err());

        // Resolving dups the descriptor; the dup refers to the same file, so it
        // is still connected to peer `b`.
        let dup = registry.resolve("x").unwrap();
        Socket::from(dup).write_all(b"hi").unwrap();
        let mut buf = [0u8; 2];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");

        // Resolving an unknown name fails.
        assert!(registry.resolve("nope").is_err());

        // Deregistering drops the entry.
        registry.deregister("x");
        assert!(registry.resolve("x").is_err());
    }

    #[test]
    fn end_to_end() {
        let registry = ResourceRegistry::default();
        let (server_sock, client_sock) = socket_pair();

        // Run the server on its own pal_async pool so the blocking client below
        // can drive it from this thread without deadlocking.
        let (_thread, driver) = DefaultPool::spawn_on_thread("fd-passing-test");
        let server = PolledSocket::new(&driver, UnixStream::from(server_sock)).unwrap();
        let server_registry = registry.clone();
        let task = driver.spawn("serve", async move {
            resource_passing::serve(&mut Connection::new(server), &server_registry).await
        });

        let mut client = TestClient { sock: client_sock };
        client.handshake();

        // A second socket pair whose one end is registered; used to verify the
        // resolved fd refers to the same file.
        let (registered, mut peer) = socket_pair();

        // Register succeeds.
        let (status, msg) = client.register("t0", registered.as_fd());
        assert_eq!(status, 0, "{msg}");

        // The name now resolves; the dup talks to the same peer.
        let dup = registry.resolve("t0").unwrap();
        Socket::from(dup).write_all(b"ok").unwrap();
        let mut buf = [0u8; 2];
        peer.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ok");

        // Registering the same name again fails; the connection stays usable.
        let (status, _) = client.register("t0", registered.as_fd());
        assert_eq!(status, 1);

        // Registering without an attached descriptor fails.
        let (status, _) = client.register_without_fd("t2");
        assert_eq!(status, 1);

        // Deregistering an unknown name fails.
        let (status, _) = client.deregister("nope");
        assert_eq!(status, 1);

        // Deregistering with an attached descriptor is malformed and fails; the
        // connection stays usable.
        let (status, _) = client.deregister_with_fd("t0", registered.as_fd());
        assert_eq!(status, 1);

        // Deregistering the real name succeeds and removes it.
        let (status, _) = client.deregister("t0");
        assert_eq!(status, 0);
        assert!(registry.resolve("t0").is_err());

        // Register again, then close the connection: per-connection cleanup
        // must drop the name.
        let (status, _) = client.register("t1", registered.as_fd());
        assert_eq!(status, 0);
        assert!(registry.resolve("t1").is_ok());
        drop(client);

        // serve() returns once the client closes; its cleanup drops t1.
        futures::executor::block_on(task).unwrap();
        assert!(registry.resolve("t1").is_err());
    }
}
