// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TTRPC server.

use crate::message::read_message;
use crate::message::write_message;
use crate::message::ReadResult;
use crate::message::Request;
use crate::message::Response;
use crate::message::TooLongError;
use crate::message::MESSAGE_TYPE_REQUEST;
use crate::message::MESSAGE_TYPE_RESPONSE;
use crate::rpc::status_from_err;
use crate::rpc::ProtocolError;
use crate::service::Code;
use crate::service::GenericRpc;
use crate::service::ServiceRpc;
use crate::service::Status;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::future::TryJoin;
use futures_concurrency::stream::Merge;
use mesh::payload::Downcast;
use mesh::CancelContext;
use pal_async::driver::Driver;
use pal_async::socket::AsSockRef;
use pal_async::socket::Listener;
use pal_async::socket::PolledSocket;
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use unicycle::FuturesUnordered;

/// A ttrpc server.
#[derive(Debug, Default)]
pub struct Server {
    services: HashMap<&'static str, mesh::Sender<(CancelContext, GenericRpc)>>,
}

impl Server {
    /// Creates a new ttrpc server.
    pub fn new() -> Self {
        Self {
            services: Default::default(),
        }
    }

    /// Adds or updates a channel for receiving service requests.
    pub fn add_service<T: ServiceRpc>(&mut self, send: mesh::Sender<(CancelContext, T)>)
    where
        GenericRpc: Downcast<T>,
    {
        self.services.insert(T::NAME, send.force_downcast());
    }

    /// Runs the server using the ttrpc transport, listening on `listener` and
    /// servicing connections until `cancel`.
    pub async fn run(
        &mut self,
        driver: &(impl Driver + ?Sized),
        listener: impl Listener,
        cancel: mesh::OneshotReceiver<()>,
    ) -> anyhow::Result<()> {
        let mut listener = PolledSocket::new(driver, listener)?;
        let mut tasks = FuturesUnordered::new();
        let mut cancel = cancel.fuse();
        loop {
            let conn = futures::select! { // merge semantics
                r = listener.accept().fuse() => r,
                _ = tasks.next() => continue,
                _ = cancel => break,
            };
            if let Ok(conn) = conn.and_then(|(conn, _)| PolledSocket::new(driver, conn)) {
                tasks.push(async {
                    let _ = self.serve(conn).await.map_err(|err| {
                        tracing::error!(
                            error = err.as_ref() as &dyn std::error::Error,
                            "connection error"
                        )
                    });
                });
            }
        }
        Ok(())
    }

    /// Runs the server, servicing a single connection `conn`.
    pub async fn run_single(
        &mut self,
        driver: &(impl Driver + ?Sized),
        conn: impl AsSockRef + Read + Write,
    ) -> anyhow::Result<()> {
        self.serve(PolledSocket::new(driver, conn)?).await
    }

    async fn serve(
        &self,
        stream: PolledSocket<impl AsSockRef + Read + Write>,
    ) -> anyhow::Result<()> {
        let (mut reader, mut writer) = stream.split();
        let (stream_send, mut stream_recv) = mesh::channel();
        let ctx = CancelContext::new();
        let recv_task = async {
            let stream_send = stream_send; // move into this task
            while let Some(message) = read_message(&mut reader).await? {
                let (send, recv) = mesh::oneshot::<Result<Vec<u8>, Status>>();
                stream_send.send((message.stream_id, recv));

                let handle = handle_message(message).and_then(|request| {
                    let service = self.services.get(request.service.as_str()).ok_or_else(|| {
                        status_from_err(
                            Code::Unimplemented,
                            anyhow::anyhow!("unknown service {}", request.service),
                        )
                    })?;

                    let ctx = if request.timeout_nano == 0 {
                        ctx.clone()
                    } else {
                        ctx.with_timeout(std::time::Duration::from_nanos(request.timeout_nano))
                    };

                    Ok(move |port| {
                        service.send((
                            ctx,
                            GenericRpc {
                                method: request.method,
                                data: request.payload,
                                port,
                            },
                        ));
                    })
                });

                match handle {
                    Ok(handle) => handle(send.into()),
                    Err(err) => send.send(Err(err)),
                }
            }
            Ok(())
        };
        let send_task = async {
            let mut responses = FuturesUnordered::new();
            enum Event<T> {
                Request((u32, mesh::OneshotReceiver<Result<Vec<u8>, Status>>)),
                Response(T),
            }
            while let Some(event) = (
                (&mut stream_recv).map(Event::Request),
                (&mut responses).map(Event::Response),
            )
                .merge()
                .next()
                .await
            {
                match event {
                    Event::Request((stream_id, recv)) => {
                        let recv = recv.map(move |r| {
                            (
                                stream_id,
                                match r {
                                    Ok(Ok(payload)) => Response::Payload(payload),
                                    Ok(Err(status)) => Response::Status(status),
                                    Err(_) => Response::Status(Status {
                                        code: Code::Internal.into(),
                                        message: "unknown error".to_string(),
                                        details: Vec::new(),
                                    }),
                                },
                            )
                        });
                        responses.push(recv);
                    }
                    Event::Response((stream_id, payload)) => {
                        write_message(
                            &mut writer,
                            stream_id,
                            MESSAGE_TYPE_RESPONSE,
                            &mesh::payload::encode(payload),
                        )
                        .await?;
                    }
                }
            }
            anyhow::Result::<_>::Ok(())
        };
        (recv_task, send_task).try_join().await?;
        Ok(())
    }
}

