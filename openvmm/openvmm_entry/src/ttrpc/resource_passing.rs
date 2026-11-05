// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The OpenVMM resource-passing protocols: fd-passing (unix) and
//! handle-passing (Windows).
//!
//! These are small, bespoke protocols that let a client hand pre-opened OS
//! resources (file descriptors or handles) to the management server, each under
//! a client-chosen name. A name can then be referenced from the ordinary
//! ttrpc/gRPC API, for example a `TapBackend` `fd_name` or a `VirtioFs`
//! `WindowsOptions.user_token_name`.
//!
//! Both run over the same `AF_UNIX` stream socket as ttrpc and gRPC, selected
//! by the magic first byte [`wire::MAGIC_FIRST_BYTE`]. See
//! `openvmm_ttrpc_vmservice/src/resource_passing.md` for the wire
//! specification.
//!
//! This module implements everything the protocols share: the handshake,
//! request and response framing, request semantics, and the registry. They
//! differ only in how a `Register` request carries its resource, which a
//! [`Transport`] implements in the platform modules `fd_passing` and
//! `handle_passing`.

use anyhow::Context as _;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;

/// Wire constants common to both transports. See
/// `openvmm_ttrpc_vmservice/src/resource_passing.md`.
pub(super) mod wire {
    /// The first byte of every handshake magic. Distinct from the ttrpc
    /// (`0x00`) and gRPC (`b'P'`) first bytes, so the server can route a
    /// connection by peeking a single byte.
    pub(in crate::ttrpc) const MAGIC_FIRST_BYTE: u8 = 0xFD;

    /// The length of the handshake: a 4-byte magic and a `u32` features field.
    pub(in crate::ttrpc) const HANDSHAKE_LEN: usize = 8;

    pub(in crate::ttrpc) const OPCODE_REGISTER: u8 = 1;
    pub(in crate::ttrpc) const OPCODE_DEREGISTER: u8 = 2;

    pub(in crate::ttrpc) const STATUS_OK: u8 = 0;
    pub(in crate::ttrpc) const STATUS_FAILURE: u8 = 1;
}

/// A connected client stream and how it carries resources.
pub(super) trait Transport {
    type Resource: Resource;

    /// The 4-byte handshake magic, which identifies protocol and revision.
    const MAGIC: [u8; 4];

    /// The number of bytes of resource data that follow the name in a
    /// `Register` request.
    const INLINE_RESOURCE_LEN: usize;

    /// Performs a single read into `buf`, returning the number of bytes read
    /// (0 on EOF). Resources received out of band are held for
    /// [`Self::take_received`].
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Writes all of `buf`.
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;

    /// Takes the resource received out of band since the last call, if any.
    fn take_received(&mut self) -> Option<Self::Resource> {
        None
    }

    /// Decodes the [`Self::INLINE_RESOURCE_LEN`] bytes of resource data in a
    /// `Register` request.
    fn decode_inline(data: &[u8]) -> anyhow::Result<Option<Self::Resource>> {
        let _ = data;
        Ok(None)
    }
}

