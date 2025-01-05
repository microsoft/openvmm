#![allow(unsafe_code)]

use crate::ChannelError;
use crate::RecvError;
use mesh_node::local_node::HandleMessageError;
use mesh_node::local_node::HandlePortEvent;
use mesh_node::local_node::Port;
use mesh_node::local_node::PortField;
use mesh_node::local_node::PortWithHandler;
use mesh_node::message::MeshField;
use mesh_node::message::Message;
use mesh_protobuf::DefaultEncoding;
use parking_lot::Mutex;
use std::fmt::Debug;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::Arc;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use thiserror::Error;

/// Creates a unidirection channel for sending a single value of type `T`.
///
/// The channel is automatically closed after the value is sent. Use this
/// instead of [`channel`] when only one value ever needs to be sent to avoid
/// programming errors where the channel is left open longer than necessary.
/// This is also more efficient.
///
/// Use [`OneshotSender::send`] and [`OneshotReceiver`] (directly as a future)
/// to communicate between the ends of the channel.
///
/// Both channel endpoints are initially local to this process, but either or
/// both endpoints may be sent to other processes via a cross-process channel
/// that has already been established.
///
/// ```rust
/// # use mesh_channel_core::*;
/// # futures::executor::block_on(async {
/// let (send, recv) = oneshot::<u32>();
/// send.send(5);
/// let n = recv.await.unwrap();
/// assert_eq!(n, 5);
/// # });
/// ```
pub fn oneshot<T>() -> (OneshotSender<T>, OneshotReceiver<T>) {
    fn oneshot_core() -> (OneshotSenderCore, OneshotReceiverCore) {
        let slot = Arc::new(Slot(Mutex::new(SlotState::Waiting(None))));
        (
            OneshotSenderCore(slot.clone()),
            OneshotReceiverCore {
                slot: Some(slot),
                port: None,
            },
        )
    }

    let (sender, receiver) = oneshot_core();
    (
        OneshotSender(sender, PhantomData),
        OneshotReceiver(receiver, PhantomData),
    )
}

/// The sending half of a channel returned by [`oneshot`].
pub struct OneshotSender<T>(OneshotSenderCore, PhantomData<Arc<Mutex<T>>>);

impl<T> Debug for OneshotSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<T> OneshotSender<T> {
    /// Sends `value` to the receiving endpoint of the channel.
    pub fn send(self, value: T) {
        // SAFETY: the slot is of type `T`.
        unsafe { self.0.send(value) }
    }
}

impl<T: 'static + MeshField + Send> DefaultEncoding for OneshotSender<T> {
    type Encoding = PortField;
}

impl<T: 'static + MeshField + Send> From<OneshotSender<T>> for Port {
    fn from(sender: OneshotSender<T>) -> Self {
        // SAFETY: the slot is of type `T`.
        unsafe { sender.0.into_port::<T>() }
    }
}

impl<T: 'static + MeshField + Send> From<Port> for OneshotSender<T> {
    fn from(port: Port) -> Self {
        Self(OneshotSenderCore::from_port::<T>(port), PhantomData)
    }
}

/// # Safety
/// The caller must ensure that `value` is of type `T`.
unsafe fn encode_message<T: 'static + MeshField + Send>(value: BoxedValue) -> Message {
    // SAFETY: the caller ensures that `value` is of type `T`.
    let value = unsafe { value.cast::<T>() };
    Message::new((value,))
}

fn decode_message<T: 'static + MeshField + Send>(message: Message) -> Result<BoxedValue, ChannelError> {
    let (value,) = message.parse::<(Box<T>,)>()?;
    Ok(BoxedValue::new(value))
}

#[derive(Debug)]
struct OneshotSenderCore(Arc<Slot>);

impl Drop for OneshotSenderCore {
    fn drop(&mut self) {
        self.close();
    }
}

impl OneshotSenderCore {
    fn into_slot(self) -> Arc<Slot> {
        match *ManuallyDrop::new(self) {
            Self(ref slot) => {
                // SAFETY: `slot` is not dropped.
                unsafe { <*const _>::read(slot) }
            }
        }
    }

