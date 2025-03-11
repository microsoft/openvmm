// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh-backed [`InputSource`] implementation.

use crate::InputSource;
use mesh::MeshPayload;
use mesh::message::MeshField;
use std::pin::Pin;

/// An input source that receives input over a mesh channel.
#[derive(MeshPayload)]
#[mesh(bound = "T: 'static + MeshField + Send")]
pub struct MeshInputSource<T> {
    recv: mesh::Receiver<T>,
    active: mesh::CellUpdater<bool>,
}

impl<T: 'static + Send> InputSource<T> for MeshInputSource<T> {
    fn set_active(&mut self, active: bool) -> Pin<Box<dyn '_ + Future<Output = ()> + Send>> {
        Box::pin(async move {
            if *self.active.get() != active {
                self.active.set(active).await;
            }
        })
    }
}

impl<T: 'static + Send> futures::Stream for MeshInputSource<T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.recv).poll_next(cx)
    }
}

/// The sending side of the [`MeshInputSource`].
#[derive(MeshPayload)]
#[mesh(bound = "T: 'static + MeshField + Send")]
pub struct MeshInputSink<T> {
    send: mesh::Sender<T>,
    active: mesh::Cell<bool>,
}

impl<T: 'static + Send> MeshInputSink<T> {
    /// Sends an input message to the matching input source.
    pub fn send(&mut self, input: T) {
        self.send.send(input);
    }

    /// Returns true if the matching input source is currently active.
    pub fn is_active(&self) -> bool {
        self.active.get()
    }
}

/// Returns a new input source/sink pair.
pub fn input_pair<T: 'static + Send>() -> (MeshInputSource<T>, MeshInputSink<T>) {
    let (send, recv) = mesh::channel();
    let (update, active) = mesh::cell(false);
    (
        MeshInputSource {
            recv,
            active: update,
        },
        MeshInputSink { send, active },
    )
}
