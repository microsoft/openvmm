// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TTRPC server.

use crate::message::MESSAGE_TYPE_REQUEST;
use crate::message::MESSAGE_TYPE_RESPONSE;
use crate::message::ReadResult;
use crate::message::Request;
use crate::message::Response;
use crate::message::TooLongError;
use crate::message::read_message;
use crate::message::write_message;
use crate::rpc::ProtocolError;
use crate::rpc::status_from_err;
use crate::service::Code;
use crate::service::DecodedRpc;
use crate::service::GenericRpc;
use crate::service::ServiceRpc;
use crate::service::ServiceRpcError;
use crate::service::Status;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use futures::future::Either;
use futures::future::select;
use futures::stream::FusedStream;
use futures_concurrency::stream::Merge;
use mesh::CancelContext;
use mesh::MeshPayload;
use mesh::local_node::Port;
use pal_async::driver::Driver;
use pal_async::interest::PollEvents;
use pal_async::socket::AsSockRef;
use pal_async::socket::Listener;
use pal_async::socket::PollReadyExt;
use pal_async::socket::PolledSocket;
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::pin::Pin;
use std::task::ready;
use unicycle::FuturesUnordered;

/// A ttrpc server.
#[derive(Debug, Default)]
pub struct Server {
    services: HashMap<&'static str, mesh::Sender<(CancelContext, GenericRpc)>>,
}

/// A receiver for RPC requests for a given service.
///
/// Returned by [`Server::add_service`].
#[derive(MeshPayload)]
#[mesh(bound = "T: ServiceRpc")]
pub struct RpcReceiver<T>(mesh::Receiver<(CancelContext, DecodedRpc<T>)>);

impl<T: ServiceRpc> RpcReceiver<T> {
    /// Returns a disconnected stream, useful for when a service is dynamically
    /// not registered.
    pub fn disconnected() -> Self {
        let (_send, recv) = mesh::channel();
        Self(recv)
    }
}

impl<T: ServiceRpc> Stream for RpcReceiver<T> {
    type Item = (CancelContext, T);

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        while let Some((ctx, rpc)) = ready!(Pin::new(&mut this.0).poll_next(cx)) {
            match rpc {
                DecodedRpc::Rpc(rpc) => return Some((ctx, rpc)).into(),
                DecodedRpc::Err { rpc, err } => {
                    rpc.fail(err);
                }
            }
        }
        None.into()
    }
}

impl<T: ServiceRpc> FusedStream for RpcReceiver<T> {
    fn is_terminated(&self) -> bool {
        self.0.is_terminated()
    }
}

impl GenericRpc {
    fn fail(self, err: ServiceRpcError) {
        let status = match err {
            ServiceRpcError::UnknownMethod => Status {
                code: Code::Unimplemented.into(),
                message: format!("unknown method {}", self.method),
                details: Vec::new(),
            },
            ServiceRpcError::InvalidInput(error) => status_from_err(Code::InvalidArgument, error),
        };
        self.respond_status(status);
    }
}

impl Server {
    /// Creates a new ttrpc server.
    pub fn new() -> Self {
        Self {
            services: Default::default(),
        }
    }