/// Services a single connection: validates the handshake, then handles
/// `Register`/`Deregister` requests until the peer closes the connection.
/// Names registered by this connection are dropped on exit, including when
/// this future is cancelled.
pub(super) async fn serve<T: Transport>(
    transport: &mut T,
    registry: &Registry<T::Resource>,
) -> anyhow::Result<()> {
    let mut conn = Stream::new(transport);

    // Exchange handshakes: read and validate the client's, then send ours.
    let handshake = conn
        .read_exact(wire::HANDSHAKE_LEN)
        .await
        .context("failed to read handshake")?;
    anyhow::ensure!(handshake[..4] == T::MAGIC, "invalid handshake magic");
    // `features` is reserved and ignored in this revision.
    anyhow::ensure!(
        conn.transport.take_received().is_none(),
        "handshake must not carry a {}",
        T::Resource::KIND
    );
    conn.transport
        .write_all(&handshake_bytes(T::MAGIC))
        .await
        .context("failed to send handshake")?;

    let mut registrations = Registrations::new(registry.clone());
    loop {
        // A clean EOF at a frame boundary is a normal shutdown.
        let Some(header) = conn
            .try_read_exact(2)
            .await
            .context("failed to read request header")?
        else {
            return Ok(());
        };
        let opcode = header[0];
        let name = conn
            .read_exact(header[1].into())
            .await
            .context("failed to read request name")?;
        let inline = if opcode == wire::OPCODE_REGISTER {
            Some(
                conn.read_exact(T::INLINE_RESOURCE_LEN)
                    .await
                    .context("failed to read request resource")?,
            )
        } else {
            None
        };
        // Take any resource that arrived with this request, so it never
        // lingers into the next one.
        let received = conn.transport.take_received();

        let result = inline
            .map_or(Ok(None), |inline| T::decode_inline(&inline))
            .map_err(RequestError::Failed)
            .and_then(|inline| {
                let resource = match (received, inline) {
                    (Some(_), Some(_)) => {
                        return Err(RequestError::Failed(anyhow::anyhow!(
                            "a request may carry at most one {}",
                            T::Resource::KIND
                        )));
                    }
                    (received, inline) => received.or(inline),
                };
                registrations.handle(opcode, &name, resource)
            });
        let error = match result {
            Ok(()) => None,
            Err(RequestError::Failed(err)) => Some(format!("{err:#}")),
            // An unknown opcode is unrecoverable: the stream has no length
            // prefix to resynchronize on, so close the connection.
            Err(err @ RequestError::UnknownOpcode(_)) => {
                return Err(anyhow::Error::from(err).context("protocol violation"));
            }
        };
        conn.transport
            .write_all(&encode_response(error.as_deref()))
            .await
            .context("failed to send response")?;
    }
}

/// Buffers a [`Transport`], reassembling length-known frames.
struct Stream<'a, T> {
    transport: &'a mut T,
    read_buf: VecDeque<u8>,
}

impl<'a, T: Transport> Stream<'a, T> {
    fn new(transport: &'a mut T) -> Self {
        Self {
            transport,
            read_buf: VecDeque::new(),
        }
    }

    /// Performs a single read, appending to the read buffer. Returns the number
    /// of bytes read (0 on EOF).
    async fn fill(&mut self) -> io::Result<usize> {
        let mut buf = [0u8; 512];
        let n = self.transport.read(&mut buf).await?;
        self.read_buf.extend(&buf[..n]);
        Ok(n)
    }

    /// Reads exactly `n` bytes, erroring on EOF.
    async fn read_exact(&mut self, n: usize) -> io::Result<Vec<u8>> {
        self.try_read_exact(n)
            .await?
            .ok_or_else(|| io::ErrorKind::UnexpectedEof.into())
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
}

/// An owned OS resource that can be passed by name.
pub(super) trait Resource: Sized {
    /// A human-readable name for the resource kind, used in diagnostics.
    const KIND: &'static str;

    /// Duplicates the resource, referring to the same underlying object.
    fn duplicate(&self) -> io::Result<Self>;
}

/// A registry of resources passed in by a client, keyed by client-chosen name.
///
/// The registry is shared (cheaply cloneable). Names live in a single global
/// namespace so that a resource registered on a passing connection can be
/// resolved from a separate ttrpc/gRPC connection. Cleanup is per-connection;
/// see [`Registrations`].
pub(super) struct Registry<T> {
    inner: Arc<Mutex<HashMap<String, T>>>,
}

impl<T> Clone for Registry<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Self {
            inner: Default::default(),
        }
    }
}

impl<T: Resource> Registry<T> {
    /// Registers `resource` under `name`, failing (and dropping `resource`) if
    /// the name is already registered by any connection.
    pub(super) fn register(&self, name: String, resource: T) -> anyhow::Result<()> {
        let mut map = self.inner.lock();
        if map.contains_key(&name) {
            anyhow::bail!("name '{name}' is already registered");
        }
        map.insert(name, resource);
        Ok(())
    }

