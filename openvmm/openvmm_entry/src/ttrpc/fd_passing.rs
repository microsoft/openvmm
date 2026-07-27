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
//! `openvmm_ttrpc_vmservice/src/fd_passing.md` for the wire specification.
//!
//! The entire protocol is UNIX-only (it relies on `SCM_RIGHTS`), so this whole
//! module is compiled only on `cfg(unix)`.

#![cfg(unix)]

use anyhow::Context as _;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::AsSockRef;
use pal_async::socket::PolledSocket;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use unix_socket::UnixStream;

pub(super) use protocol::MAGIC_FIRST_BYTE;

/// The wire format of the fd-passing protocol: the on-the-socket constants that
/// define the framing, independent of the implementation that reads and writes
/// them. See `openvmm_ttrpc_vmservice/src/fd_passing.md` for the full
/// specification.
mod protocol {
    /// The first byte of the fd-passing handshake magic. Distinct from the ttrpc
    /// (`0x00`) and gRPC (`b'P'`) first bytes, so the server can route a connection
    /// by peeking a single byte.
    pub(crate) const MAGIC_FIRST_BYTE: u8 = 0xFD;

    /// The 4-byte handshake magic: `0xFD 'F' 'D' 0x01`. Encodes both protocol and
    /// revision.
    pub(super) const HANDSHAKE_MAGIC: [u8; 4] = [0xFD, b'F', b'D', 0x01];

    pub(super) const OPCODE_REGISTER: u8 = 1;
    pub(super) const OPCODE_DEREGISTER: u8 = 2;

    /// The number of descriptors a single connection's receiver can hold. Every
    /// message in this protocol carries at most one descriptor (a `Register`
    /// carries exactly one; nothing else carries any). A message with zero is
    /// rejected with a failure response; a message attaching more than one is a
    /// protocol violation that trips truncation and closes the connection.
    pub(super) const MAX_MESSAGE_FDS: usize = 1;
}

/// A registry of file descriptors passed in via the fd-passing protocol, keyed
/// by client-chosen name.
///
/// The registry is shared (cheaply cloneable). Names live in a single global
/// namespace so that a descriptor registered on the fd-passing connection can
/// be resolved from a separate ttrpc/gRPC connection. Cleanup, however, is
/// per-connection: the fd-passing handler drops every name it registered when
/// its connection closes.
#[derive(Clone, Default)]
pub struct FdRegistry {
    inner: Arc<Mutex<HashMap<String, OwnedFd>>>,
}

impl FdRegistry {
    /// Registers `fd` under `name`, failing (and dropping `fd`) if the name is
    /// already registered by any connection.
    fn register(&self, name: String, fd: OwnedFd) -> anyhow::Result<()> {
        let mut map = self.inner.lock();
        if map.contains_key(&name) {
            anyhow::bail!("name '{name}' is already registered");
        }
        map.insert(name, fd);
        Ok(())
    }

    /// Removes `name` from the registry, dropping (closing) its descriptor.
    fn deregister(&self, name: &str) {
        self.inner.lock().remove(name);
    }

    /// Resolves `name` to a freshly duplicated descriptor. The registry entry
    /// remains valid.
    ///
    /// Only the tap NIC backend (`cfg(target_os = "linux")`) and the unit tests
    /// call this, so on other unix targets (e.g. macOS) it is otherwise
    /// unused; suppress the resulting dead-code warning there.
    #[cfg_attr(not(any(target_os = "linux", test)), expect(dead_code))]
    pub fn resolve(&self, name: &str) -> anyhow::Result<OwnedFd> {
        let map = self.inner.lock();
        let fd = map
            .get(name)
            .with_context(|| format!("no file descriptor registered under name '{name}'"))?;
        fd.try_clone()
            .context("failed to duplicate registered file descriptor")
    }
}