    fn close(&self) {
        let mut state = self.0 .0.lock();
        match std::mem::replace(&mut *state, SlotState::Done) {
            SlotState::Waiting(waker) => {
                drop(state);
                if let Some(waker) = waker {
                    waker.wake();
                }
            }
            SlotState::Sent(v) => {
                *state = SlotState::Sent(v);
            }
            SlotState::Done => {}
            SlotState::SenderRemote { .. } => unreachable!(),
            SlotState::ReceiverRemote(port, _) => {
                drop(port);
            }
        }
    }

    /// # Safety
    /// The caller must ensure that the slot is of type `T`.
    unsafe fn send<T>(self, value: T) {
        fn send(this: OneshotSenderCore, value: BoxedValue) -> Option<BoxedValue> {
            let slot = this.into_slot();
            let mut state = slot.0.lock();
            match std::mem::replace(&mut *state, SlotState::Done) {
                SlotState::ReceiverRemote(port, encode) => {
                    // SAFETY: `encode` has been set to operate on values of type
                    // `T`, and `value` is of type `T`.
                    let value = unsafe { encode(value) };
                    port.send(value);
                    None
                }
                SlotState::Waiting(waker) => {
                    *state = SlotState::Sent(value);
                    drop(state);
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                    None
                }
                SlotState::Done => Some(value),
                SlotState::Sent { .. } | SlotState::SenderRemote { .. } => unreachable!(),
            }
        }
        if let Some(value) = send(self, BoxedValue::new(Box::new(value))) {
            // SAFETY: the value is of type `T`, and it has not been dropped.
            unsafe { value.drop::<T>() };
        }
    }

    /// # Safety
    /// The caller must ensure that the slot is of type `T`.
    unsafe fn into_port<T: 'static + MeshField + Send>(self) -> Port {
        fn into_port(this: OneshotSenderCore, decode: DecodeFn) -> Port {
            let slot = this.into_slot();
            let mut state = slot.0.lock();
            match std::mem::replace(&mut *state, SlotState::Done) {
                SlotState::Waiting(waker) => {
                    let (send, recv) = Port::new_pair();
                    *state = SlotState::SenderRemote(recv, decode);
                    drop(state);
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                    send
                }
                SlotState::ReceiverRemote(port, _) => port,
                SlotState::Done => Port::new_pair().0,
                SlotState::Sent(_) | SlotState::SenderRemote { .. } => unreachable!(),
            }
        }
        into_port(self, decode_message::<T>)
    }

    fn from_port<T: 'static + MeshField + Send>(port: Port) -> Self {
        fn from_port(port: Port, encode: EncodeFn) -> OneshotSenderCore {
            let slot = Arc::new(Slot(Mutex::new(SlotState::ReceiverRemote(port, encode))));
            OneshotSenderCore(slot)
        }
        from_port(port, encode_message::<T>)
    }
}

/// The receiving half of a channel returned by [`oneshot`].
///
/// A value is received by `poll`ing or `await`ing the channel.
pub struct OneshotReceiver<T>(OneshotReceiverCore, PhantomData<Arc<Mutex<T>>>);

impl<T> Debug for OneshotReceiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<T> OneshotReceiver<T> {
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        // SAFETY: the slot is of type `T`.
        let v = unsafe { ready!(self.0.poll_recv(cx))? };
        Ok(*v).into()
    }

    fn into_core(self) -> OneshotReceiverCore {
        let Self(ref core, _) = *ManuallyDrop::new(self);
        // SAFETY: `core` is not dropped.
        unsafe { <*const _>::read(core) }
    }
}

impl<T> Drop for OneshotReceiver<T> {
    fn drop(&mut self) {
        // SAFETY: the slot is of type `T`.
        unsafe { self.0.clear::<T>() };
    }
}

impl<T> Future for OneshotReceiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_recv(cx)
    }
}

impl<T: 'static + MeshField + Send> DefaultEncoding for OneshotReceiver<T> {
    type Encoding = PortField;
}