    /// Removes `name` from the registry, dropping its resource.
    pub(super) fn deregister(&self, name: &str) {
        self.inner.lock().remove(name);
    }

    /// Resolves `name` to a freshly duplicated resource. The registry entry
    /// remains valid.
    ///
    /// Only the Linux tap backend, Windows virtio-fs, and the unit tests call
    /// this, so it is otherwise unused (e.g. on macOS).
    #[cfg_attr(not(any(target_os = "linux", windows, test)), expect(dead_code))]
    pub(super) fn resolve(&self, name: &str) -> anyhow::Result<T> {
        let map = self.inner.lock();
        let resource = map
            .get(name)
            .with_context(|| format!("no {} registered under name '{name}'", T::KIND))?;
        resource
            .duplicate()
            .with_context(|| format!("failed to duplicate registered {}", T::KIND))
    }
}

/// An error handling a request.
#[derive(Debug, thiserror::Error)]
pub(super) enum RequestError {
    /// The opcode is unknown. Stream transports cannot resynchronize after
    /// this, so they close the connection.
    #[error("unknown opcode {0}")]
    UnknownOpcode(u8),
    /// The request failed; the connection remains usable.
    #[error(transparent)]
    Failed(anyhow::Error),
}

/// The names registered by one connection.
///
/// Dropping this deregisters every name, so cleanup runs however the
/// connection ends, including when its serving future is cancelled.
pub(super) struct Registrations<T: Resource> {
    registry: Registry<T>,
    names: Vec<String>,
}

impl<T: Resource> Registrations<T> {
    pub(super) fn new(registry: Registry<T>) -> Self {
        Self {
            registry,
            names: Vec::new(),
        }
    }

    /// Handles a decoded request: `opcode`, the raw `name` bytes, and the
    /// resource (if any) that arrived with it. A resource not consumed by a
    /// successful registration is dropped.
    ///
    /// The transport is responsible for rejecting requests that carry more
    /// than one resource.
    pub(super) fn handle(
        &mut self,
        opcode: u8,
        name: &[u8],
        resource: Option<T>,
    ) -> Result<(), RequestError> {
        let failed = |err: anyhow::Error| RequestError::Failed(err);
        match opcode {
            wire::OPCODE_REGISTER => {
                let name = parse_name(name).map_err(failed)?;
                let resource = resource
                    .with_context(|| format!("register requires exactly one {}", T::KIND))
                    .map_err(failed)?;
                self.registry
                    .register(name.to_owned(), resource)
                    .map_err(failed)?;
                self.names.push(name.to_owned());
            }
            wire::OPCODE_DEREGISTER => {
                if resource.is_some() {
                    return Err(failed(anyhow::anyhow!(
                        "deregister does not accept a {}",
                        T::KIND
                    )));
                }
                let name = parse_name(name).map_err(failed)?;
                let index = self
                    .names
                    .iter()
                    .position(|n| n == name)
                    .with_context(|| format!("name '{name}' is not registered by this connection"))
                    .map_err(failed)?;
                self.names.swap_remove(index);
                self.registry.deregister(name);
            }
            _ => return Err(RequestError::UnknownOpcode(opcode)),
        }
        Ok(())
    }
}

impl<T: Resource> Drop for Registrations<T> {
    fn drop(&mut self) {
        for name in self.names.drain(..) {
            self.registry.deregister(&name);
        }
    }
}

/// Validates a request name: it must be non-empty and valid UTF-8.
fn parse_name(bytes: &[u8]) -> anyhow::Result<&str> {
    match std::str::from_utf8(bytes) {
        Ok(name) if !name.is_empty() => Ok(name),
        _ => anyhow::bail!("invalid name: must be non-empty UTF-8"),
    }
}

/// Returns the handshake for `magic`, with `features = 0`.
fn handshake_bytes(magic: [u8; 4]) -> [u8; wire::HANDSHAKE_LEN] {
    let mut handshake = [0; wire::HANDSHAKE_LEN];
    handshake[..4].copy_from_slice(&magic);
    handshake
}