/// Services a single fd-passing connection: validates the handshake, then loops
/// handling `Register`/`Deregister` requests until the peer closes the
/// connection. Any names registered by this connection are dropped on exit.
pub(super) async fn serve(
    conn: PolledSocket<UnixStream>,
    registry: &FdRegistry,
) -> anyhow::Result<()> {
    let mut conn = Connection::new(conn);

    // Exchange handshakes: read and validate the client's, then send ours.
    let handshake = conn
        .read_exact(8)
        .await
        .context("failed to read fd-passing handshake")?;
    if handshake[..4] != protocol::HANDSHAKE_MAGIC {
        anyhow::bail!("invalid fd-passing handshake magic");
    }
    // `features` (handshake[4..8]) is reserved and ignored in this revision.
    // The handshake carries no ancillary data; a descriptor attached here is a
    // protocol violation, so fail fast and close the connection.
    if conn.fd.is_some() {
        anyhow::bail!("fd-passing handshake must not carry a descriptor");
    }

    let mut server_handshake = [0u8; 8];
    server_handshake[..4].copy_from_slice(&protocol::HANDSHAKE_MAGIC);
    conn.write_all(&server_handshake)
        .await
        .context("failed to send fd-passing handshake")?;

    // Track the names registered by this connection so they are dropped when
    // it closes. `OwnedNames` deregisters on drop, so cleanup runs even if this
    // future is cancelled (dropped) before `serve_requests` returns.
    let mut owned_names = OwnedNames {
        registry,
        names: Vec::new(),
    };
    serve_requests(&mut conn, &mut owned_names).await
}

/// Tracks the names a connection has registered and deregisters them on drop,
/// so cleanup runs even if the serving future is dropped mid-flight (e.g. when
/// the accept loop is cancelled) rather than leaking them into the registry.
struct OwnedNames<'a> {
    registry: &'a FdRegistry,
    names: Vec<String>,
}

impl Drop for OwnedNames<'_> {
    fn drop(&mut self) {
        for name in self.names.drain(..) {
            self.registry.deregister(&name);
        }
    }
}

async fn serve_requests(
    conn: &mut Connection,
    owned_names: &mut OwnedNames<'_>,
) -> anyhow::Result<()> {
    loop {
        // Read the request header (opcode + name length). A clean EOF here (at
        // a frame boundary) is a normal shutdown.
        let Some(header) = conn
            .try_read_exact(2)
            .await
            .context("failed to read request header")?
        else {
            return Ok(());
        };
        let opcode = header[0];
        let name_len = header[1] as usize;
        let name_bytes = conn
            .read_exact(name_len)
            .await
            .context("failed to read request name")?;

        // Take the descriptor (if any) that arrived with this message, clearing
        // it so it never lingers into the next message. Only a register consumes
        // one; any other message carrying a descriptor is malformed and is
        // rejected below (or closes the connection).
        let fd = conn.fd.take();

        match opcode {
            protocol::OPCODE_REGISTER => match parse_name(&name_bytes) {
                Some(name) => match fd {
                    Some(fd) => match owned_names.registry.register(name.clone(), fd) {
                        Ok(()) => {
                            owned_names.names.push(name);
                            conn.write_response(None).await?;
                        }
                        Err(err) => conn.write_response(Some(&err.to_string())).await?,
                    },
                    None => {
                        conn.write_response(Some(
                            "register requires exactly one attached descriptor",
                        ))
                        .await?
                    }
                },
                None => {
                    // Invalid name; drop any attached descriptor.
                    drop(fd);
                    conn.write_response(Some("invalid name")).await?;
                }
            },
            protocol::OPCODE_DEREGISTER => match parse_name(&name_bytes) {
                _ if fd.is_some() => {
                    conn.write_response(Some("deregister does not accept an attached descriptor"))
                        .await?
                }
                Some(name) => {
                    if let Some(pos) = owned_names.names.iter().position(|n| *n == name) {
                        owned_names.names.swap_remove(pos);
                        owned_names.registry.deregister(&name);
                        conn.write_response(None).await?;
                    } else {
                        conn.write_response(Some("name not registered by this connection"))
                            .await?;
                    }
                }
                None => conn.write_response(Some("invalid name")).await?,
            },
            // An unknown opcode is unrecoverable: the stream has no length
            // prefix to resynchronize on, so close the connection. Any received
            // descriptors are dropped with `conn`.
            other => anyhow::bail!("unknown fd-passing opcode {other}"),
        }
    }
}

