// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Contains a state unit for distributing keyboard and mouse input to the
//! appropriate devices.

use async_trait::async_trait;
use futures::StreamExt;
use futures_concurrency::stream::Merge;
use input_core::InputData;
use input_core::KeyboardData;
use input_core::MouseData;
use input_core::MultiplexedInputHandle;
use input_core::ResolvedInputSource;
use input_core::mesh_input::MeshInputSink;
use input_core::mesh_input::MeshInputSource;
use input_core::mesh_input::input_pair;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use state_unit::StateRequest;
use state_unit::StateUnit;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::KeyboardInputHandleKind;
use vm_resource::kind::MouseInputHandleKind;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// Distributes keyboard and mouse input to the appropriate devices.
pub struct InputDistributor {
    recv: mesh::Receiver<InputData>,
    client_recv: mesh::Receiver<DistributorRequest>,
    client: InputDistributorClient,
    inner: Inner,
}

#[derive(Clone)]
pub struct InputDistributorClient {
    send: mesh::Sender<DistributorRequest>,
}

enum DistributorRequest {
    AddKeyboard(Rpc<Sink<KeyboardData>, Result<(), AddSinkError>>),
    AddMouse(Rpc<Sink<MouseData>, Result<(), AddSinkError>>),
}

impl InputDistributor {
    /// Returns a new distributor for the provided input channel.
    pub fn new(input: mesh::Receiver<InputData>) -> Self {
        let (client_send, client_recv) = mesh::channel();
        Self {
            inner: Inner {
                running: false,
                keyboard: Forwarder::new(),
                mouse: Forwarder::new(),
            },
            recv: input,
            client: InputDistributorClient { send: client_send },
            client_recv,
        }
    }

    pub fn client(&self) -> &InputDistributorClient {
        &self.client
    }

    /// Returns the input channel.
    pub fn into_inner(self) -> mesh::Receiver<InputData> {
        self.recv
    }

    /// Runs the distributor.
    pub async fn run(&mut self, recv: &mut mesh::Receiver<StateRequest>) {
        enum Event {
            State(StateRequest),
            Request(DistributorRequest),
            Done,
            Input(InputData),
        }

        let mut stream = (
            recv.map(Event::State)
                .chain(futures::stream::iter([Event::Done])),
            (&mut self.recv).map(Event::Input),
            (&mut self.client_recv).map(Event::Request),
        )
            .merge();

        while let Some(event) = stream.next().await {
            match event {
                Event::State(req) => {
                    req.apply(&mut self.inner).await;
                }
                Event::Request(req) => match req {
                    DistributorRequest::AddKeyboard(rpc) => {
                        rpc.handle_sync(|sink| self.inner.keyboard.add_sink(sink))
                    }
                    DistributorRequest::AddMouse(rpc) => {
                        rpc.handle_sync(|sink| self.inner.mouse.add_sink(sink))
                    }
                },
                Event::Done => break,
                Event::Input(data) => {
                    // Drop input while the VM is paused.
                    if !self.inner.running {
                        continue;
                    }
                    match data {
                        InputData::Keyboard(input) => {
                            tracing::trace!(
                                code = input.code,
                                make = input.make,
                                "forwarding keyboard input"
                            );
                            self.inner.keyboard.forward(input)
                        }
                        InputData::Mouse(input) => {
                            tracing::trace!(
                                button_mask = input.button_mask,
                                x = input.x,
                                y = input.y,
                                "forwarding mouse input"
                            );
                            self.inner.mouse.forward(input)
                        }
                    }
                }
            }
        }
    }
}

impl InputDistributorClient {
    /// Adds a keyboard with the given name.
    ///
    /// The device with the highest elevation that is active will receive input.
    pub async fn add_keyboard(
        &self,
        name: impl Into<String>,
        elevation: usize,
    ) -> Result<MeshInputSource<KeyboardData>, AddSinkError> {
        let (source, sink) = input_pair();
        // Treat a missing distributor as success.
        self.send
            .call(
                DistributorRequest::AddKeyboard,
                Sink {
                    name: name.into(),
                    elevation,
                    sink,
                },
            )
            .await
            .unwrap_or(Ok(()))?;

        Ok(source)
    }

    /// Adds a mouse with the given name. Returns an input channel and a cell
    /// that can be set to make the device active or not.
    ///
    /// The device with the highest elevation that is active will receive input.
    pub async fn add_mouse(
        &self,
        name: impl Into<String>,
        elevation: usize,
    ) -> Result<MeshInputSource<MouseData>, AddSinkError> {
        let (source, sink) = input_pair();
        // Treat a missing distributor as success.
        self.send
            .call(
                DistributorRequest::AddMouse,
                Sink {
                    name: name.into(),
                    elevation,
                    sink,
                },
            )
            .await
            .unwrap_or(Ok(()))?;

        Ok(source)
    }
}

#[derive(InspectMut)]
struct Inner {
    running: bool,
    keyboard: Forwarder<KeyboardData>,
    mouse: Forwarder<MouseData>,
}

impl StateUnit for Inner {
    async fn start(&mut self) {
        self.running = true;
    }

    async fn stop(&mut self) {
        self.running = false;
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        Ok(None)
    }

    async fn restore(&mut self, _buffer: SavedStateBlob) -> Result<(), RestoreError> {
        Err(RestoreError::SavedStateNotSupported)
    }
}

struct Forwarder<T> {
    /// Sorted by elevation.
    sinks: Vec<Sink<T>>,
}

impl<T: 'static + Send> Inspect for Forwarder<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for sink in &self.sinks {
            resp.field(&sink.elevation.to_string(), sink);
        }
    }
}

struct Sink<T> {
    elevation: usize,
    name: String,
    sink: MeshInputSink<T>,
}

impl<T: 'static + Send> Inspect for Sink<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("name", &self.name)
            .field("active", self.sink.is_active());
    }
}

#[derive(Debug, Error)]
#[error("new input sink '{name}' at elevation {elevation} conflicts with '{other}'")]
pub struct AddSinkError {
    name: String,
    elevation: usize,
    other: String,
}

impl<T: 'static + Send> Forwarder<T> {
    fn new() -> Self {
        Self { sinks: Vec::new() }
    }

    fn add_sink(&mut self, sink: Sink<T>) -> Result<(), AddSinkError> {
        // Insert the sink to keep the list ordered.
        let i = match self
            .sinks
            .binary_search_by(|other| other.elevation.cmp(&sink.elevation))
        {
            Err(i) => i,
            Ok(i) => {
                let other = &self.sinks[i];
                return Err(AddSinkError {
                    name: sink.name,
                    elevation: sink.elevation,
                    other: other.name.clone(),
                });
            }
        };
        self.sinks.insert(i, sink);
        Ok(())
    }

    fn forward(&mut self, t: T) {
        for sink in self.sinks.iter_mut().rev() {
            if sink.sink.is_active() {
                sink.sink.send(t);
                break;
            }
        }
    }
}

#[async_trait]
impl AsyncResolveResource<KeyboardInputHandleKind, MultiplexedInputHandle>
    for InputDistributorClient
{
    type Output = ResolvedInputSource<KeyboardData>;
    type Error = AddSinkError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: MultiplexedInputHandle,
        input: &str,
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.add_keyboard(input, resource.elevation).await?.into())
    }
}

#[async_trait]
impl AsyncResolveResource<MouseInputHandleKind, MultiplexedInputHandle> for InputDistributorClient {
    type Output = ResolvedInputSource<MouseData>;
    type Error = AddSinkError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: MultiplexedInputHandle,
        input: &str,
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.add_mouse(input, resource.elevation).await?.into())
    }
}
