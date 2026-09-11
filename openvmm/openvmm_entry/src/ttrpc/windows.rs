// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows RPC transport setup and named-pipe handling.

use super::ResolvedTransport;
use super::bind_unix_listener;
use super::dispatch;
use anyhow::Context;
use futures::AsyncRead;
use futures::AsyncReadExt;
use futures::AsyncWrite;
use futures::FutureExt;
use pal::windows::security::LocalSid;
use pal::windows::security::current_process_user_sid;
use pal_async::driver::Driver;
use pal_async::pipe::PolledPipe;
use pal_async::socket::PolledSocket;
use pal_async::windows::pipe::NamedPipeServer;
use unicycle::FuturesUnordered;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use windows_sys::Win32::Storage::FileSystem::FILE_APPEND_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

/// Placeholder for shared NIC configuration; Windows has no fd-passing protocol.
#[derive(Clone, Default)]
pub(super) struct FdRegistry {}

#[derive(mesh::MeshPayload)]
pub enum Listener {
    Unix(UnixListener),
    Pipe {
        path: String,
        allow_sid: Option<String>,
    },
}

pub fn listener_from_path(
    path: &std::path::Path,
    allow_sid: Option<&str>,
) -> anyhow::Result<Listener> {
    match mesh_rpc::is_named_pipe_path(path) {
        true => Ok(Listener::Pipe {
            path: path.to_string_lossy().into_owned(),
            allow_sid: allow_sid.map(str::to_owned),
        }),
        false => Ok(Listener::Unix(bind_unix_listener(path)?)),
    }
}

pub(super) enum BoundListener {
    Unix(UnixListener),
    Pipe(NamedPipeServer),
}

impl BoundListener {
    pub(super) fn new(listener: Listener) -> anyhow::Result<Self> {
        Ok(match listener {
            Listener::Unix(listener) => Self::Unix(listener),
            Listener::Pipe { path, allow_sid } => {
                let security_descriptor = allow_sid
                    .as_deref()
                    .map(rpc_pipe_security_descriptor)
                    .transpose()?;
                Self::Pipe(
                    NamedPipeServer::create_with_security(path, security_descriptor.as_deref())
                        .context("failed to create named pipe")?,
                )
            }
        })
    }

    pub(super) async fn run(
        self,
        server: &mesh_rpc::Server,
        driver: &(impl Driver + ?Sized),
        cancel: mesh::OneshotReceiver<()>,
        transport: ResolvedTransport,
        registry: FdRegistry,
    ) -> anyhow::Result<()> {
        match self {
            Self::Unix(listener) => {
                dispatch::run(server, driver, listener, cancel, transport, registry).await
            }
            Self::Pipe(listener) => run_pipe(server, driver, listener, cancel, transport).await,
        }
    }
}

pub(super) async fn serve_socket(
    server: &mesh_rpc::Server,
    conn: PolledSocket<UnixStream>,
    first_byte: u8,
    transport: ResolvedTransport,
    _registry: &FdRegistry,
) -> anyhow::Result<()> {
    dispatch::serve_by_protocol(
        server,
        conn,
        first_byte,
        transport,
        mesh_rpc::server::ShutdownPolicy::Close,
    )
    .await
}

async fn run_pipe(
    server: &mesh_rpc::Server,
    driver: &(impl Driver + ?Sized),
    listener: NamedPipeServer,
    cancel: mesh::OneshotReceiver<()>,
    transport: ResolvedTransport,
) -> anyhow::Result<()> {
    let mut tasks = FuturesUnordered::new();
    let mut cancel = cancel.fuse();
    let mut accept = std::pin::pin!(listener.accept(driver)?.fuse());
    loop {
        futures::select! { // merge semantics
            result = accept => {
                accept.set(listener.accept(driver)?.fuse());
                if let Ok(conn) = result.and_then(|conn| PolledPipe::new(driver, conn)) {
                    tasks.push(async move {
                        let _ = serve_pipe(server, conn, transport)
                            .await
                            .map_err(|err| {
                                tracing::error!(
                                    error = err.as_ref() as &dyn std::error::Error,
                                    "connection error"
                                )
                            });
                    });
                }
            }
            _ = tasks.next() => continue,
            _ = cancel => break,
        }
    }
    Ok(())
}

