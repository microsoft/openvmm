// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An implementation of a cell type that can be updated from a remote mesh
//! node.

use super::bidir::Channel;
use mesh_node::local_node::HandleMessageError;
use mesh_node::local_node::HandlePortEvent;
use mesh_node::local_node::NodeError;
use mesh_node::local_node::Port;
use mesh_node::local_node::PortControl;
use mesh_node::local_node::PortWithHandler;
use mesh_node::message::MeshField;
use mesh_node::message::Message;
use mesh_node::resource::Resource;
use mesh_protobuf::EncodeAs;
use mesh_protobuf::Protobuf;
use mesh_protobuf::SerializedMessage;
use std::future::poll_fn;
use std::future::Future;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

/// A cell containing a value that can be updated from a remote node.
///
/// Created by [`cell()`].
#[derive(Debug, Clone, Protobuf)]
#[mesh(bound = "T: MeshField", resource = "Resource")]
pub struct Cell<T: MeshField + Sync + Clone>(EncodeAs<Inner<T>, EncodedCell<T>>);

#[derive(Debug)]
struct Inner<T> {
    port: PortWithHandler<State<T>>,
    last_id: u64,
}

#[derive(Debug)]
struct State<T> {
    id: u64,
    value: T,
    waker: Option<Waker>,
}

#[derive(Protobuf)]
#[mesh(resource = "Resource")]
struct EncodedCell<T> {
    id: u64,
    value: T,
    port: Port,
}

/// A type used to update the value in one or more [`Cell`]s.
#[derive(Debug, Protobuf)]
#[mesh(resource = "Resource")]
pub struct CellUpdater<T> {
    value: T,
    current_id: u64,
    ports: Vec<(u64, Channel)>,
}

impl<T: Clone + MeshField + Sync> CellUpdater<T> {
    /// Creates a new cell updater with no associated cells.
    pub fn new(value: T) -> Self {
        Self {
            value,
            current_id: 0,
            ports: Vec::new(),
        }
    }

    /// Creates a new associated cell.
    pub fn cell(&mut self) -> Cell<T> {
        let (recv, send) = Port::new_pair();
        send.send(Message::new(UpdateMessage {
            id: self.current_id,
            value: self.value.clone(),
        }));
        self.ports.push((self.current_id, send.into()));
        Cell(EncodeAs::new(Inner::from_parts(
            self.current_id,
            self.value.clone(),
            recv,
        )))
    }

    /// Gets the current value.
    pub fn get(&self) -> &T {
        &self.value
    }

    /// Asynchronously updates the value in the associated cells.
    pub fn set(&mut self, value: T) -> impl '_ + Future<Output = ()> + Unpin {
        self.send_value(value);
        self.process_incoming()
    }

    fn send_value(&mut self, value: T) {
        self.value = value;
        self.current_id += 1;
        for (_, port) in self.ports.iter_mut() {
            port.send(SerializedMessage::from_message(UpdateMessage {
                id: self.current_id,
                value: self.value.clone(),
            }));
        }
    }

    fn poll_one(&mut self, cx: &mut Context<'_>, i: usize) -> Poll<bool> {
        loop {
            let (id, port) = &mut self.ports[i];
            if *id >= self.current_id {
                break Poll::Ready(true);
            }
            let message = std::task::ready!(port.poll_recv(cx));
            let message = message.ok().and_then(|m| m.into_message().ok());
            match message {
                Some(message) => match message {
                    UpdateResponse::NewPort(new_id, new_port) => {
                        if new_id < self.current_id {
                            // This port has a stale value. Send it the new
                            // value. We'll wait for its response in a
                            // subsequent call.
                            new_port.send(Message::new(UpdateMessage {
                                id: self.current_id,
                                value: self.value.clone(),
                            }));
                        }
                        self.ports.push((new_id, new_port.into()));
                    }
                    UpdateResponse::Updated(new_id) => {
                        if new_id > *id {
                            *id = new_id;
                        }
                    }
                },

                None => {
                    break Poll::Ready(false);
                }
            }
        }
    }

    fn process_incoming(&mut self) -> impl '_ + Future<Output = ()> + Unpin {
        poll_fn(|cx| {
            let mut wait = false;
            let mut i = 0;
            while i < self.ports.len() {
                match self.poll_one(cx, i) {
                    Poll::Ready(true) => i += 1,
                    Poll::Ready(false) => {
                        self.ports.swap_remove(i);
                    }
                    Poll::Pending => {
                        i += 1;
                        wait = true;
                    }
                }
            }
            if wait {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
    }
}

/// Creates a new cell and its associated updater.
///
/// Both the cell and the updater can be sent to remote processes via mesh channels.
///
/// ```rust
/// # use mesh_channel::cell::cell;
/// # use futures::executor::block_on;
/// let (mut updater, cell) = cell::<u32>(5);
/// assert_eq!(cell.get(), 5);
/// block_on(updater.set(6));
/// assert_eq!(cell.get(), 6);
/// ```
pub fn cell<T: MeshField + Sync + Clone>(value: T) -> (CellUpdater<T>, Cell<T>) {
    let mut updater = CellUpdater::new(value);
    let cell = updater.cell();
    (updater, cell)
}

impl<T: MeshField + Sync + Clone> Clone for Inner<T> {
    fn clone(&self) -> Self {
        let (left, right) = Port::new_pair();
        // Hold the lock for the whole operation to ensure the new port message
        // is sent before the update callback sends the update response;
        // otherwise, the updater will fail to see that there is a new port with
        // a stale value.
        let (id, value) = self.port.with_port_and_handler(|control, state| {
            let id = state.id;
            let value = state.value.clone();
            control.respond(Message::new(UpdateResponse::NewPort(id, left)));
            (id, value)
        });
        Self::from_parts(id, value, right)
    }
}

impl<T: MeshField + Sync + Clone> Cell<T> {
    /// Gets a clone of the cell's current value.
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.0.port.with_handler(|state| state.value.clone())
    }

    /// Runs `f` with a reference to the cell's current value.
    ///
    /// While `f` is running, updates to the cell's value will not be
    /// acknowledged (and the remote updater's `set` method will block).
    pub fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        self.0.port.with_handler(|state| f(&state.value))
    }

    /// Runs `f` with a mutable reference to the cell's current value.
    ///
    /// While `f` is running, updates to the cell's value will not be
    /// acknowledged (and the remote updater's `set` method will block).
    pub fn with_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        self.0.port.with_handler(|state| f(&mut state.value))
    }

    /// Waits for a new value to be set.
    pub async fn wait_next(&mut self) {
        poll_fn(|cx| {
            let mut old_waker = None;
            let inner = &mut *self.0;
            inner.port.with_handler(|state| {
                if inner.last_id == state.id {
                    old_waker = state.waker.replace(cx.waker().clone());
                    return Poll::Pending;
                }
                inner.last_id = state.id;
                Poll::Ready(())
            })
        })
        .await
    }
}

