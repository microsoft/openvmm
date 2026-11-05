// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Windows transport of the resource-passing protocol: handle-passing.
//!
//! Windows `AF_UNIX` sockets cannot carry handles, so the client duplicates
//! each handle into this process with `DuplicateHandle` and appends its value
//! to the `Register` request. This is the only place OpenVMM interprets a
//! client-supplied handle value; the server makes its own duplicate and never
//! closes the client's. Everything else is shared in `resource_passing`.

#![cfg(windows)]

use super::resource_passing::Resource;
use super::resource_passing::Transport;
use anyhow::Context as _;
use pal_async::socket::PolledSocket;
use std::io;
use std::os::windows::io::OwnedHandle;
use std::os::windows::io::RawHandle;
use unix_socket::UnixStream;

/// The wire format of the handle-passing transport. See
/// `openvmm_ttrpc_vmservice/src/resource_passing.md`.
mod protocol {
    /// The 4-byte handshake magic: `0xFD 'H' 'P' 0x01`. Encodes both protocol
    /// and revision.
    pub(super) const HANDSHAKE_MAGIC: [u8; 4] = [0xFD, b'H', b'P', 0x01];

    /// The length of the handle value that follows the name in a `Register`
    /// request: a little-endian `u64`.
    pub(super) const HANDLE_LEN: usize = 8;
}

impl Resource for OwnedHandle {
    const KIND: &'static str = "handle";

    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }
}

/// A handle-passing connection.
pub(super) struct Connection(PolledSocket<UnixStream>);

impl Connection {
    pub(super) fn new(conn: PolledSocket<UnixStream>) -> Self {
        Self(conn)
    }
}

impl Transport for Connection {
    type Resource = OwnedHandle;
    const MAGIC: [u8; 4] = protocol::HANDSHAKE_MAGIC;
    const INLINE_RESOURCE_LEN: usize = protocol::HANDLE_LEN;

    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use futures::AsyncReadExt;
        self.0.read(buf).await
    }

    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        use futures::AsyncWriteExt;
        self.0.write_all(buf).await
    }

    fn decode_inline(data: &[u8]) -> anyhow::Result<Option<OwnedHandle>> {
        let value = u64::from_le_bytes(data.try_into().context("truncated handle value")?);
        // Reject null and the negative pseudo-handles (such as the current
        // process or token), which would otherwise name this process's own
        // objects.
        anyhow::ensure!(
            value != 0 && i64::try_from(value).is_ok(),
            "invalid handle value {value:#x}"
        );
        let handle = usize::try_from(value).context("invalid handle value")? as RawHandle;
        let handle = pal::windows::duplicate_raw_handle(handle)
            .with_context(|| format!("failed to duplicate handle {value:#x}"))?;
        Ok(Some(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ttrpc::ResourceRegistry;
    use crate::ttrpc::resource_passing;
    use crate::ttrpc::resource_passing::wire;
    use pal_async::DefaultPool;
    use pal_async::task::Spawn;
    use std::io::Read as _;
    use std::io::Write as _;
    use std::os::windows::io::AsRawHandle;
    use test_with_tracing::test;

    /// A blocking client, as an external process would implement.
    struct Client(UnixStream);

    impl Client {
        fn handshake(&mut self) {
            let mut handshake = [0; wire::HANDSHAKE_LEN];
            handshake[..4].copy_from_slice(&protocol::HANDSHAKE_MAGIC);
            self.0.write_all(&handshake).unwrap();
            let mut reply = [0; wire::HANDSHAKE_LEN];
            self.0.read_exact(&mut reply).unwrap();
            assert_eq!(reply[..4], protocol::HANDSHAKE_MAGIC);
        }

        fn request(&mut self, opcode: u8, name: &str, handle: Option<u64>) -> u8 {
            let mut frame = vec![opcode, name.len() as u8];
            frame.extend_from_slice(name.as_bytes());
            if let Some(handle) = handle {
                frame.extend_from_slice(&handle.to_le_bytes());
            }
            self.0.write_all(&frame).unwrap();
            let mut header = [0; 3];
            self.0.read_exact(&mut header).unwrap();
            let mut msg = vec![0; u16::from_le_bytes([header[1], header[2]]).into()];
            self.0.read_exact(&mut msg).unwrap();
            header[0]
        }
    }

    fn process_token() -> OwnedHandle {
        use windows_sys::Win32::Security::TOKEN_DUPLICATE;
        use windows_sys::Win32::Security::TOKEN_QUERY;
        pal::windows::security::open_process_token(TOKEN_DUPLICATE | TOKEN_QUERY).unwrap()
    }

    #[test]
    fn end_to_end() {
        let registry = ResourceRegistry::default();
        let (server, client) = UnixStream::pair().unwrap();
        let (_thread, driver) = DefaultPool::spawn_on_thread("handle-passing-test");
        let server = PolledSocket::new(&driver, server).unwrap();
        let task = driver.spawn("serve", {
            let registry = registry.clone();
            async move { resource_passing::serve(&mut Connection::new(server), &registry).await }
        });

        let mut client = Client(client);
        client.handshake();

        // The client and server are the same process here, so the token's own
        // value stands in for one duplicated into the server.
        let token = process_token();
        let value = token.as_raw_handle() as usize as u64;

        assert_eq!(
            client.request(wire::OPCODE_REGISTER, "t0", Some(value)),
            wire::STATUS_OK
        );
        // The server made its own duplicate and did not take the client's.
        drop(registry.resolve("t0").unwrap());
        token.try_clone().unwrap();

        // Null and pseudo-handle values are rejected.
        for bad in [0, u64::MAX, u64::MAX - 3] {
            assert_ne!(
                client.request(wire::OPCODE_REGISTER, "bad", Some(bad)),
                wire::STATUS_OK
            );
        }
        registry.resolve("bad").unwrap_err();

        assert_eq!(
            client.request(wire::OPCODE_DEREGISTER, "t0", None),
            wire::STATUS_OK
        );
        registry.resolve("t0").unwrap_err();

        // Closing the connection drops its names.
        assert_eq!(
            client.request(wire::OPCODE_REGISTER, "t1", Some(value)),
            wire::STATUS_OK
        );
        drop(client);
        futures::executor::block_on(task).unwrap();
        registry.resolve("t1").unwrap_err();
    }
}
