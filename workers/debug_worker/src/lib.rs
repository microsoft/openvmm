// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A worker which runs a gdbstub event loop.
//!
//! Implements [`DebuggerWorker`], which exposes control of the VM via the GDB
//! Remote Serial Protocol. This is used to debug the VM's execution when a
//! guest debugger is not available or practical.

#![expect(missing_docs)]

mod gdb;

use anyhow::Context;
use debug_worker_defs::DEBUGGER_WORKER;
use debug_worker_defs::DebuggerParameters;
use futures::AsyncReadExt;
use futures::FutureExt;
use gdb::VmProxy;
use gdb::targets::TargetArch;
use gdb::targets::VmTarget;
use gdbstub::target::ext::breakpoints::WatchKind;
use inspect::InspectMut;
use mesh::message::MeshField;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use pal_async::local::LocalDriver;
use pal_async::local::block_with_io;
use pal_async::socket::Listener;
use pal_async::socket::PolledSocket;
use socket2::Socket;
use std::fmt::Display;
use std::io::Write;
use std::net::TcpListener;
use std::pin::Pin;
use vmm_core_defs::debug_rpc::BreakpointType;
use vmm_core_defs::debug_rpc::DebugRequest;
use vmm_core_defs::debug_rpc::DebugStopReason;

pub struct DebuggerWorker<T: Listener> {
    listener: T,
    state: State<T::Address>,
    initial_arch: Architecture,
}

/// The current server state.
enum State<T> {
    Listening {
        vm_proxy: VmProxy,
    },
    Connected {
        remote_addr: T,
        task: Pin<Box<dyn Future<Output = VmProxy>>>,
        abort: mesh::OneshotSender<()>,
    },
    Invalid,
}

trait GdbListener: 'static + Send + Listener + Sized + MeshField {
    const ID: WorkerId<DebuggerParameters<Self>>;
}

impl GdbListener for TcpListener {
    const ID: WorkerId<DebuggerParameters<Self>> = DEBUGGER_WORKER;
}

#[cfg(any(windows, target_os = "linux"))]
impl GdbListener for vmsocket::VmListener {
    const ID: WorkerId<DebuggerParameters<Self>> = debug_worker_defs::DEBUGGER_VSOCK_WORKER;
}

impl<T: GdbListener> Worker for DebuggerWorker<T>
where
    T::Address: Display,
{
    type Parameters = DebuggerParameters<T>;
    type State = DebuggerParameters<T>;
    const ID: WorkerId<Self::Parameters> = T::ID;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        Ok(Self {
            listener: params.listener,
            state: State::Listening {
                vm_proxy: VmProxy::new(params.req_chan, params.vp_count),
            },
            initial_arch: match params.target_arch {
                debug_worker_defs::TargetArch::X86_64 => Architecture::X86_64,
                debug_worker_defs::TargetArch::I8086 => Architecture::I8086,
                debug_worker_defs::TargetArch::Aarch64 => Architecture::Aarch64,
            },
        })
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        Self::new(state)
    }

    fn run(self, mut rpc_recv: mesh::Receiver<WorkerRpc<Self::Parameters>>) -> anyhow::Result<()> {
        block_with_io(async |driver| {
            tracing::info!(
                address = %self.listener.local_addr().unwrap(),
                "gdbstub listening",
            );

            let listener = PolledSocket::new(&driver, self.listener)?;
            let mut server = Server {
                listener,
                state: self.state,
                architecture: self.initial_arch,
            };

            loop {
                let r = futures::select! { // merge semantics
                    r = rpc_recv.recv().fuse() => r,
                    r = server.process(&driver).fuse() => {
                        r?;
                        return Ok(())
                    },
                };
                match r {
                    Ok(message) => match message {
                        WorkerRpc::Stop => return Ok(()),
                        WorkerRpc::Inspect(deferred) => deferred.inspect(&mut server),
                        WorkerRpc::Restart(rpc) => {
                            let vm_proxy = match server.state {
                                State::Listening { vm_proxy } => vm_proxy,
                                State::Connected { task, abort, .. } => {
                                    drop(abort);
                                    task.await
                                }
                                State::Invalid => unreachable!(),
                            };

                            let state = {
                                let (req_chan, vp_count) = vm_proxy.into_params();
                                DebuggerParameters {
                                    listener: server.listener.into_inner(),
                                    req_chan,
                                    vp_count,
                                    target_arch: match server.architecture {
                                        Architecture::X86_64 => {
                                            debug_worker_defs::TargetArch::X86_64
                                        }
                                        Architecture::I8086 => debug_worker_defs::TargetArch::I8086,
                                        Architecture::Aarch64 => {
                                            debug_worker_defs::TargetArch::Aarch64
                                        }
                                    },
                                }
                            };
                            rpc.complete(Ok(state));
                            return Ok(());
                        }
                    },
                    Err(_) => return Ok(()),
                }
            }
        })
    }
}