async fn serve_pipe(
    server: &mesh_rpc::Server,
    mut conn: PolledPipe,
    transport: ResolvedTransport,
) -> anyhow::Result<()> {
    let mut first_byte = [0];
    if conn.read(&mut first_byte).await? == 0 {
        return Ok(());
    }
    let conn = PrefixedStream {
        prefix: Some(first_byte[0]),
        inner: conn,
    };
    dispatch::serve_by_protocol(
        server,
        conn,
        first_byte[0],
        transport,
        mesh_rpc::server::ShutdownPolicy::NoHalfClose,
    )
    .await
}

struct PrefixedStream<T> {
    prefix: Option<u8>,
    inner: T,
}

impl<T: AsyncRead + Unpin> AsyncRead for PrefixedStream<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return std::task::Poll::Ready(Ok(0));
        }
        if let Some(prefix) = self.prefix.take() {
            buf[0] = prefix;
            return std::task::Poll::Ready(Ok(1));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_close(cx)
    }
}

fn rpc_pipe_security_descriptor(
    allow_sid: &str,
) -> anyhow::Result<pal::windows::security::LocalSecurityDescriptor> {
    anyhow::ensure!(
        !allow_sid.contains('\0'),
        "invalid RPC 'allow-sid': contains NUL"
    );
    let client: LocalSid = allow_sid.parse().context("invalid RPC 'allow-sid'")?;
    let server = current_process_user_sid().context("failed to query RPC pipe owner")?;
    let client_access = (FILE_GENERIC_READ | FILE_GENERIC_WRITE) & !FILE_APPEND_DATA;
    format!(
        "O:{server}D:P(A;;FA;;;{server})(A;;0x{client_access:x};;;{client})",
        server = server.to_string_sid(),
        client = client.to_string_sid(),
    )
    .parse()
    .context("failed to build RPC pipe security descriptor")
}

#[cfg(test)]
mod tests {
    use super::super::Parameters;
    use super::super::RpcTransport;
    use super::super::TtrpcWorker;
    use super::*;
    use mesh_worker::Worker;
    use pal::windows::security::LocalSecurityDescriptor;
    use pal::windows::security::current_process_user_sid;
    use pal_async::DefaultPool;
    use std::fs::File;
    use std::fs::OpenOptions;
    use std::os::windows::io::AsHandle;
    use test_with_tracing::test;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;

    fn pipe_path() -> String {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        format!(
            r"\\.\pipe\openvmm-rpc-test-{:032x}",
            u128::from_ne_bytes(random)
        )
    }

    fn assert_pipe_security(file: &File, expected: &str) {
        let descriptor = LocalSecurityDescriptor::from_handle(
            file.as_handle(),
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
        )
        .unwrap();
        assert_eq!(descriptor.to_sddl().unwrap(), expected);
    }

    #[test]
    fn reject_invalid_pipe_sid() {
        for sid in [
            "",
            "not-a-sid",
            "S-1-5-19)(A;;FA;;;WD)",
            "S-1-5-19\0",
            "S-1-5-19\0\0",
            "S-1-5-19\0trailing",
        ] {
            assert!(
                rpc_pipe_security_descriptor(sid).is_err(),
                "accepted invalid SID: {sid:?}"
            );
        }
    }

    #[test]
    fn worker_pipe_authorizes_configured_sid() {
        DefaultPool::run_with(async |driver| {
            let path = pipe_path();
            let worker = TtrpcWorker::new(Parameters {
                listener: listener_from_path(path.as_ref(), Some("S-1-5-19")).unwrap(),
                transport: RpcTransport::Auto,
            })
            .unwrap();
            let BoundListener::Pipe(listener) = &worker.listener else {
                panic!("expected named pipe listener");
            };
            let user = current_process_user_sid().unwrap();
            let expected: LocalSecurityDescriptor = format!(
                "O:{user}D:P(A;;FA;;;{user})(A;;0x12019b;;;S-1-5-19)",
                user = user.to_string_sid()
            )
            .parse()
            .unwrap();
            let expected = expected.to_sddl().unwrap();
            for _ in 0..2 {
                let accept = listener.accept(&driver).unwrap();
                let client = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                let server = accept.await.unwrap();
                assert_pipe_security(&client, &expected);
                assert_pipe_security(&server, &expected);
            }
        });
    }

    #[test]
    fn worker_construction_reserves_pipe() {
        let path = pipe_path();
        let parameters = || Parameters {
            listener: listener_from_path(path.as_ref(), None).unwrap(),
            transport: RpcTransport::Auto,
        };

        let worker = TtrpcWorker::new(parameters()).unwrap();
        assert!(NamedPipeServer::create(&path).is_err());
        assert!(TtrpcWorker::new(parameters()).is_err());

        drop(worker);
        NamedPipeServer::create(&path).unwrap();
    }
}
