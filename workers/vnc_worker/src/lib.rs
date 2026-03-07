// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A worker for running a VNC server.

#![forbid(unsafe_code)]

use anyhow::Context;
use anyhow::anyhow;
use futures::FutureExt;
use input_core::InputData;
use input_core::KeyboardData;
use input_core::MouseData;
use mesh::message::MeshField;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use pal_async::local::LocalDriver;
use pal_async::local::block_with_io;
use pal_async::socket::Listener;
use pal_async::socket::PolledSocket;
use pal_async::timer::PolledTimer;
use std::future::Future;
use std::net::TcpListener;
use std::pin::Pin;
use std::time::Duration;
use tracing_helpers::AnyhowValueExt;
use vnc_worker_defs::VncParameters;

/// A worker for running a VNC server.
pub struct VncWorker<T: Listener> {
    listener: T,
    state: State<T>,
}

/// The current server state.
enum State<T: Listener> {
    Listening {
        view: ViewWrapper,
        input: VncInput,
    },
    Connected {
        remote_addr: T::Address,
        task: Pin<Box<dyn Future<Output = (ViewWrapper, VncInput)>>>,
        abort: mesh::OneshotSender<()>,
    },
    Invalid,
}

impl Worker for VncWorker<TcpListener> {
    type Parameters = VncParameters<TcpListener>;
    type State = VncParameters<TcpListener>;
    const ID: WorkerId<Self::Parameters> = vnc_worker_defs::VNC_WORKER_TCP;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        Self::new_inner(params)
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        Self::new(state)
    }

    fn run(self, rpc_recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        self.run_inner(rpc_recv)
    }
}

#[cfg(any(windows, target_os = "linux"))]
impl Worker for VncWorker<vmsocket::VmListener> {
    type Parameters = VncParameters<vmsocket::VmListener>;
    type State = VncParameters<vmsocket::VmListener>;
    const ID: WorkerId<Self::Parameters> = vnc_worker_defs::VNC_WORKER_VMSOCKET;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        Self::new_inner(params)
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        Self::new(state)
    }

    fn run(self, rpc_recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        self.run_inner(rpc_recv)
    }
}

impl<T: 'static + Listener + MeshField + Send> VncWorker<T> {
    fn new_inner(params: VncParameters<T>) -> anyhow::Result<Self> {
        Ok(Self {
            listener: params.listener,
            state: State::Listening {
                view: ViewWrapper(
                    params
                        .framebuffer
                        .view()
                        .context("failed to map framebuffer")?,
                ),
                input: VncInput {
                    send: params.input_send,
                },
            },
        })
    }

    fn run_inner(
        self,
        mut rpc_recv: mesh::Receiver<WorkerRpc<VncParameters<T>>>,
    ) -> anyhow::Result<()> {
        block_with_io(async |driver| {
            tracing::info!(
                address = ?self.listener.local_addr().unwrap(),
                "VNC server listening",
            );

            let listener = PolledSocket::new(&driver, self.listener)?;
            let mut server = Server {
                listener,
                state: self.state,
            };

            let rpc = loop {
                let r = futures::select! { // merge semantics
                    r = rpc_recv.recv().fuse() => r,
                    r = server.process(&driver).fuse() => break r.map(|_| None)?,
                };
                match r {
                    Ok(message) => match message {
                        WorkerRpc::Stop => break None,
                        WorkerRpc::Inspect(deferred) => deferred.inspect(&server),
                        WorkerRpc::Restart(response) => break Some(response),
                    },
                    Err(_) => break None,
                }
            };
            if let Some(rpc) = rpc {
                let (view, input) = match server.state {
                    State::Listening { view, input } => (view, input),
                    State::Connected { task, abort, .. } => {
                        drop(abort);
                        task.await
                    }
                    State::Invalid => unreachable!(),
                };
                let state = VncParameters {
                    listener: server.listener.into_inner(),
                    framebuffer: view.0.access(),
                    input_send: input.send,
                };
                rpc.complete(Ok(state));
            }
            Ok(())
        })
    }
}

struct Server<T: Listener> {
    listener: PolledSocket<T>,
    state: State<T>,
}

impl<T: Listener> Server<T> {
    /// Runs the state machine forward, either advancing the current connection
    /// task or waiting for a new connection.
    ///
    /// This function's future can be dropped safely at any time without losing
    /// any data or connections.
    async fn process(&mut self, driver: &LocalDriver) -> anyhow::Result<()> {
        loop {
            match &mut self.state {
                State::Listening { .. } => {
                    // Accept the connection if one is really ready.
                    let (socket, remote_addr) = self.listener.accept().await?;
                    let socket = PolledSocket::new(driver, socket.into())?;

                    tracing::info!(address = ?remote_addr, "VNC client connected");

                    let (view, input) = if let State::Listening { view, input } =
                        std::mem::replace(&mut self.state, State::Invalid)
                    {
                        (view, input)
                    } else {
                        unreachable!()
                    };

                    let mut vncserver = vnc::Server::new("OpenVMM VM".into(), socket, view, input);
                    let mut timer = PolledTimer::new(driver);

                    let (abort_send, abort_recv) = mesh::oneshot();
                    let connection = Box::pin(async move {
                        let updater = vncserver.updater();
                        let update_task = async {
                            // For now, just mark the framebuffer as updated
                            // every 30ms (about 30 frames per second).
                            loop {
                                timer.sleep(Duration::from_millis(30)).await;
                                updater.update();
                            }
                        };
                        let r = futures::select! { // race semantics
                            r = vncserver.run().fuse() => r.context("VNC error"),
                            _ = abort_recv.fuse() => Err(anyhow!("VNC connection aborted")),
                            _ = update_task.fuse() => unreachable!(),
                        };
                        match r {
                            Ok(_) => {
                                tracing::info!("VNC client disconnected");
                            }
                            Err(err) => {
                                tracing::error!(error = err.as_error(), "VNC client error");
                            }
                        }
                        vncserver.done()
                    });
                    self.state = State::Connected {
                        remote_addr,
                        task: connection,
                        abort: abort_send,
                    };
                }
                State::Connected { task, .. } => {
                    let (view, input) = task.await;
                    self.state = State::Listening { view, input };
                }
                State::Invalid => unreachable!(),
            }
        }
    }
}

impl<T: Listener> inspect::Inspect for Server<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.display_debug("local_addr", &self.listener.get().local_addr().unwrap());
        let state = match &self.state {
            State::Listening { .. } => "listening",
            State::Connected { remote_addr, .. } => {
                resp.display_debug("remote_addr", &remote_addr);
                "connected"
            }
            State::Invalid => unreachable!(),
        };
        resp.field("state", state);
    }
}

struct VncInput {
    send: mesh::Sender<InputData>,
}

impl vnc::Input for VncInput {
    fn key(&mut self, scancode: u16, is_down: bool) {
        // TODO: need some kind of backpressure
        self.send.send(InputData::Keyboard(KeyboardData {
            code: scancode,
            make: is_down,
        }));
    }

    fn mouse(&mut self, button_mask: u8, x: u16, y: u16) {
        self.send
            .send(InputData::Mouse(MouseData { button_mask, x, y }));
    }
}

struct ViewWrapper(framebuffer::View);

impl vnc::Framebuffer for ViewWrapper {
    fn read_line(&mut self, line: u16, data: &mut [u8]) {
        self.0.read_line(line, data)
    }

    fn resolution(&mut self) -> (u16, u16) {
        self.0.resolution()
    }
}
