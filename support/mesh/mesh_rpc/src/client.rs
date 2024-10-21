// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TTRPC client.

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
use anyhow::Context;
use futures_concurrency::future::Race;
use mesh::MeshPayload;
use pal_async::driver::Driver;
use pal_async::socket::AsSockRef;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::time::Duration;

/// A TTRPC client connection.
pub struct Client {
    send: mesh::Sender<mesh::Message>,
    task: Task<()>,
}

#[derive(MeshPayload)]
struct ClientRequest<T> {
    service: String,
    timeout: Option<u64>,
    rpc: T,
}

impl Client {
    /// Creates a new client from a connection.
    pub fn new<T>(driver: &(impl Driver + Spawn + ?Sized), conn: T) -> Self
    where
        T: 'static + Send + Sync + AsSockRef + Read + Write,
    {
        let (send, recv) = mesh::channel();
        let conn = PolledSocket::new(driver, conn).unwrap();
        let task = (&driver).spawn("ttrpc client", async move {
            if let Err(err) = Self::run(conn, recv).await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "client error"
                );
            }
        });
        Self {
            send: send.force_downcast(),
            task,
        }
    }

    async fn run(
        stream: PolledSocket<impl AsSockRef + Read + Write>,
        mut rpc_recv: mesh::Receiver<ClientRequest<GenericRpc>>,
    ) -> anyhow::Result<()> {
        let (mut reader, mut writer) = stream.split();
        let responses = Mutex::new(HashMap::<u32, mesh::OneshotSender<mesh::Message>>::new());
        let recv_task = async {
            while let Some(message) = read_message(&mut reader)
                .await
                .context("fatal connection error")?
            {
                let stream_id = message.stream_id;
                tracing::debug!(stream_id, "response");

                let response_send = responses.lock().remove(&stream_id);

                let Some(response_send) = response_send else {
                    tracing::error!(stream_id, "response for unknown stream");
                    continue;
                };

                let result = handle_message(message);

                response_send.send(mesh::Message::new(result));
            }
            Ok(())
        };

        let send_task = async {
            let mut next_stream_id = 1;
            while let Ok(request) = rpc_recv.recv().await {
                responses
                    .lock()
                    .insert(next_stream_id, request.rpc.port.into());

                let payload = mesh::payload::encode(Request {
                    service: request.service,
                    method: request.rpc.method,
                    payload: request.rpc.data,
                    timeout_nano: request.timeout.unwrap_or(0),
                    metadata: vec![],
                });

                write_message(&mut writer, next_stream_id, MESSAGE_TYPE_REQUEST, &payload)
                    .await
                    .context("failed to write to connection")?;

                next_stream_id = next_stream_id.wrapping_add(2);
            }
            Ok(())
        };

        (send_task, recv_task).race().await
    }

    /// Sends an RPC message to the server.
    fn send<T: ServiceRpc>(&self, rpc: T, timeout: Option<Duration>) {
        self.send.send(mesh::Message::new(ClientRequest {
            service: T::NAME.to_string(),
            timeout: timeout.map(|d| d.as_nanos() as u64),
            rpc,
        }));
    }

    /// Calls a remote function `rpc` with `input` and with no timeout, and
    /// waits for a result.
    pub async fn call<F, R, T, U>(&self, rpc: F, input: T) -> anyhow::Result<Result<U, Status>>
    where
        F: FnOnce(T, mesh::OneshotSender<Result<U, Status>>) -> R,
        R: ServiceRpc,
        U: MeshPayload,
    {
        self.start_call(rpc, input, None).await.context("rpc error")
    }

    /// Calls a remote function `rpc` with `input` and returns a mesh channel
    /// that will receive the result.
    pub fn start_call<F, R, T, U>(
        &self,
        rpc: F,
        input: T,
        timeout: Option<Duration>,
    ) -> mesh::OneshotReceiver<Result<U, Status>>
    where
        F: FnOnce(T, mesh::OneshotSender<Result<U, Status>>) -> R,
        R: ServiceRpc,
        U: MeshPayload,
    {
        let (send, recv) = mesh::oneshot();

        self.send(rpc(input, send), timeout);
        recv
    }

    /// Shuts down the client, waiting for the associated task to complete.
    pub async fn shutdown(self) {
        drop(self.send);
        self.task.await;
    }
}

fn handle_message(message: ReadResult) -> Result<Vec<u8>, Status> {
    match message.message_type {
        MESSAGE_TYPE_RESPONSE => {
            let payload = message.payload.map_err(|err @ TooLongError { .. }| {
                status_from_err(Code::ResourceExhausted, err)
            })?;

            let response = mesh::payload::decode(&payload)
                .map_err(|err| status_from_err(Code::Unknown, err))?;

            match response {
                Response::Payload(payload) => Ok(payload),
                Response::Status(status) => Err(status),
            }
        }
        ty => Err(status_from_err(
            Code::Internal,
            ProtocolError::InvalidMessageType(ty),
        )),
    }
}