    /// Adds or updates a channel for receiving service requests.
    pub fn add_service<T: ServiceRpc>(&mut self) -> RpcReceiver<T> {
        let (send, recv) = mesh::channel();
        self.services.insert(T::NAME, Port::from(send).into());
        RpcReceiver(recv)
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
                    let _ = self.serve_connection(conn).await.map_err(|err| {
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
        self.serve_connection(PolledSocket::new(driver, conn)?)
            .await
    }

    /// Services a single, already-accepted connection using the ttrpc protocol.
    ///
    /// This is useful when the caller owns the accept loop, for example to
    /// dispatch a connection to ttrpc or another protocol based on a sniffed
    /// prefix byte.
    ///
    /// A read EOF allows accepted responses to finish. Pending request contexts
    /// are cancelled by request deadlines, socket hangup, I/O failure or dropping
    /// the connection task. Dropping an individual client call is not a wire
    /// cancellation signal in unary ttrpc.
    ///
    /// TCP and graceful Windows disconnects may report only read EOF, which
    /// cannot reliably distinguish a full disconnect from a write half close.
    /// Full disconnect cancellation before a response is not guaranteed in these
    /// cases. Clients must send deadlines for bounded requests and services must
    /// stop work when the request context is cancelled.
    pub async fn serve_connection(
        &self,
        stream: PolledSocket<impl AsSockRef + Read + Write>,
    ) -> anyhow::Result<()> {
        let (mut reader, mut writer) = stream.split();
        let (stream_send, mut stream_recv) = mesh::channel();
        let (ctx, _cancel) = CancelContext::new().with_cancel();
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
            anyhow::Result::<_>::Ok(())
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
                                    Err(err) => {
                                        Response::Status(status_from_err(Code::Internal, err))
                                    }
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
        let mut send_task = std::pin::pin!(send_task);
        {
            let recv_task = std::pin::pin!(recv_task);
            match select(recv_task, send_task.as_mut()).await {
                Either::Left((result, _)) => result?,
                Either::Right((result, _)) => return result,
            }
        }
        // Read EOF can be a write half close, so keep draining accepted replies.
        match select(
            send_task,
            reader.wait_ready(PollEvents::HUP | PollEvents::ERR),
        )
        .await
        {
            Either::Left((result, _)) => result,
            Either::Right((events, _)) if events.has_err() => Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "socket error while draining ttrpc replies",
            )
            .into()),
            Either::Right(_) => Ok(()),
        }
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
    use crate::rpc::status_from_err;
    use crate::service::Code;
    use crate::service::GenericRpc;
    use crate::service::Status;
    use anyhow::Context as _;
    use futures::AsyncRead as _;
    use futures::AsyncWrite;
    use futures::FutureExt;
    use futures::StreamExt;
    use futures::future::Either;
    use futures::future::select;
    use futures_concurrency::stream::Merge;
    use h2::RecvStream;
    use h2::server::SendResponse;
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

    pub(super) struct GrpcIo<T>(pub(super) PolledSocket<T>);

    impl<T: AsSockRef + Read> tokio::io::AsyncRead for GrpcIo<T> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let n =
                ready!(Pin::new(&mut self.get_mut().0).poll_read(cx, buf.initialize_unfilled()))?;
            buf.advance(n);
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl<T: AsSockRef + Write> tokio::io::AsyncWrite for GrpcIo<T> {
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
                        let _ = self.serve_connection_grpc(conn).await.map_err(|err| {
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

        /// Services a single, already-accepted connection using the gRPC
        /// (HTTP/2) protocol.
        ///
        /// This is useful when the caller owns the accept loop, for example to
        /// dispatch a connection to gRPC or another protocol based on a sniffed
        /// prefix byte.
        pub async fn serve_connection_grpc(
            &self,
            stream: PolledSocket<impl AsSockRef + Read + Write>,
        ) -> anyhow::Result<()> {
            let mut conn = h2::server::handshake(GrpcIo(stream))
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
                        Err(RequestError::H2(error)) if error.is_reset() => {
                            tracing::debug!(%error, "gRPC stream reset");
                            Ok(())
                        }
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
                    b'H' => std::time::Duration::from_hours(1),
                    b'M' => std::time::Duration::from_mins(1),
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
            let (ctx, _cancel) = ctx.with_cancel();

            let (head, body) = req.into_parts();
            let path = head.uri.path();
            let path = path.strip_prefix('/').ok_or(http::StatusCode::NOT_FOUND)?;
            let (service, method) = path.split_once('/').ok_or(http::StatusCode::NOT_FOUND)?;

            // No returning HTTP status code errors after this point.
            let mut resp = resp.send_response(response.body(())?, false)?;

            let result = {
                let invoke = std::pin::pin!(self.invoke_rpc(service, method, body, ctx));
                let reset = std::pin::pin!(std::future::poll_fn(|cx| resp.poll_reset(cx)));
                match select(invoke, reset).await {
                    Either::Left((result, _)) => result?,
                    Either::Right((result, _)) => {
                        result?;
                        return Ok(());
                    }
                }
            };

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

            Ok(recv
                .await
                .unwrap_or_else(|err| Err(status_from_err(Code::Internal, err))))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Client;
    use crate::Server;
    use crate::client::ExistingConnection;
    use crate::service::Code;
    use crate::service::ServiceRpc;
    use futures::StreamExt;
    use futures::executor::block_on;
    use pal_async::DefaultPool;
    use pal_async::local::block_with_io;
    use pal_async::socket::PolledSocket;
    use test_with_tracing::test;

    #[cfg(feature = "grpc")]
    async fn grpc_request(
        client: &mut h2::client::SendRequest<prost::bytes::Bytes>,
    ) -> (
        h2::client::ResponseFuture,
        h2::SendStream<prost::bytes::Bytes>,
    ) {
        std::future::poll_fn(|cx| client.poll_ready(cx))
            .await
            .unwrap();
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("http://localhost/{}/Method1", items::Example::NAME))
            .header("content-type", "application/grpc+proto")
            .body(())
            .unwrap();
        let (response, mut body) = client.send_request(request, false).unwrap();
        let data = prost::Message::encode_to_vec(&items::Method1Request {
            foo: "abc".into(),
            bar: "def".into(),
        });
        let mut frame = vec![0];
        frame.extend((data.len() as u32).to_be_bytes());
        frame.extend(data);
        body.send_data(frame.into(), true).unwrap();
        (response, body)
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn grpc_cancellation_reaches_pending_rpc_without_deadline() {
        use pal_async::task::Spawn;
        use std::time::Duration;

        DefaultPool::run_with(async |driver| {
            mesh::CancelContext::new()
                .with_timeout(Duration::from_secs(5))
                .until_cancelled(async {
                    for reset_stream in [true, false] {
                        let (client_socket, server_socket) =
                            unix_socket::UnixStream::pair().unwrap();
                        let mut server = Server::new();
                        let mut requests = server.add_service::<items::Example>();
                        let server_driver = driver.clone();
                        let server_task = driver.spawn("grpc-cancel-server", async move {
                            server
                                .serve_connection_grpc(
                                    PolledSocket::new(&server_driver, server_socket).unwrap(),
                                )
                                .await
                        });
                        let (mut client, connection) = h2::client::handshake(super::grpc::GrpcIo(
                            PolledSocket::new(&driver, client_socket).unwrap(),
                        ))
                        .await
                        .unwrap();
                        let mut connection_task =
                            Some(driver.spawn("grpc-cancel-client", connection));
                        let (response, mut stream) = grpc_request(&mut client).await;
                        let response = response.await.unwrap();
                        let (mut ctx, pending) = requests.next().await.unwrap();
                        assert!(ctx.deadline().is_none());
                        if reset_stream {
                            stream.send_reset(h2::Reason::CANCEL);
                        } else {
                            let _ = connection_task.take().unwrap().cancel().await;
                        }
                        assert_eq!(ctx.cancelled().await, mesh::CancelReason::Cancelled);
                        drop(pending);
                        drop(response);

                        if reset_stream {
                            let (response, _stream) = grpc_request(&mut client).await;
                            let (ctx, request) = requests.next().await.unwrap();
                            assert!(ctx.deadline().is_none());
                            let items::Example::Method1(input, send) = request else {
                                panic!("expected Method1");
                            };
                            send.send(Ok(items::Method1Response {
                                foo: input.foo,
                                bar: input.bar,
                            }));
                            let mut body = response.await.unwrap().into_body();
                            while let Some(data) = body.data().await {
                                let data = data.unwrap();
                                body.flow_control().release_capacity(data.len()).unwrap();
                            }
                            assert_eq!(body.trailers().await.unwrap().unwrap()["grpc-status"], "0");
                        }
                        drop(client);
                        if let Some(task) = connection_task {
                            let _ = task.cancel().await;
                        }
                        let _ = server_task.cancel().await;
                    }
                })
                .await
                .expect("gRPC transport cancellation did not reach the pending RPC");
        });
    }

    #[expect(clippy::allow_attributes)]
    mod items {
        include!(concat!(env!("OUT_DIR"), "/ttrpc.example.v1.rs"));
    }

    async fn send_native_request(
        writer: &mut (impl futures::AsyncWrite + Unpin),
        stream_id: u32,
        timeout_nano: u64,
    ) {
        let request = crate::message::Request {
            service: items::Example::NAME.into(),
            method: "Method1".into(),
            payload: prost::Message::encode_to_vec(&items::Method1Request {
                foo: stream_id.to_string(),
                bar: "pending".into(),
            }),
            timeout_nano,
            metadata: Vec::new(),
        };
        crate::message::write_message(
            writer,
            stream_id,
            crate::message::MESSAGE_TYPE_REQUEST,
            &mesh::payload::encode(request),
        )
        .await
        .unwrap();
    }

    fn assert_disconnect_result(result: anyhow::Result<()>) {
        if let Err(error) = result {
            let kind = error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind);
            assert!(
                matches!(
                    kind,
                    Some(
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                    )
                ),
                "unexpected server error after disconnect: {error:#}"
            );
        }
    }

    #[test]
    fn native_half_close_preserves_replies_until_full_disconnect() {
        use crate::message::MESSAGE_TYPE_RESPONSE;
        use crate::message::Response;
        use crate::message::read_message;
        use futures::AsyncWriteExt;
        use pal_async::task::Spawn;
        use std::time::Duration;

        DefaultPool::run_with(async |driver| {
            mesh::CancelContext::new()
                .with_timeout(Duration::from_secs(5))
                .until_cancelled(async {
                    let modes: &[bool] = if cfg!(windows) {
                        &[true]
                    } else {
                        &[true, false]
                    };
                    for &complete_replies in modes {
                        let (client_socket, server_socket) =
                            unix_socket::UnixStream::pair().unwrap();
                        let mut client = PolledSocket::new(&driver, client_socket).unwrap();
                        let mut server = Server::new();
                        let mut requests = server.add_service::<items::Example>();
                        let stream_ids = if complete_replies {
                            &[1, 3][..]
                        } else {
                            &[1][..]
                        };
                        for &stream_id in stream_ids {
                            send_native_request(&mut client, stream_id, 0).await;
                        }
                        // Buffer requests and the write EOF before starting the server.
                        client.close().await.unwrap();
                        let server_driver = driver.clone();
                        let server_task = driver.spawn("native-half-close-server", async move {
                            server
                                .serve_connection(
                                    PolledSocket::new(&server_driver, server_socket).unwrap(),
                                )
                                .await
                        });
                        let mut pending = Vec::new();
                        for _ in stream_ids {
                            let (mut ctx, request) = requests.next().await.unwrap();
                            assert!(ctx.deadline().is_none());
                            assert!(
                                !ctx.is_cancelled(),
                                "write EOF cancelled an accepted request"
                            );
                            pending.push((ctx, request));
                        }

                        if complete_replies {
                            for &stream_id in stream_ids.iter().rev() {
                                let (_, request) = pending.pop().unwrap();
                                let items::Example::Method1(input, send) = request else {
                                    panic!("expected Method1");
                                };
                                let response = items::Method1Response {
                                    foo: input.foo,
                                    bar: input.bar,
                                };
                                let expected = prost::Message::encode_to_vec(&response);
                                send.send(Ok(response));
                                let reply = read_message(&mut client).await.unwrap().unwrap();
                                assert_eq!(reply.stream_id, stream_id);
                                assert_eq!(reply.message_type, MESSAGE_TYPE_RESPONSE);
                                let Response::Payload(payload) =
                                    mesh::payload::decode(&reply.payload.unwrap()).unwrap()
                                else {
                                    panic!("expected a successful response");
                                };
                                assert_eq!(payload, expected);
                                for (ctx, _) in &mut pending {
                                    assert!(!ctx.is_cancelled());
                                }
                            }
                            server_task.await.unwrap();
                        } else {
                            let (mut ctx, request) = pending.pop().unwrap();
                            drop(client);
                            assert_eq!(ctx.cancelled().await, mesh::CancelReason::Cancelled);
                            assert_disconnect_result(server_task.await);
                            drop(request);
                        }
                    }
                })
                .await
                .expect("half closed native ttrpc connection did not finish");
        });
    }

    #[test]
    fn native_half_close_with_deadline_cancels_pending_request() {
        use futures::AsyncWriteExt;
        use pal_async::task::Spawn;
        use std::time::Duration;

        DefaultPool::run_with(async |driver| {
            mesh::CancelContext::new()
                .with_timeout(Duration::from_secs(5))
                .until_cancelled(async {
                    let (client_socket, server_socket) = unix_socket::UnixStream::pair().unwrap();
                    let mut client = PolledSocket::new(&driver, client_socket).unwrap();
                    send_native_request(&mut client, 1, 50_000_000).await;
                    client.close().await.unwrap();
                    let mut server = Server::new();
                    let mut requests = server.add_service::<items::Example>();
                    let server_driver = driver.clone();
                    let server_task = driver.spawn("native-deadline-server", async move {
                        server.run_single(&server_driver, server_socket).await
                    });
                    let (mut ctx, request) = requests.next().await.unwrap();
                    assert!(ctx.deadline().is_some());
                    assert_eq!(ctx.cancelled().await, mesh::CancelReason::DeadlineExceeded);
                    let items::Example::Method1(_, send) = request else {
                        panic!("expected Method1");
                    };
                    send.send(Err(crate::service::Status {
                        code: Code::DeadlineExceeded as i32,
                        message: "request deadline reached".into(),
                        details: Vec::new(),
                    }));
                    let reply = crate::message::read_message(&mut client)
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(reply.stream_id, 1);
                    let crate::message::Response::Status(status) =
                        mesh::payload::decode(&reply.payload.unwrap()).unwrap()
                    else {
                        panic!("expected a deadline response");
                    };
                    assert_eq!(status.code, Code::DeadlineExceeded as i32);
                    server_task.await.unwrap();
                })
                .await
                .expect("native request deadline failed after a write half close");
        });
    }

    #[test]
    fn native_tcp_half_close_preserves_replies() {
        use futures::AsyncWriteExt;
        use pal_async::task::Spawn;
        use std::time::Duration;

        DefaultPool::run_with(async |driver| {
            mesh::CancelContext::new()
                .with_timeout(Duration::from_secs(5))
                .until_cancelled(async {
                    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
                    let client_socket =
                        std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
                    client_socket.set_nodelay(true).unwrap();
                    let server_socket = listener.accept().unwrap().0;
                    let mut client = PolledSocket::new(&driver, client_socket).unwrap();
                    send_native_request(&mut client, 1, 0).await;
                    client.close().await.unwrap();
                    let mut server = Server::new();
                    let mut requests = server.add_service::<items::Example>();
                    let server_driver = driver.clone();
                    let server_task = driver.spawn("native-tcp-half-close-server", async move {
                        server.run_single(&server_driver, server_socket).await
                    });
                    let (mut ctx, request) = requests.next().await.unwrap();
                    assert!(!ctx.is_cancelled());
                    let items::Example::Method1(input, send) = request else {
                        panic!("expected Method1");
                    };
                    let response = items::Method1Response {
                        foo: input.foo,
                        bar: input.bar,
                    };
                    let expected = prost::Message::encode_to_vec(&response);
                    send.send(Ok(response));
                    let reply = crate::message::read_message(&mut client)
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(reply.stream_id, 1);
                    assert_eq!(reply.message_type, crate::message::MESSAGE_TYPE_RESPONSE);
                    let crate::message::Response::Payload(payload) =
                        mesh::payload::decode(&reply.payload.unwrap()).unwrap()
                    else {
                        panic!("expected a successful response");
                    };
                    assert_eq!(payload, expected);
                    server_task.await.unwrap();
                })
                .await
                .expect("native TCP half close discarded the response");
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_tcp_close_cancels_context_only_when_deadline_expires() {
        use pal_async::task::Spawn;
        use std::time::Duration;

        DefaultPool::run_with(async |driver| {
            mesh::CancelContext::new()
                .with_timeout(Duration::from_secs(5))
                .until_cancelled(async {
                    for timeout_nano in [0, 100_000_000] {
                        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
                        let client_socket =
                            std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
                        client_socket.set_nodelay(true).unwrap();
                        let server_socket = listener.accept().unwrap().0;
                        let mut client = PolledSocket::new(&driver, client_socket).unwrap();
                        send_native_request(&mut client, 1, timeout_nano).await;
                        drop(client);
                        let mut server = Server::new();
                        let mut requests = server.add_service::<items::Example>();
                        let server_driver = driver.clone();
                        let mut server_task = driver.spawn("native-tcp-close-server", async move {
                            server.run_single(&server_driver, server_socket).await
                        });
                        let (mut ctx, request) = requests.next().await.unwrap();
                        assert_eq!(ctx.deadline().is_some(), timeout_nano != 0);
                        assert!(!ctx.is_cancelled(), "a TCP FIN is not a full hangup signal");
                        assert!(futures::poll!(&mut server_task).is_pending());
                        let items::Example::Method1(input, send) = request else {
                            panic!("expected Method1");
                        };
                        if timeout_nano == 0 {
                            send.send(Ok(items::Method1Response {
                                foo: input.foo,
                                bar: input.bar,
                            }));
                        } else {
                            assert_eq!(ctx.cancelled().await, mesh::CancelReason::DeadlineExceeded);
                            send.send(Err(crate::service::Status {
                                code: Code::DeadlineExceeded as i32,
                                message: "request deadline reached".into(),
                                details: Vec::new(),
                            }));
                        }
                        assert_disconnect_result(server_task.await);
                    }
                })
                .await
                .expect("native TCP request did not respect its deadline");
        });
    }

    #[test]
    fn client_server() {
        let (c, s) = unix_socket::UnixStream::pair().unwrap();
        let mut server = Server::new();
        let mut recv = server.add_service::<items::Example>();
        let server_thread = std::thread::spawn(move || {
            block_with_io(async |driver| server.run_single(&driver, s).await)
        });

        let client_thread = std::thread::spawn(move || {
            DefaultPool::run_with(async |driver| {
                let client = Client::new(
                    &driver,
                    ExistingConnection::new(PolledSocket::new(&driver, c).unwrap()),
                );
                let response = client
                    .call()
                    .start(
                        items::Example::Method1,
                        items::Method1Request {
                            foo: "abc".to_string(),
                            bar: "def".to_string(),
                        },
                    )
                    .await
                    .unwrap();

                assert_eq!(&response.foo, "abc123");
                assert_eq!(&response.bar, "def456");

                let status = client
                    .call()
                    .start_raw(items::Example::NAME, "unknown", Vec::new())
                    .await
                    .unwrap_err();

                assert_eq!(status.code, Code::Unimplemented as i32);

                client.shutdown().await;
            })
        });

        block_on(async {
            let (_, req) = recv.next().await.unwrap();
            match req {
                items::Example::Method1(input, resp) => {
                    assert_eq!(&input.foo, "abc");
                    assert_eq!(&input.bar, "def");
                    resp.send(Ok(items::Method1Response {
                        foo: input.foo + "123",
                        bar: input.bar + "456",
                    }));
                }
                _ => panic!("{:?}", req),
            }

            assert!(recv.next().await.is_none());
        });

        client_thread.join().unwrap();
        server_thread.join().unwrap().unwrap();
    }
}