impl<T: 'static + MeshField + Send> From<OneshotReceiver<T>> for Port {
    fn from(receiver: OneshotReceiver<T>) -> Self {
        // SAFETY: the slot is of type `T`.
        unsafe { receiver.into_core().into_port::<T>() }
    }
}

impl<T: 'static + MeshField + Send> From<Port> for OneshotReceiver<T> {
    fn from(port: Port) -> Self {
        Self(OneshotReceiverCore::from_port::<T>(port), PhantomData)
    }
}

#[derive(Debug)]
struct OneshotReceiverCore {
    slot: Option<Arc<Slot>>,
    port: Option<PortWithHandler<SlotHandler>>,
}

impl OneshotReceiverCore {
    // # Safety
    // The caller must ensure that the slot is of type `T`.
    unsafe fn clear<T>(&mut self) {
        fn clear(this: &mut OneshotReceiverCore) -> Option<BoxedValue> {
            let slot = this.slot.take()?;
            let v = if let SlotState::Sent(value) =
                std::mem::replace(&mut *slot.0.lock(), SlotState::Done)
            {
                Some(value)
            } else {
                None
            };
            v
        }
        if let Some(v) = clear(self) {
            // SAFETY: the value is of type `T`.
            unsafe { v.drop::<T>() };
        }
    }

    // # Safety
    // The caller must ensure that `T` is slot's type.
    unsafe fn poll_recv<T>(&mut self, cx: &mut Context<'_>) -> Poll<Result<Box<T>, RecvError>> {
        fn poll_recv(
            this: &mut OneshotReceiverCore,
            cx: &mut Context<'_>,
        ) -> Poll<Result<BoxedValue, RecvError>> {
            let Some(slot) = &this.slot else {
                return Poll::Ready(Err(RecvError::Closed));
            };
            let v = loop {
                let mut state = slot.0.lock();
                break match std::mem::replace(&mut *state, SlotState::Done) {
                    SlotState::SenderRemote(port, decode) => {
                        *state = SlotState::Waiting(None);
                        drop(state);
                        assert!(this.port.is_none());
                        this.port = Some(port.set_handler(SlotHandler {
                            slot: slot.clone(),
                            decode,
                        }));
                        continue;
                    }
                    SlotState::Waiting(mut waker) => {
                        if let Some(waker) = &mut waker {
                            waker.clone_from(cx.waker());
                        } else {
                            waker = Some(cx.waker().clone());
                        }
                        *state = SlotState::Waiting(waker);
                        return Poll::Pending;
                    }
                    SlotState::Sent(data) => Ok(data),
                    SlotState::Done => {
                        let err = this.port.as_ref().map_or(RecvError::Closed, |port| {
                            port.is_closed()
                                .map(|_| RecvError::Closed)
                                .unwrap_or_else(|err| RecvError::Error(err.into()))
                        });
                        Err(err)
                    }
                    SlotState::ReceiverRemote { .. } => {
                        unreachable!()
                    }
                };
            };
            Poll::Ready(v)
        }
        poll_recv(self, cx).map(|r| r.map(|v| unsafe { v.cast::<T>() }))
    }