fn handle_message(message: ReadResult) -> Result<Request, Status> {
    if message.stream_id % 2 != 1 {
        return Err(status_from_err(
            Code::InvalidArgument,
            ProtocolError::EvenStreamId,
        ));
    }

    match message.message_type {
        MESSAGE_TYPE_REQUEST => {
            let payload = message.payload.map_err(|err @ TooLongError { .. }| {
                status_from_err(Code::ResourceExhausted, err)
            })?;
            let request = mesh::payload::decode::<Request>(&payload)
                .map_err(|err| status_from_err(Code::InvalidArgument, err))?;

            tracing::debug!(
                stream_id = message.stream_id,
                service = %request.service,
                method = %request.method,
                timeout = request.timeout_nano / 1000 / 1000,
                "message",
            );

            Ok(request)
        }
        ty => Err(status_from_err(
            Code::InvalidArgument,
            ProtocolError::InvalidMessageType(ty),
        )),
    }
}

#[cfg(feature = "grpc")]
mod grpc {
    use super::Server;
    use crate::service::Code;
    use crate::service::GenericRpc;
    use crate::service::Status;
    use anyhow::Context as _;
    use futures::AsyncRead as _;
    use futures::AsyncWrite;
    use futures::FutureExt;
    use futures::StreamExt;
    use futures_concurrency::stream::Merge;
    use h2::server::SendResponse;
    use h2::RecvStream;
    use http::HeaderMap;
    use http::HeaderValue;
    use mesh::CancelContext;
    use pal_async::driver::Driver;
    use pal_async::socket::AsSockRef;
    use pal_async::socket::Listener;
    use pal_async::socket::PolledSocket;
    use prost::bytes::Bytes;
    use std::io::Read;
    use std::io::Write;
    use std::pin::Pin;
    use std::task::ready;
    use thiserror::Error;
    use unicycle::FuturesUnordered;

