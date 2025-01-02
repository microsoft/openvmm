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
    let (sender, receiver) = oneshot_core();
    (
        OneshotSender(sender, PhantomData),
        OneshotReceiver(receiver, PhantomData),
    )
}

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
        unsafe { self.0.send(BoxedValue::new(Box::new(value))) }
    }
}

impl<T: MeshField> DefaultEncoding for OneshotSender<T> {
    type Encoding = PortField;
}

impl<T: MeshField> From<OneshotSender<T>> for Port {
    fn from(sender: OneshotSender<T>) -> Self {
        sender.into_port()
    }
}

impl<T: MeshField> From<Port> for OneshotSender<T> {
    fn from(port: Port) -> Self {
        Self::from_port(port)
    }
}

impl<T: MeshField> OneshotSender<T> {
    fn into_port(self) -> Port {
        unsafe { self.0.into_port(decode_message::<T>) }
    }

    fn from_port(port: Port) -> Self {
        Self(
            OneshotSenderCore::from_port(port, encode_message::<T>),
            PhantomData,
        )
    }
}

unsafe fn encode_message<T: MeshField>(value: BoxedValue) -> Message {
    let value = unsafe { value.cast::<T>() };
    Message::new((value,))
}

unsafe fn decode_message<T: MeshField>(message: Message) -> Result<BoxedValue, ChannelError> {
    let (value,) = message.parse::<(Box<T>,)>()?;
    Ok(unsafe { BoxedValue::new(value) })
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
            Self(ref slot) => unsafe { <*const _>::read(slot) },
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

    unsafe fn send(self, value: BoxedValue) {
        let slot = self.into_slot();
        let mut state = slot.0.lock();
        match std::mem::replace(&mut *state, SlotState::Done) {
            SlotState::ReceiverRemote(port, encode) => {
                port.send(unsafe { encode(value) });
            }
            SlotState::Waiting(waker) => {
                *state = SlotState::Sent(value);
                drop(state);
                if let Some(waker) = waker {
                    waker.wake();
                }
            }
            SlotState::Done => {}
            SlotState::Sent { .. } | SlotState::SenderRemote { .. } => unreachable!(),
        }
    }

    unsafe fn into_port(self, decode: DecodeFn) -> Port {
        let slot = self.into_slot();
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

    fn from_port(port: Port, encode: EncodeFn) -> Self {
        let slot = Arc::new(Slot(Mutex::new(SlotState::ReceiverRemote(port, encode))));
        Self(slot)
    }
}

/// The receiving half of a channel returned by [`oneshot`].
///
/// A value is received by `poll`ing or `await`ing the channel.
pub struct OneshotReceiver<T>(OneshotReceiverCore, PhantomData<Arc<Mutex<T>>>);

impl<T> OneshotReceiver<T> {
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let v = ready!(self.0.poll_recv(cx))?;
        let v = unsafe { v.cast::<T>() };
        Ok(*v).into()
    }

    fn into_core(self) -> OneshotReceiverCore {
        match *ManuallyDrop::new(self) {
            Self(ref core, _) => unsafe { <*const _>::read(core) },
        }
    }
}

impl<T> Drop for OneshotReceiver<T> {
    fn drop(&mut self) {
        // Drop the value if it exists.
        if let Some(v) = self.0.clear() {
            let _v = unsafe { v.cast::<T>() };
        }
    }
}

impl<T> Future for OneshotReceiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_recv(cx)
    }
}

impl<T: MeshField> DefaultEncoding for OneshotReceiver<T> {
    type Encoding = PortField;
}

impl<T: MeshField> From<OneshotReceiver<T>> for Port {
    fn from(receiver: OneshotReceiver<T>) -> Self {
        receiver.into_port()
    }
}

impl<T: MeshField> From<Port> for OneshotReceiver<T> {
    fn from(port: Port) -> Self {
        Self::from_port(port)
    }
}

impl<T: MeshField> OneshotReceiver<T> {
    fn into_port(self) -> Port {
        unsafe { self.into_core().into_port(encode_message::<T>) }
    }

    fn from_port(port: Port) -> Self {
        Self(
            OneshotReceiverCore::from_port(port, decode_message::<T>),
            PhantomData,
        )
    }
}

struct OneshotReceiverCore {
    slot: Option<Arc<Slot>>,
    port: Option<PortWithHandler<SlotHandler>>,
}

impl OneshotReceiverCore {
    #[must_use]
    fn clear(&mut self) -> Option<BoxedValue> {
        let slot = self.slot.take()?;
        let v = if let SlotState::Sent(value) =
            std::mem::replace(&mut *slot.0.lock(), SlotState::Done)
        {
            Some(value)
        } else {
            None
        };
        v
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<BoxedValue, RecvError>> {
        let Some(slot) = &self.slot else {
            return Poll::Ready(Err(RecvError::Closed));
        };
        let v = loop {
            let mut state = slot.0.lock();
            break match std::mem::replace(&mut *state, SlotState::Done) {
                SlotState::SenderRemote(port, decode) => {
                    *state = SlotState::Waiting(None);
                    drop(state);
                    assert!(self.port.is_none());
                    self.port = Some(port.set_handler(SlotHandler {
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
                    let err = self.port.as_ref().map_or(RecvError::Closed, |port| {
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

    unsafe fn into_port(mut self, encode: EncodeFn) -> Port {
        let existing = self.port.take().map(|port| port.remove_handler().0);
        let Some(slot) = self.slot.take() else {
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
                send.send(unsafe { encode(value) });
                if let Some(existing) = existing {
                    existing.bridge(send);
                }
                recv
            }
            SlotState::Done => existing.unwrap_or_else(|| Port::new_pair().0),
            SlotState::ReceiverRemote { .. } => unreachable!(),
        }
    }

    fn from_port(port: Port, decode: DecodeFn) -> Self {
        let slot = Arc::new(Slot(Mutex::new(SlotState::SenderRemote(port, decode))));
        Self {
            slot: Some(slot),
            port: None,
        }
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
struct BoxedValue(*mut ());

unsafe impl Send for BoxedValue {}
unsafe impl Sync for BoxedValue {}

impl BoxedValue {
    unsafe fn new<T>(value: Box<T>) -> Self {
        Self(Box::into_raw(value).cast())
    }

    unsafe fn cast<T>(self) -> Box<T> {
        unsafe { Box::from_raw(self.0.cast::<T>()) }
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
                let value = unsafe { (self.decode)(message) }.map_err(HandleMessageError::new)?;
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