    /// # Safety
    /// The caller must ensure that `encode` is a valid function to encode
    /// values of type `T`, the type of this slot.
    unsafe fn into_port<T: 'static + MeshField + Send>(self) -> Port {
        fn into_port(mut this: OneshotReceiverCore, encode: EncodeFn) -> Port {
            let existing = this.port.take().map(|port| port.remove_handler().0);
            let Some(slot) = this.slot.take() else {
                return existing.unwrap_or_else(|| Port::new_pair().0);
            };
            let mut state = slot.0.lock();
            match std::mem::replace(&mut *state, SlotState::Done) {
                SlotState::SenderRemote(port, _) => {
                    assert!(existing.is_none());
                    port
                }
                SlotState::Waiting(_) => {
                    let (send, recv) = Port::new_pair();
                    *state = SlotState::ReceiverRemote(recv, encode);
                    send
                }
                SlotState::Sent(value) => {
                    let (send, recv) = Port::new_pair();
                    // SAFETY: `encode` has been set to operate on values of type
                    // `T`, the type of this slot.
                    let value = unsafe { encode(value) };
                    send.send(value);
                    if let Some(existing) = existing {
                        existing.bridge(send);
                    }
                    recv
                }
                SlotState::Done => existing.unwrap_or_else(|| Port::new_pair().0),
                SlotState::ReceiverRemote { .. } => unreachable!(),
            }
        }
        into_port(self, encode_message::<T>)
    }

    fn from_port<T: 'static + MeshField + Send>(port: Port) -> Self {
        fn from_port(port: Port, decode: DecodeFn) -> OneshotReceiverCore {
            let slot = Arc::new(Slot(Mutex::new(SlotState::SenderRemote(port, decode))));
            OneshotReceiverCore {
                slot: Some(slot),
                port: None,
            }
        }
        from_port(port, decode_message::<T>)
    }
}

#[derive(Debug)]
enum SlotState {
    Done,
    Waiting(Option<Waker>),
    Sent(BoxedValue),
    SenderRemote(Port, DecodeFn),
    ReceiverRemote(Port, EncodeFn),
}

type EncodeFn = unsafe fn(BoxedValue) -> Message;
type DecodeFn = unsafe fn(Message) -> Result<BoxedValue, ChannelError>;

#[derive(Debug)]
struct BoxedValue(NonNull<()>);

// SAFETY: `BoxedValue` is `Send` and `Sync` even though the underlying element
// types may not be. It is the caller's responsibility to ensure that they don't
// send or share this across threads when it shouldn't be.
unsafe impl Send for BoxedValue {}
/// SAFETY: see above.
unsafe impl Sync for BoxedValue {}

impl BoxedValue {
    fn new<T>(value: Box<T>) -> Self {
        Self(NonNull::new(Box::into_raw(value).cast()).unwrap())
    }

    /// # Safety
    /// The caller must ensure that `T` is the correct type of the value, and that
    /// the value has not been sent across threads unless `T` is `Send`.
    #[expect(clippy::unnecessary_box_returns)]
    unsafe fn cast<T>(self) -> Box<T> {
        // SAFETY: the caller ensures that `T` is the correct type of the value.
        unsafe { Box::from_raw(self.0.cast::<T>().as_ptr()) }
    }

    /// # Safety
    /// The caller must ensure that `T` is the correct type of the value and that
    /// the value has not been sent across threads unless `T` is `Send`.
    unsafe fn drop<T>(self) {
        // SAFETY: the caller ensures that `T` is the correct type of the value.
        let _ = unsafe { self.cast::<T>() };
    }
}

#[derive(Debug)]
struct Slot(Mutex<SlotState>);

struct SlotHandler {
    slot: Arc<Slot>,
    decode: DecodeFn,
}

#[derive(Debug, Error)]
#[error("unexpected oneshot message")]
struct UnexpectedMessage;

impl SlotHandler {
    fn close_or_fail(&mut self, control: &mut mesh_node::local_node::PortControl<'_>, fail: bool) {
        let mut state = self.slot.0.lock();
        match std::mem::replace(&mut *state, SlotState::Done) {
            SlotState::Waiting(waker) => {
                if let Some(waker) = waker {
                    control.wake(waker);
                }
            }
            SlotState::Sent(v) => {
                if !fail {
                    *state = SlotState::Sent(v);
                }
            }
            SlotState::Done => {}
            SlotState::SenderRemote { .. } | SlotState::ReceiverRemote { .. } => unreachable!(),
        }
    }
}