    #[derive(Debug, Error)]
    enum RequestError {
        #[error("http error")]
        Http(#[from] http::Error),
        #[error("http2 error")]
        H2(#[from] h2::Error),
        #[error("unreachable")]
        Status(http::StatusCode),
        #[error("invalid message header")]
        InvalidHeader,
    }

    impl From<http::StatusCode> for RequestError {
        fn from(status: http::StatusCode) -> Self {
            RequestError::Status(status)
        }
    }

    impl Server {
        /// Runs the server using the gRPC transport, listening on `listener` and servicing connections until
        /// `cancel`.
        pub async fn run_grpc(
            &mut self,
            driver: &(impl Driver + ?Sized),
            listener: impl Listener,
            cancel: mesh::OneshotReceiver<()>,
        ) -> anyhow::Result<()> {
            let mut listener = PolledSocket::new(driver, listener)?;
            let mut tasks = FuturesUnordered::new();
            let mut cancel = cancel.fuse();
            loop {
                let conn = futures::select! { // merge semantics
                    r = listener.accept().fuse() => r,
                    _ = tasks.next() => continue,
                    _ = cancel => break,
                };
                if let Ok(conn) = conn.and_then(|(conn, _)| PolledSocket::new(driver, conn)) {
                    tasks.push(async {
                        let _ = self.serve_grpc(conn).await.map_err(|err| {
                            tracing::error!(
                                error = err.as_ref() as &dyn std::error::Error,
                                "connection error"
                            )
                        });
                    });
                }
            }
            Ok(())
        }

        async fn serve_grpc(
            &self,
            stream: PolledSocket<impl AsSockRef + Read + Write>,
        ) -> anyhow::Result<()> {
            struct Wrap<T>(T);

            impl<T: AsSockRef + Read> tokio::io::AsyncRead for Wrap<PolledSocket<T>> {
                fn poll_read(
                    self: Pin<&mut Self>,
                    cx: &mut std::task::Context<'_>,
                    buf: &mut tokio::io::ReadBuf<'_>,
                ) -> std::task::Poll<std::io::Result<()>> {
                    let n =
                        ready!(Pin::new(&mut self.get_mut().0)
                            .poll_read(cx, buf.initialize_unfilled()))?;
                    buf.advance(n);
                    std::task::Poll::Ready(Ok(()))
                }
            }

            impl<T: AsSockRef + Write> tokio::io::AsyncWrite for Wrap<PolledSocket<T>> {
                fn poll_write(
                    self: Pin<&mut Self>,
                    cx: &mut std::task::Context<'_>,
                    buf: &[u8],
                ) -> std::task::Poll<Result<usize, std::io::Error>> {
                    Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
                }

                fn poll_flush(
                    self: Pin<&mut Self>,
                    cx: &mut std::task::Context<'_>,
                ) -> std::task::Poll<Result<(), std::io::Error>> {
                    Pin::new(&mut self.get_mut().0).poll_flush(cx)
                }

                fn poll_shutdown(
                    self: Pin<&mut Self>,
                    cx: &mut std::task::Context<'_>,
                ) -> std::task::Poll<Result<(), std::io::Error>> {
                    Pin::new(&mut self.get_mut().0).poll_close(cx)
                }
            }

            let mut conn = h2::server::handshake(Wrap(stream))
                .await
                .context("failed http2 handshake")?;

            let mut tasks = FuturesUnordered::new();

            loop {
                enum Event<A, B> {
                    Accept(A),
                    Task(Result<(), B>),
                }

                let r = (
                    (&mut conn).map(Event::Accept),
                    (&mut tasks).map(Event::Task),
                )
                    .merge()
                    .next()
                    .await;

                let (req, mut resp) = match r {
                    None => break,
                    Some(Event::Task(r)) => {
                        r?;
                        continue;
                    }
                    Some(Event::Accept(r)) => r.context("failed http2 stream accept")?,
                };

                let task = async move {
                    match self.handle_request(req, &mut resp).await {
                        Err(RequestError::Status(status)) => {
                            tracing::debug!(status = status.as_u16(), "request error");
                            resp.send_response(
                                http::Response::builder().status(status).body(())?,
                                true,
                            )?;
                            Ok(())
                        }
                        r => r,
                    }
                };
                tasks.push(task);
            }

            std::future::poll_fn(|cx| conn.poll_closed(cx)).await?;
            Ok(())
        }

        async fn handle_request(
            &self,
            req: http::Request<RecvStream>,
            resp: &mut SendResponse<Bytes>,
        ) -> Result<(), RequestError> {
            tracing::debug!(url = %req.uri(), "rpc request");

            if req.method() != http::Method::POST {
                Err(http::StatusCode::METHOD_NOT_ALLOWED)?
            }
            let content_type = req.headers().get("content-type");
            match content_type.map(|v| v.as_bytes()) {
                Some(b"application/grpc" | b"application/grpc+proto") => {}
                _ => Err(http::StatusCode::UNSUPPORTED_MEDIA_TYPE)?,
            }

            let response =
                http::Response::builder().header("content-type", "application/grpc+proto");

            let ctx = if let Some(timeout) = req.headers().get("grpc-timeout") {
                let timeout = timeout
                    .to_str()
                    .map_err(|_| http::StatusCode::BAD_REQUEST)?;
                let mul = match timeout
                    .bytes()
                    .last()
                    .ok_or(http::StatusCode::BAD_REQUEST)?
                {
                    b'H' => std::time::Duration::from_secs(60 * 60),
                    b'M' => std::time::Duration::from_secs(60),
                    b'S' => std::time::Duration::from_secs(1),
                    b'm' => std::time::Duration::from_millis(1),
                    b'u' => std::time::Duration::from_micros(1),
                    b'n' => std::time::Duration::from_nanos(1),
                    _ => Err(http::StatusCode::BAD_REQUEST)?,
                };
                let timeout = timeout[..timeout.len() - 1]
                    .parse::<u32>()
                    .map_err(|_| http::StatusCode::BAD_REQUEST)?;
                CancelContext::new().with_timeout(mul * timeout)
            } else {
                CancelContext::new()
            };

            let (head, body) = req.into_parts();
            let path = head.uri.path();
            let path = path.strip_prefix('/').ok_or(http::StatusCode::NOT_FOUND)?;
            let (service, method) = path.split_once('/').ok_or(http::StatusCode::NOT_FOUND)?;

            // No returning HTTP status code errors after this point.
            let mut resp = resp.send_response(response.body(())?, false)?;

            let result = self.invoke_rpc(service, method, body, ctx).await?;

            let mut trailers = HeaderMap::new();
            match result {
                Ok(data) => {
                    tracing::debug!(service, method, "rpc success");

                    let mut buf = Vec::with_capacity(5 + data.len());
                    buf.push(0);
                    buf.extend(&(data.len() as u32).to_be_bytes());
                    buf.extend(data);
                    resp.send_data(buf.into(), false)?;
                    trailers.insert("grpc-status", const { HeaderValue::from_static("0") });
                }
                Err(status) => {
                    tracing::debug!(service, method, ?status, "rpc error");

                    trailers.insert("grpc-status", status.code.into());
                    trailers.insert(
                        "grpc-message",
                        urlencoding::encode(&status.message)
                            .into_owned()
                            .try_into()
                            .unwrap(),
                    );
                    trailers.insert(
                        "grpc-status-details-bin",
                        base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            prost::Message::encode_to_vec(&status),
                        )
                        .try_into()
                        .unwrap(),
                    );
                }
            }
            resp.send_trailers(trailers)?;
            Ok(())
        }

        async fn invoke_rpc(
            &self,
            service: &str,
            method: &str,
            mut body: RecvStream,
            ctx: CancelContext,
        ) -> Result<Result<Vec<u8>, Status>, RequestError> {
            let Some(service) = self.services.get(service) else {
                return Ok(Err(Status {
                    code: Code::Unimplemented.into(),
                    message: format!("unknown service {}", service),
                    details: Vec::new(),
                }));
            };

            // For now, only non-stream RPCs are supported, so read the first
            // message and ignore the rest.
            //
            // FUTURE: change the `GenericRpc` type to include channels for
            // streams.

            let mut buf = Vec::new();

            // Read data frames until the header is complete.
            while buf.len() < 5 {
                let data = body.data().await.ok_or(RequestError::InvalidHeader)??;
                buf.extend(&data);
                body.flow_control().release_capacity(data.len()).unwrap();
            }
            let hdr = buf.get(0..5).ok_or(RequestError::InvalidHeader)?;
            if hdr[0] != 0 {
                // Compression was not advertised as supported, so the client
                // should not send compressed messages.
                return Err(RequestError::InvalidHeader);
            }
            let len = u32::from_be_bytes(hdr[1..5].try_into().unwrap()) as usize;

            buf.drain(..5);
            while buf.len() < len {
                let data = body.data().await.ok_or(RequestError::InvalidHeader)??;
                buf.extend(&data);
                body.flow_control().release_capacity(data.len()).unwrap();
            }

            let (send, recv) = mesh::oneshot();

            let rpc = GenericRpc {
                method: method.to_owned(),
                data: buf,
                port: send.into(),
            };

            service.send((ctx, rpc));

            Ok(recv.await.unwrap_or_else(|_| {
                Err(Status {
                    code: Code::Internal.into(),
                    message: "unknown error".to_string(),
                    details: Vec::new(),
                })
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Client;
    use crate::Server;
    use futures::executor::block_on;
    use pal_async::local::block_with_io;
    use pal_async::DefaultPool;
    use test_with_tracing::test;

    mod items {
        include!(concat!(env!("OUT_DIR"), "/ttrpc.example.v1.rs"));
    }

    #[test]
    fn client_server() {
        let (c, s) = unix_socket::UnixStream::pair().unwrap();
        let mut server = Server::new();
        let (send, mut recv) = mesh::channel();
        server.add_service::<items::Example>(send);
        let server_thread = std::thread::spawn(move || {
            block_with_io(|driver| async move { server.run_single(&driver, s).await })
        });

        let client_thread = std::thread::spawn(move || {
            DefaultPool::run_with(|driver| async move {
                let client = Client::new(&driver, c);
                let response = client
                    .call(
                        items::Example::Method1,
                        items::Method1Request {
                            foo: "abc".to_string(),
                            bar: "def".to_string(),
                        },
                    )
                    .await
                    .unwrap()
                    .unwrap();

                assert_eq!(&response.foo, "abc123");
                assert_eq!(&response.bar, "def456");
                client.shutdown().await;
            })
        });

        block_on(async {
            let (_, req) = recv.recv().await.unwrap();
            match req {
                items::Example::Method1(input, resp) => {
                    assert_eq!(&input.foo, "abc");
                    assert_eq!(&input.bar, "def");
                    resp.send(Ok(items::Method1Response {
                        foo: input.foo + "123",
                        bar: input.bar + "456",
                    }));
                }
                _ => panic!("{:?}", &req),
            }

            recv.recv().await.unwrap_err();
        });

        client_thread.join().unwrap();
        server_thread.join().unwrap().unwrap();
    }
}
