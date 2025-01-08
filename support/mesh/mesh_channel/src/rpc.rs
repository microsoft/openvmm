// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Remote Procedure Call functionality.

use super::error::RemoteResult;
use crate::error::RemoteError;
use crate::error::RemoteResultExt;
use crate::error::RpcError;
use crate::oneshot;
use crate::OneshotReceiver;
use crate::OneshotSender;
use mesh_node::message::MeshField;
use mesh_protobuf::Protobuf;
use std::future::Future;
use std::pin::Pin;
use std::task::ready;
use std::task::Poll;

/// An RPC message for a request with input of type `I` and output of type `R`.
/// The receiver of the message should process the request and return results
/// via the `Sender<R>`.
#[derive(Debug, Protobuf)]
#[mesh(
    bound = "I: 'static + MeshField + Send, R: 'static + MeshField + Send",
    resource = "mesh_node::resource::Resource"
)]
pub struct Rpc<I, R>(pub I, pub OneshotSender<R>);

/// An RPC message with a failable result.
pub type FailableRpc<I, R> = Rpc<I, RemoteResult<R>>;

impl<I, R: 'static + Send> Rpc<I, R> {
    /// Handles an RPC request by calling `f` and sending the result to the
    /// initiator.
    pub fn handle_sync<F>(self, f: F)
    where
        F: FnOnce(I) -> R,
    {
        let r = f(self.0);
        self.1.send(r);
    }

    /// Handles an RPC request by calling `f`, awaiting its result, and sending
    /// the result to the initiator.
    pub async fn handle<F, Fut>(self, f: F)
    where
        F: FnOnce(I) -> Fut,
        Fut: Future<Output = R>,
    {
        let r = f(self.0).await;
        self.1.send(r);
    }

    /// Handles an RPC request by calling `f`, awaiting its result, and sending
    /// Ok results back to the initiator.
    ///
    /// If `f` fails, the error is propagated back to the caller, and the RPC
    /// channel is dropped (resulting in a `RecvError::Closed` on the
    /// initiator).
    pub async fn handle_must_succeed<F, Fut, E>(self, f: F) -> Result<(), E>
    where
        F: FnOnce(I) -> Fut,
        Fut: Future<Output = Result<R, E>>,
    {
        let r = f(self.0).await?;
        self.1.send(r);
        Ok(())
    }

    /// Completes the RPC with the specified result value.
    pub fn complete(self, result: R) {
        self.1.send(result);
    }
}

impl<I, R: 'static + Send> Rpc<I, Result<R, RemoteError>> {
    /// Handles an RPC request by calling `f` and sending the result to the
    /// initiator, after converting any error to a [`RemoteError`].
    pub fn handle_failable_sync<F, E>(self, f: F)
    where
        F: FnOnce(I) -> Result<R, E>,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let r = f(self.0);
        self.1.send(r.map_err(RemoteError::new));
    }

    /// Handles an RPC request by calling `f`, awaiting its result, and sending
    /// the result to the initiator, after converting any error to a
    /// [`RemoteError`].
    pub async fn handle_failable<F, Fut, E>(self, f: F)
    where
        F: FnOnce(I) -> Fut,
        Fut: Future<Output = Result<R, E>>,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let r = f(self.0).await;
        self.1.send(r.map_err(RemoteError::new));
    }
}

/// A trait implemented by objects that can send RPC requests.
pub trait RpcSend {
    /// The message type for this sender.
    type Message;

    /// Send an RPC request.
    fn send_rpc(&self, message: Self::Message);

    /// Issues a request and returns a channel to receive the result.
    ///
    /// `f` maps an [`Rpc`] object to the message type and is often an enum
    /// variant name.
    ///
    /// `input` is the input to the call.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use mesh_channel::rpc::{Rpc, RpcSend};
    /// # use mesh_channel::Sender;
    /// enum Request {
    ///     Add(Rpc<(u32, u32), u32>),
    /// }
    /// async fn add(send: &Sender<Request>) {
    ///     assert_eq!(send.call(Request::Add, (3, 4)).await.unwrap(), 7);
    /// }
    /// ```
    fn call<F, I, R>(&self, f: F, input: I) -> OneshotReceiver<R>
    where
        F: FnOnce(Rpc<I, R>) -> Self::Message,
        R: 'static + Send,
    {
        let (result_send, result_recv) = oneshot();
        self.send_rpc(f(Rpc(input, result_send)));
        result_recv
    }

    /// Issues a request and returns an object to receive the result.
    ///
    /// This is like [`RpcSend::call`], but for RPCs that return a [`Result`].
    /// The returned object combines the channel error and the call's error into
    /// a single [`RpcError`] type, which makes it easier to handle errors.
    fn call_failable<F, I, T, E>(&self, f: F, input: I) -> RpcResultReceiver<Result<T, E>>
    where
        F: FnOnce(Rpc<I, Result<T, E>>) -> Self::Message,
        T: 'static + Send,
        E: 'static + Send,
    {
        RpcResultReceiver(self.call(f, input))
    }
}

/// The result future of an [`RpcSend::call_failable`] call.
#[must_use]
pub struct RpcResultReceiver<R>(OneshotReceiver<R>);

impl<T: 'static + Send, E: 'static + Send> Future for RpcResultReceiver<Result<T, E>> {
    type Output = Result<T, RpcError<E>>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(ready!(Pin::new(&mut self.get_mut().0).poll(cx)).flatten())
    }
}

#[cfg(feature = "newchan")]
impl<T: 'static + Send> RpcSend for mesh_channel_core::Sender<T> {
    type Message = T;
    fn send_rpc(&self, message: T) {
        self.send(message);
    }
}

#[cfg(not(feature = "newchan_spsc"))]
impl<T: 'static + Send> RpcSend for crate::Sender<T> {
    type Message = T;
    fn send_rpc(&self, message: T) {
        self.send(message);
    }
}

#[cfg(not(feature = "newchan_mpsc"))]
impl<T: 'static + Send> RpcSend for crate::MpscSender<T> {
    type Message = T;
    fn send_rpc(&self, message: T) {
        self.send(message);
    }
}

impl<T: RpcSend> RpcSend for &T {
    type Message = T::Message;
    fn send_rpc(&self, message: T::Message) {
        (*self).send_rpc(message);
    }
}