struct Server<T: Listener> {
    listener: PolledSocket<T>,
    state: State<T::Address>,
    architecture: Architecture,
}

#[derive(Debug, Copy, Clone, InspectMut)]
enum Architecture {
    #[inspect(rename = "x86_64")]
    X86_64,
    #[inspect(rename = "i8086")]
    I8086,
    Aarch64,
}

impl<T: Listener> Server<T>
where
    T::Address: Display,
{
    /// Runs the state machine forward, either advancing the current connection
    /// task or waiting for a new connection.
    async fn process(&mut self, driver: &LocalDriver) -> anyhow::Result<()> {
        loop {
            match &mut self.state {
                State::Listening { .. } => {
                    // Accept the connection if one is really ready.
                    let (socket, remote_addr) = self.listener.accept().await?;
                    let socket = PolledSocket::new(driver, socket.into())?;

                    let architecture = self.architecture;
                    tracing::info!(address = %remote_addr, ?architecture, "GDB client connected");

                    let mut vm_proxy = if let State::Listening { vm_proxy } =
                        std::mem::replace(&mut self.state, State::Invalid)
                    {
                        vm_proxy
                    } else {
                        unreachable!()
                    };

                    let (abort_send, abort_recv) = mesh::oneshot();
                    let connection = Box::pin(async move {
                        let state_machine_fut = async {
                            match architecture {
                                Architecture::X86_64 => {
                                    run_state_machine(
                                        socket,
                                        VmTarget::<gdb::arch::x86::X86_64_QEMU>::new(&mut vm_proxy),
                                    )
                                    .await
                                }
                                Architecture::I8086 => {
                                    run_state_machine(
                                        socket,
                                        VmTarget::<gdb::arch::x86::I8086>::new(&mut vm_proxy),
                                    )
                                    .await
                                }
                                Architecture::Aarch64 => {
                                    run_state_machine(
                                        socket,
                                        VmTarget::<gdbstub_arch::aarch64::AArch64>::new(
                                            &mut vm_proxy,
                                        ),
                                    )
                                    .await
                                }
                            }
                        };

                        let res = futures::select! { // race semantics
                            gdb_res = state_machine_fut.fuse() => Some(gdb_res),
                            _ = abort_recv.fuse() => None,
                        };

                        match res {
                            Some(gdb_res) => {
                                if let Err(err) = gdb_res {
                                    tracing::error!(
                                        error = (&err) as &dyn std::error::Error,
                                        "gdbstub error"
                                    );
                                }
                            }
                            None => {
                                tracing::info!("Aborting existing GDB worker...");
                            }
                        }

                        vm_proxy
                    });

                    self.state = State::Connected {
                        remote_addr,
                        task: connection,
                        abort: abort_send,
                    };
                }
                State::Connected { task, .. } => {
                    let vm_proxy = task.await;
                    self.state = State::Listening { vm_proxy };
                }
                State::Invalid => unreachable!(),
            }
        }
    }
}