/// Validates a request name: it must be non-empty and valid UTF-8.
fn parse_name(bytes: &[u8]) -> Option<String> {
    match std::str::from_utf8(bytes) {
        Ok(name) if !name.is_empty() => Some(name.to_owned()),
        _ => None,
    }
}

/// Buffers a stream socket, reassembling length-known frames and holding the
/// single descriptor received via `SCM_RIGHTS` with the current message.
struct Connection {
    conn: PolledSocket<UnixStream>,
    read_buf: VecDeque<u8>,
    receiver: unix_socket::ScmReceiver,
    /// The descriptor received with the message currently being read, if any.
    /// Every message carries at most one (`MAX_MESSAGE_FDS == 1`), and the
    /// kernel never merges descriptors across a sender's message boundary, so
    /// at most one is ever held: each message consumes or discards it before
    /// the next message's header is read.
    fd: Option<OwnedFd>,
}

impl Connection {
    fn new(conn: PolledSocket<UnixStream>) -> Self {
        Self {
            conn,
            read_buf: VecDeque::new(),
            receiver: unix_socket::ScmReceiver::new(protocol::MAX_MESSAGE_FDS),
            fd: None,
        }
    }

    /// Performs a single `recvmsg`, appending received bytes to the read buffer
    /// and taking the at-most-one descriptor into `fd`. Returns the number of
    /// bytes read (0 on EOF).
    async fn fill(&mut self) -> io::Result<usize> {
        let mut buf = [0u8; 512];
        let n = poll_fn(|cx| {
            self.conn
                .poll_io(cx, InterestSlot::Read, PollEvents::IN, |this| {
                    let sock = this.get().as_sock_ref();
                    self.receiver.recv(sock.as_fd(), &mut buf)
                })
        })
        .await?;
        self.read_buf.extend(&buf[..n]);
        // Take the at-most-one descriptor that arrived with this `recv`. A
        // conforming client attaches a descriptor only to a `Register` and
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

    /// Reads exactly `n` bytes, erroring on EOF.
    async fn read_exact(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.read_buf.len() < n {
            if self.fill().await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(self.read_buf.drain(..n).collect())
    }

    /// Reads exactly `n` bytes, returning `None` if the peer closes the
    /// connection at a frame boundary (before any of the `n` bytes arrive) and
    /// erroring on a partial (mid-frame) EOF.
    async fn try_read_exact(&mut self, n: usize) -> io::Result<Option<Vec<u8>>> {
        while self.read_buf.len() < n {
            if self.fill().await? == 0 {
                if self.read_buf.is_empty() {
                    return Ok(None);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(Some(self.read_buf.drain(..n).collect()))
    }

    /// Writes a response frame: `status: u8`, `msg_len: u16` (LE), `msg`.
    /// `None` means success; `Some(msg)` is a generic failure with diagnostic
    /// text.
    async fn write_response(&mut self, error: Option<&str>) -> io::Result<()> {
        let (status, msg) = match error {
            None => (0u8, ""),
            Some(msg) => (1u8, msg),
        };
        let msg = msg.as_bytes();
        let msg_len = msg.len().min(u16::MAX as usize);
        let mut frame = Vec::with_capacity(3 + msg_len);
        frame.push(status);
        frame.extend_from_slice(&(msg_len as u16).to_le_bytes());
        frame.extend_from_slice(&msg[..msg_len]);
        self.write_all(&frame).await
    }

    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        use futures::AsyncWriteExt;
        self.conn.write_all(buf).await
    }
}

#[cfg(test)]
mod tests {
    use super::FdRegistry;
    use super::protocol::HANDSHAKE_MAGIC;
    use super::protocol::OPCODE_DEREGISTER;
    use super::protocol::OPCODE_REGISTER;
    use super::serve;
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
        let registry = FdRegistry::default();
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
        let registry = FdRegistry::default();
        let (server_sock, client_sock) = socket_pair();

        // Run the server on its own pal_async pool so the blocking client below
        // can drive it from this thread without deadlocking.
        let (_thread, driver) = DefaultPool::spawn_on_thread("fd-passing-test");
        let server = PolledSocket::new(&driver, UnixStream::from(server_sock)).unwrap();
        let server_registry = registry.clone();
        let task = driver.spawn(
            "serve",
            async move { serve(server, &server_registry).await },
        );

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