#[derive(Protobuf)]
#[mesh(resource = "Resource")]
struct UpdateMessage<T> {
    value: T,
    id: u64,
}

#[derive(Protobuf)]
#[mesh(resource = "Resource")]
enum UpdateResponse {
    Updated(u64),
    NewPort(u64, Port),
}

impl<T: MeshField + Sync> HandlePortEvent for State<T> {
    fn message(
        &mut self,
        control: &mut PortControl<'_>,
        message: Message,
    ) -> Result<(), HandleMessageError> {
        let UpdateMessage::<T> { id, value } = message.parse().map_err(HandleMessageError::new)?;
        if self.id < id {
            self.id = id;
            self.value = value;
            if let Some(waker) = self.waker.take() {
                control.wake(waker);
            }
            control.respond(Message::new(UpdateResponse::Updated(id)));
        }
        Ok(())
    }

    fn close(&mut self, _control: &mut PortControl<'_>) {}

    fn fail(&mut self, _control: &mut PortControl<'_>, _err: NodeError) {}

    fn drain(&mut self) -> Vec<Message> {
        Vec::new()
    }
}

impl<T: MeshField + Sync> Inner<T> {
    fn from_parts(id: u64, value: T, port: Port) -> Self {
        let state = State {
            id,
            value,
            waker: None,
        };
        Self {
            port: port.set_handler(state),
            last_id: id,
        }
    }

    fn into_parts(self) -> (u64, T, Port) {
        let (port, state) = self.port.remove_handler();
        (state.id, state.value, port)
    }
}

impl<T: MeshField + Sync + Clone> From<Inner<T>> for EncodedCell<T> {
    fn from(cell: Inner<T>) -> Self {
        let (id, value, port) = cell.into_parts();
        Self { id, value, port }
    }
}

impl<T: MeshField + Sync + Clone> From<EncodedCell<T>> for Inner<T> {
    fn from(encoded: EncodedCell<T>) -> Self {
        Inner::from_parts(encoded.id, encoded.value, encoded.port)
    }
}

#[cfg(test)]
mod tests {
    use super::CellUpdater;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::DefaultDriver;
    use std::future::poll_fn;
    use std::task::Poll;

    #[async_test]
    async fn cell() {
        let (mut updater, cell) = super::cell("hey".to_string());
        updater.set("hello".to_string()).await;
        cell.with(|val| assert_eq!(&val, &"hello"));
    }

    #[async_test]
    async fn multi_cell() {
        let mut updater = CellUpdater::new(0);
        let c1 = updater.cell();
        let c2 = updater.cell();
        let c3 = updater.cell();
        let c4 = c3.clone();
        updater.set(5).await;
        let c5 = updater.cell();
        let c6 = c4.clone();
        assert_eq!(c1.get(), 5);
        assert_eq!(c2.get(), 5);
        assert_eq!(c3.get(), 5);
        assert_eq!(c4.get(), 5);
        assert_eq!(c5.get(), 5);
        assert_eq!(c6.get(), 5);
    }

    #[async_test]
    async fn wait_next(driver: DefaultDriver) {
        let mut updater = CellUpdater::new(0);
        let mut c = updater.cell();
        for i in 1..100 {
            let t = driver.spawn("test", async {
                c.wait_next().await;
                c
            });

            // Yield so that `t` runs until it blocks.
            let mut yielded = false;
            poll_fn(|cx| {
                if yielded {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    yielded = true;
                    Poll::Pending
                }
            })
            .await;

            drop(updater.set(i));
            c = t.await;
            assert_eq!(c.get(), i);
        }
    }
}