/// Encodes a response frame: `status: u8`, `msg_len: u16` (LE), `msg`. `None`
/// is success; `Some(msg)` is a generic failure with diagnostic text, truncated
/// to fit on a character boundary.
fn encode_response(error: Option<&str>) -> Vec<u8> {
    let (status, msg) = match error {
        None => (wire::STATUS_OK, ""),
        Some(msg) => (wire::STATUS_FAILURE, msg),
    };
    let mut len = msg.len().min(u16::MAX.into());
    while !msg.is_char_boundary(len) {
        len -= 1;
    }
    let mut frame = Vec::with_capacity(3 + len);
    frame.push(status);
    frame.extend_from_slice(&(len as u16).to_le_bytes());
    frame.extend_from_slice(&msg.as_bytes()[..len]);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    /// A stand-in resource that records its identity across duplication.
    #[derive(Debug, PartialEq)]
    struct Fake(u32);

    impl Resource for Fake {
        const KIND: &'static str = "fake";

        fn duplicate(&self) -> io::Result<Self> {
            Ok(Fake(self.0))
        }
    }

    fn register(r: &mut Registrations<Fake>, name: &str, id: Option<u32>) -> Result<(), String> {
        r.handle(wire::OPCODE_REGISTER, name.as_bytes(), id.map(Fake))
            .map_err(|e| e.to_string())
    }

    fn deregister(r: &mut Registrations<Fake>, name: &str, id: Option<u32>) -> Result<(), String> {
        r.handle(wire::OPCODE_DEREGISTER, name.as_bytes(), id.map(Fake))
            .map_err(|e| e.to_string())
    }

    #[test]
    fn register_resolve_deregister() {
        let registry = Registry::default();
        let mut conn = Registrations::new(registry.clone());

        register(&mut conn, "a", Some(1)).unwrap();
        // Resolving duplicates and does not consume the registration.
        assert_eq!(registry.resolve("a").unwrap(), Fake(1));
        assert_eq!(registry.resolve("a").unwrap(), Fake(1));

        register(&mut conn, "a", Some(2)).unwrap_err();
        assert_eq!(registry.resolve("a").unwrap(), Fake(1));

        deregister(&mut conn, "a", None).unwrap();
        registry.resolve("a").unwrap_err();
        deregister(&mut conn, "a", None).unwrap_err();
    }

    #[test]
    fn rejects_invalid_requests() {
        let registry = Registry::default();
        let mut conn = Registrations::new(registry.clone());

        register(&mut conn, "a", None).unwrap_err();
        register(&mut conn, "", Some(1)).unwrap_err();
        conn.handle(wire::OPCODE_REGISTER, &[0xff], Some(Fake(1)))
            .unwrap_err();
        assert!(matches!(
            conn.handle(99, b"a", Some(Fake(1))),
            Err(RequestError::UnknownOpcode(99))
        ));

        register(&mut conn, "a", Some(1)).unwrap();
        deregister(&mut conn, "a", Some(2)).unwrap_err();
        registry.resolve("a").unwrap();
    }

    #[test]
    fn names_are_owned_per_connection() {
        let registry = Registry::default();
        let mut owner = Registrations::new(registry.clone());
        let mut other = Registrations::new(registry.clone());

        register(&mut owner, "a", Some(1)).unwrap();
        deregister(&mut other, "a", None).unwrap_err();
        register(&mut other, "a", Some(2)).unwrap_err();

        drop(owner);
        registry.resolve("a").unwrap_err();
        register(&mut other, "a", Some(2)).unwrap();
    }

    #[test]
    fn response_encoding() {
        assert_eq!(encode_response(None), [0, 0, 0]);
        assert_eq!(encode_response(Some("boom")), b"\x01\x04\x00boom");
        // Truncation keeps whole characters.
        let long = "é".repeat(40000);
        let frame = encode_response(Some(&long));
        assert_eq!(u16::from_le_bytes([frame[1], frame[2]]), u16::MAX - 1);
    }

    /// A transport over in-memory buffers whose `Register` requests carry an
    /// inline little-endian `u32` resource, as handle-passing does.
    struct InlineTransport {
        input: VecDeque<u8>,
        output: Vec<u8>,
    }

    impl Transport for InlineTransport {
        type Resource = Fake;
        const MAGIC: [u8; 4] = [0xFD, b'X', b'Y', 0x01];
        const INLINE_RESOURCE_LEN: usize = 4;

        async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // Return one byte at a time to exercise frame reassembly.
            match self.input.pop_front() {
                Some(b) if !buf.is_empty() => {
                    buf[0] = b;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }

        async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
            self.output.extend_from_slice(buf);
            Ok(())
        }

        fn decode_inline(data: &[u8]) -> anyhow::Result<Option<Fake>> {
            let id = u32::from_le_bytes(data.try_into().unwrap());
            anyhow::ensure!(id != 0, "invalid resource");
            Ok(Some(Fake(id)))
        }
    }

    fn request(opcode: u8, name: &str, inline: &[u8]) -> Vec<u8> {
        let mut frame = vec![opcode, name.len() as u8];
        frame.extend_from_slice(name.as_bytes());
        frame.extend_from_slice(inline);
        frame
    }

    /// Runs `serve` over `input`, returning the result, the response
    /// statuses, and the registry.
    fn run(input: Vec<u8>) -> (anyhow::Result<()>, Vec<u8>, Registry<Fake>) {
        let registry = Registry::default();
        let mut transport = InlineTransport {
            input: input.into(),
            output: Vec::new(),
        };
        let result = futures::executor::block_on(serve(&mut transport, &registry));
        let mut output = &transport.output[..];
        if output.len() >= wire::HANDSHAKE_LEN {
            assert_eq!(
                output[..wire::HANDSHAKE_LEN],
                handshake_bytes(InlineTransport::MAGIC)
            );
            output = &output[wire::HANDSHAKE_LEN..];
        }
        let mut statuses = Vec::new();
        while let [status, lo, hi, rest @ ..] = output {
            statuses.push(*status);
            output = &rest[u16::from_le_bytes([*lo, *hi]).into()..];
        }
        (result, statuses, registry)
    }

    #[test]
    fn serves_inline_requests() {
        let mut input = handshake_bytes(InlineTransport::MAGIC).to_vec();
        input.extend(request(wire::OPCODE_REGISTER, "a", &7u32.to_le_bytes()));
        input.extend(request(wire::OPCODE_REGISTER, "b", &0u32.to_le_bytes()));
        input.extend(request(wire::OPCODE_REGISTER, "c", &8u32.to_le_bytes()));
        input.extend(request(wire::OPCODE_DEREGISTER, "c", &[]));
        input.extend(request(wire::OPCODE_DEREGISTER, "c", &[]));
        let (result, statuses, _) = run(input);
        result.unwrap();
        assert_eq!(statuses, [0, 1, 0, 0, 1]);
    }

    #[test]
    fn disconnect_drops_names() {
        let registry = Registry::default();
        let mut input = handshake_bytes(InlineTransport::MAGIC).to_vec();
        input.extend(request(wire::OPCODE_REGISTER, "a", &7u32.to_le_bytes()));
        let mut transport = InlineTransport {
            input: input.into(),
            output: Vec::new(),
        };
        futures::executor::block_on(serve(&mut transport, &registry)).unwrap();
        registry.resolve("a").unwrap_err();
    }

    #[test]
    fn protocol_violations_close_the_connection() {
        // Bad magic: no handshake reply.
        let (result, statuses, _) = run(handshake_bytes([0xFD, b'F', b'D', 0x01]).to_vec());
        result.unwrap_err();
        assert!(statuses.is_empty());

        // Unknown opcode.
        let mut input = handshake_bytes(InlineTransport::MAGIC).to_vec();
        input.extend(request(99, "a", &[]));
        run(input).0.unwrap_err();

        // EOF in the middle of a request.
        let mut input = handshake_bytes(InlineTransport::MAGIC).to_vec();
        input.extend(&request(wire::OPCODE_REGISTER, "a", &7u32.to_le_bytes())[..4]);
        run(input).0.unwrap_err();
    }
}