async fn run_state_machine<T: TargetArch>(
    socket: PolledSocket<Socket>,
    mut vm_target: VmTarget<'_, T>,
) -> Result<(), gdbstub::stub::GdbStubError<anyhow::Error, std::io::Error>> {
    use gdbstub::common::Signal;
    use gdbstub::stub::DisconnectReason;
    use gdbstub::stub::GdbStubError;
    use gdbstub::stub::MultiThreadStopReason;
    use gdbstub::stub::state_machine::GdbStubStateMachine;

    vm_target.send_req(DebugRequest::Attach);
    let (init_break_send, init_break_recv) = mesh::oneshot();
    vm_target.send_req(DebugRequest::Resume {
        response: init_break_send,
    });
    vm_target.send_req(DebugRequest::Break);

    // Wait for the initial break.
    let reason = init_break_recv
        .await
        .context("failed to wait for initial break")
        .map_err(GdbStubError::TargetError)?;

    tracing::info!(?reason, "got initial breakpoint");

    let mut gdb =
        gdbstub::stub::GdbStub::new(SocketConnection(socket)).run_state_machine(&mut vm_target)?;

    let reason = loop {
        gdb = match gdb {
            GdbStubStateMachine::Idle(mut gdb) => {
                // "blocking" read waiting for GDB to send a command
                let mut b = [0];
                gdb.borrow_conn()
                    .0
                    .read_exact(&mut b)
                    .await
                    .map_err(GdbStubError::ConnectionRead)?;

                gdb.incoming_data(&mut vm_target, b[0])?
            }

            GdbStubStateMachine::Disconnected(gdb) => {
                break gdb.get_reason();
            }

            GdbStubStateMachine::CtrlCInterrupt(gdb) => {
                vm_target.send_req(DebugRequest::Break);

                let stop_reason = Some(MultiThreadStopReason::Signal(Signal::SIGINT));
                gdb.interrupt_handled(&mut vm_target, stop_reason)?
            }

            GdbStubStateMachine::Running(mut gdb) => {
                enum Event {
                    HaltReason(DebugStopReason),
                    IncomingData(u8),
                }

                let stop_chan = vm_target
                    .take_stop_chan()
                    .expect("halt chan is set as part of `resume`");

                let mut b = [0];
                let incoming_data = gdb.borrow_conn().0.read_exact(&mut b);

                let event = futures::select! { // race semantics
                    r = stop_chan.fuse() => {
                        let reason = r.map_err(|e| GdbStubError::TargetError(e.into()))?;
                        Event::HaltReason(reason)
                    },
                    _ = incoming_data.fuse() => Event::IncomingData(b[0]),
                };

                match event {
                    Event::IncomingData(b) => gdb.incoming_data(&mut vm_target, b)?,
                    Event::HaltReason(reason) => {
                        let stop_reason = match reason {
                            DebugStopReason::Break => MultiThreadStopReason::Signal(Signal::SIGINT),
                            DebugStopReason::PowerOff => MultiThreadStopReason::Exited(0),
                            DebugStopReason::Reset => MultiThreadStopReason::Exited(1),
                            DebugStopReason::TripleFault { vp } => {
                                MultiThreadStopReason::SignalWithThread {
                                    tid: vm_target.vp_to_tid(vp),
                                    signal: Signal::SIGSEGV,
                                }
                            }
                            DebugStopReason::HwBreakpoint { vp, breakpoint } => {
                                if let Ok(address) = T::Address::try_from(breakpoint.address) {
                                    match breakpoint.ty {
                                        BreakpointType::Execute => {
                                            MultiThreadStopReason::HwBreak(vm_target.vp_to_tid(vp))
                                        }
                                        BreakpointType::Invalid => {
                                            tracing::error!(
                                                address = breakpoint.address,
                                                "invalid breakpoint type"
                                            );
                                            MultiThreadStopReason::Signal(Signal::SIGINT)
                                        }
                                        BreakpointType::Write => MultiThreadStopReason::Watch {
                                            tid: vm_target.vp_to_tid(vp),
                                            kind: WatchKind::Write,
                                            addr: address,
                                        },
                                        BreakpointType::ReadOrWrite => {
                                            MultiThreadStopReason::Watch {
                                                tid: vm_target.vp_to_tid(vp),
                                                kind: WatchKind::ReadWrite,
                                                addr: address,
                                            }
                                        }
                                    }
                                } else {
                                    tracing::error!(
                                        address = breakpoint.address,
                                        "breakpoint address out of range"
                                    );
                                    MultiThreadStopReason::Signal(Signal::SIGINT)
                                }
                            }
                            DebugStopReason::SingleStep { vp } => {
                                // Work around WinDbg client limitation
                                MultiThreadStopReason::SignalWithThread {
                                    tid: vm_target.vp_to_tid(vp),
                                    signal: Signal::SIGTRAP,
                                }
                            }
                        };

                        gdb.report_stop(&mut vm_target, stop_reason)?
                    }
                }
            }
        }
    };

    match reason {
        DisconnectReason::Disconnect => tracing::info!("GDB Disconnected"),
        DisconnectReason::TargetExited(status_code) => {
            tracing::info!(status_code, "Target exited")
        }
        DisconnectReason::TargetTerminated(signal) => {
            tracing::info!(signal = signal.to_string().as_str(), "Target terminated")
        }
        DisconnectReason::Kill => tracing::info!("GDB sent a kill command"),
    }

    Ok(())
}

impl<T: Listener> InspectMut for Server<T>
where
    T::Address: Display,
{
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.display("local_addr", &self.listener.get().local_addr().unwrap());
        let state = match &self.state {
            State::Listening { .. } => "listening",
            State::Connected { remote_addr, .. } => {
                resp.display("remote_addr", remote_addr);
                "connected"
            }
            State::Invalid => unreachable!(),
        };
        resp.field("state", state)
            .field_mut("architecture", &mut self.architecture);
    }
}

struct SocketConnection(PolledSocket<Socket>);

impl gdbstub::conn::Connection for SocketConnection {
    type Error = std::io::Error;

    fn write(&mut self, byte: u8) -> Result<(), Self::Error> {
        self.0.get_mut().write_all(&[byte])
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.get_mut().flush()
    }
}
