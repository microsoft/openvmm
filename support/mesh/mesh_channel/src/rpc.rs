// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Remote Procedure Call functionality.

use super::error::RemoteResult;
use crate::error::RemoteError;
use crate::oneshot;
use crate::OneshotReceiver;
use crate::OneshotSender;
use crate::RecvError;
use mesh_node::message::MeshField;
use mesh_protobuf::Protobuf;
use std::convert::Infallible;
use std::fmt::Debug;
use std::pin::Pin;
use std::task::ready;
use std::task::Poll;
use thiserror::Error;

/// An RPC message for a request with input of type `I` and output of type `R`.
/// The receiver of the message should process the request and return results
/// via the `Sender<R>`.
#[derive(Protobuf)]
#[mesh(
    bound = "I: 'static + MeshField + Send, R: 'static + MeshField + Send",
    resource = "mesh_node::resource::Resource"
)]
pub struct Rpc<I, R>(I, OneshotSender<R>);

impl<I: Debug, R> Debug for Rpc<I, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Rpc").field(&self.0).finish()
    }
}

/// An RPC message with a failable result.
pub type FailableRpc<I, R> = Rpc<I, RemoteResult<R>>;

impl<I, R: 'static + Send> Rpc<I, R> {
    /// Returns a new RPC message with `input` and no one listening for the
    /// result.
    pub fn detached(input: I) -> Self {
        let (result_send, _) = oneshot();
        Rpc(input, result_send)
    }

    /// Returns the input to the RPC.
    pub fn input(&self) -> &I {
        &self.0
    }

    /// Splits the RPC into its input and an input-less RPC. This is useful when
    /// the input is needed in one place but the RPC will be completed in
    /// another.
    pub fn split(self) -> (I, Rpc<(), R>) {
        (self.0, Rpc((), self.1))
    }

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
    pub async fn handle<F>(self, f: F)
    where
        F: AsyncFnOnce(I) -> R,
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
    pub async fn handle_must_succeed<F, E>(self, f: F) -> Result<(), E>
    where
        F: AsyncFnOnce(I) -> Result<R, E>,
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
    pub async fn handle_failable<F, E>(self, f: F)
    where
        F: AsyncFnOnce(I) -> Result<R, E>,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let r = f(self.0).await;
        self.1.send(r.map_err(RemoteError::new));
    }

    /// Fails the RPC with the specified error.
    pub fn fail<E>(self, error: E)
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        self.1.send(Err(RemoteError::new(error)));
    }
}

/// A trait implemented by objects that can send RPC requests.
pub trait RpcSend: Sized {
    /// The message type for this sender.
    type Message;

    /// Send an RPC request.
    fn send_rpc(self, message: Self::Message);

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
    fn call<F, I, R>(self, f: F, input: I) -> PendingRpc<R>
    where
        F: FnOnce(Rpc<I, R>) -> Self::Message,
        R: 'static + Send,
    {
        let (result_send, result_recv) = oneshot();
        self.send_rpc(f(Rpc(input, result_send)));
        PendingRpc(result_recv)
    }

    /// Issues a request and returns an object to receive the result.
    ///
    /// This is like [`RpcSend::call`], but for RPCs that return a [`Result`].
    /// The returned object combines the channel error and the call's error into
    /// a single [`RpcError`] type, which makes it easier to handle errors.
    fn call_failable<F, I, T, E>(self, f: F, input: I) -> PendingFailableRpc<T, E>
    where
        F: FnOnce(Rpc<I, Result<T, E>>) -> Self::Message,
        T: 'static + Send,
        E: 'static + Send,
    {
        PendingFailableRpc(self.call(f, input))
    }
}

/// A trait implemented by objects that can try to send RPC requests but may
/// fail.
pub trait TryRpcSend: Sized {
    /// The message type for this sender.
    type Message;
    /// The error type returned when sending an RPC request fails.
    type Error;

    /// Tries to send an RPC request.
    fn try_send_rpc(self, message: Self::Message) -> Result<(), Self::Error>;

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
    fn try_call<F, I, R>(self, f: F, input: I) -> Result<PendingRpc<R>, Self::Error>
    where
        F: FnOnce(Rpc<I, R>) -> Self::Message,
        R: 'static + Send,
    {
        let (result_send, result_recv) = oneshot();
        self.try_send_rpc(f(Rpc(input, result_send)))?;
        Ok(PendingRpc(result_recv))
    }

    /// Issues a request and returns an object to receive the result.
    ///
    /// This is like [`TryRpcSend::try_call`], but for RPCs that return a
    /// [`Result`]. The returned object combines the channel error and the
    /// call's error into a single [`RpcError`] type, which makes it easier to
    /// handle errors.
    fn try_call_failable<F, I, T, E>(
        self,
        f: F,
        input: I,
    ) -> Result<PendingFailableRpc<T, E>, Self::Error>
    where
        F: FnOnce(Rpc<I, Result<T, E>>) -> Self::Message,
        T: 'static + Send,
        E: 'static + Send,
    {
        Ok(PendingFailableRpc(self.try_call(f, input)?))
    }
}

/// An error from an RPC call, via
/// [`RpcSend::call_failable`] or [`RpcSend::call`].
#[derive(Debug, Error)]
pub enum RpcError<E = Infallible> {
    #[error(transparent)]
    Call(E),
    #[error(transparent)]
    Channel(RecvError),
}

/// The result future of an [`RpcSend::call`] call.
#[must_use]
#[derive(Debug)]
pub struct PendingRpc<T>(OneshotReceiver<T>);

impl<T: 'static + Send> Future for PendingRpc<T> {
    type Output = Result<T, RpcError<Infallible>>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(ready!(Pin::new(&mut self.get_mut().0).poll(cx)).map_err(RpcError::Channel))
    }
}

/// The result future of an [`RpcSend::call_failable`] call.
#[must_use]
#[derive(Debug)]
pub struct PendingFailableRpc<T, E = RemoteError>(PendingRpc<Result<T, E>>);

impl<T: 'static + Send, E: 'static + Send> Future for PendingFailableRpc<T, E> {
    type Output = Result<T, RpcError<E>>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let r = ready!(Pin::new(&mut self.get_mut().0).poll(cx));
        match r {
            Ok(Ok(t)) => Ok(t),
            Ok(Err(e)) => Err(RpcError::Call(e)),
            Err(RpcError::Channel(e)) => Err(RpcError::Channel(e)),
        }
        .into()
    }
}

impl<T: 'static + Send> RpcSend for OneshotSender<T> {
    type Message = T;
    fn send_rpc(self, message: T) {
        self.send(message);
    }
}

#[cfg(feature = "newchan")]
impl<T: 'static + Send> RpcSend for &mesh_channel_core::Sender<T> {
    type Message = T;
    fn send_rpc(self, message: T) {
        self.send(message);
    }
}

#[cfg(not(feature = "newchan_spsc"))]
impl<T: 'static + Send> RpcSend for &crate::Sender<T> {
    type Message = T;
    fn send_rpc(&self, message: T) {
        self.send(message);
    }
}

#[cfg(not(feature = "newchan_mpsc"))]
impl<T: 'static + Send> RpcSend for &crate::MpscSender<T> {
    type Message = T;
    fn send_rpc(self, message: T) {
        self.send(message);
    }
}