impl HandlePortEvent for SlotHandler {
    fn message(
        &mut self,
        control: &mut mesh_node::local_node::PortControl<'_>,
        message: Message,
    ) -> Result<(), HandleMessageError> {
        let mut state = self.slot.0.lock();
        match std::mem::replace(&mut *state, SlotState::Done) {
            SlotState::Waiting(waker) => {
                // SAFETY: the users of the slot will ensure it is not
                // sent/shared across threads unless the underlying type is
                // Send/Sync.
                let r = unsafe { (self.decode)(message) };
                let value = r.map_err(HandleMessageError::new)?;
                *state = SlotState::Sent(value);
                drop(state);
                if let Some(waker) = waker {
                    control.wake(waker);
                }
            }
            SlotState::Sent(v) => {
                *state = SlotState::Sent(v);
                return Err(HandleMessageError::new(UnexpectedMessage));
            }
            SlotState::Done => {
                *state = SlotState::Done;
            }
            SlotState::SenderRemote { .. } | SlotState::ReceiverRemote { .. } => unreachable!(),
        }
        Ok(())
    }

    fn close(&mut self, control: &mut mesh_node::local_node::PortControl<'_>) {
        self.close_or_fail(control, false);
    }

    fn fail(
        &mut self,
        control: &mut mesh_node::local_node::PortControl<'_>,
        _err: mesh_node::local_node::NodeError,
    ) {
        self.close_or_fail(control, true);
    }

    fn drain(&mut self) -> Vec<Message> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::oneshot;
    use crate::OneshotReceiver;
    use crate::OneshotSender;
    use crate::RecvError;
    use futures::executor::block_on;
    use mesh_node::local_node::Port;
    use mesh_node::message::Message;
    use std::cell::Cell;
    use test_with_tracing::test;

    // Ensure `Send` and `Sync` are implemented correctly.
    static_assertions::assert_impl_all!(OneshotSender<i32>: Send, Sync);
    static_assertions::assert_impl_all!(OneshotReceiver<i32>: Send, Sync);
    static_assertions::assert_impl_all!(OneshotSender<Cell<i32>>: Send, Sync);
    static_assertions::assert_impl_all!(OneshotReceiver<Cell<i32>>: Send, Sync);
    static_assertions::assert_not_impl_any!(OneshotSender<*const ()>: Send, Sync);
    static_assertions::assert_not_impl_any!(OneshotReceiver<*const ()>: Send, Sync);

    #[test]
    fn test_oneshot() {
        block_on(async {
            let (sender, receiver) = oneshot();
            sender.send(String::from("foo"));
            assert_eq!(receiver.await.unwrap(), "foo");
        })
    }

    #[test]
    fn test_oneshot_convert_sender_port() {
        block_on(async {
            let (sender, receiver) = oneshot::<String>();
            let sender = OneshotSender::<String>::from(Port::from(sender));
            sender.send(String::from("foo"));
            assert_eq!(receiver.await.unwrap(), "foo");
        })
    }

    #[test]
    fn test_oneshot_convert_receiver_port() {
        block_on(async {
            let (sender, receiver) = oneshot::<String>();
            let receiver = OneshotReceiver::<String>::from(Port::from(receiver));
            sender.send(String::from("foo"));
            assert_eq!(receiver.await.unwrap(), "foo");
        })
    }

    #[test]
    fn test_oneshot_message_corruption() {
        block_on(async {
            let (sender, receiver) = oneshot();
            let receiver = OneshotReceiver::<i32>::from(Port::from(receiver));
            sender.send("text".to_owned());
            let RecvError::Error(err) = receiver.await.unwrap_err() else {
                panic!()
            };
            tracing::info!(error = &err as &dyn std::error::Error, "expected error");
        })
    }

    #[test]
    fn test_oneshot_extra_messages() {
        block_on(async {
            let (sender, mut receiver) = oneshot::<()>();
            let sender = Port::from(sender);
            assert!(futures::poll!(&mut receiver).is_pending());
            sender.send(Message::new(()));
            sender.send(Message::new(()));
            let RecvError::Error(err) = receiver.await.unwrap_err() else {
                panic!()
            };
            tracing::info!(error = &err as &dyn std::error::Error, "expected error");
        })
    }

    #[test]
    fn test_oneshot_closed() {
        block_on(async {
            let (sender, receiver) = oneshot::<()>();
            drop(sender);
            let RecvError::Closed = receiver.await.unwrap_err() else {
                panic!()
            };
        })
    }
}
