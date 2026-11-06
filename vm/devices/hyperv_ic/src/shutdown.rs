// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The shutdown IC.

use crate::common::IcPipe;
use crate::common::NegotiateState;
use crate::common::Versions;
use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::once;
use futures_concurrency::stream::Merge;
use hyperv_ic_protocol::Status;
use hyperv_ic_protocol::shutdown::SHUTDOWN_VERSION_1;
use hyperv_ic_protocol::shutdown::SHUTDOWN_VERSION_3;
use hyperv_ic_protocol::shutdown::SHUTDOWN_VERSION_3_1;
use hyperv_ic_protocol::shutdown::SHUTDOWN_VERSION_3_2;
use hyperv_ic_resources::shutdown::ShutdownParams;
use hyperv_ic_resources::shutdown::ShutdownResult;
use hyperv_ic_resources::shutdown::ShutdownRpc;
use hyperv_ic_resources::shutdown::ShutdownType;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::Rpc;
use std::pin::pin;
use task_control::Cancelled;
use task_control::StopTask;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SaveRestoreSimpleVmbusDevice;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_ring::RingMem;
use zerocopy::IntoBytes;

const SHUTDOWN_VERSIONS: &[hyperv_ic_protocol::Version] = &[
    SHUTDOWN_VERSION_1,
    SHUTDOWN_VERSION_3,
    SHUTDOWN_VERSION_3_1,
    SHUTDOWN_VERSION_3_2,
];

/// A shutdown IC device.
#[derive(InspectMut)]
pub struct ShutdownIc {
    #[inspect(skip)]
    recv: mesh::Receiver<ShutdownRpc>,
    #[inspect(skip)]
    wait_ready: Vec<Rpc<(), mesh::OneshotReceiver<()>>>,
}

#[doc(hidden)]
#[derive(InspectMut)]
pub struct ShutdownChannel<M: RingMem = GpadlRingMem> {
    #[inspect(mut)]
    pipe: IcPipe<M>,
    state: ChannelState,
    #[inspect(with = "Option::is_some")]
    pending_shutdown: Option<Rpc<(), ShutdownResult>>,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum ChannelState {
    Negotiate(#[inspect(flatten)] NegotiateState),
    Ready {
        versions: Versions,
        state: ReadyState,
        #[inspect(with = "|x| x.len()")]
        clients: Vec<mesh::OneshotSender<()>>,
    },
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum ReadyState {
    Ready,
    SendShutdown(#[inspect(skip)] ShutdownParams),
    WaitShutdown,
}

impl ShutdownIc {
    /// Returns a new shutdown IC, using `recv` to receive shutdown requests.
    pub fn new(recv: mesh::Receiver<ShutdownRpc>) -> Self {
        Self {
            recv,
            wait_ready: Vec::new(),
        }
    }
}

impl<M: RingMem + Unpin> ShutdownChannel<M> {
    fn new(
        channel: RawAsyncChannel<M>,
        restore_state: Option<ChannelState>,
    ) -> Result<Self, ChannelOpenError> {
        let pipe = IcPipe::new(channel)?;
        Ok(Self {
            pipe,
            state: restore_state.unwrap_or(ChannelState::Negotiate(NegotiateState::default())),
            pending_shutdown: None,
        })
    }

    async fn process(
        &mut self,
        ic: &mut ShutdownIc,
        check: &task_control::StopCheck<'_>,
    ) -> Result<(), Cancelled> {
        enum Event {
            StateMachine(anyhow::Result<()>),
            Request(ShutdownRpc),
        }

        loop {
            check.check()?;
            let event = pin!(
                (
                    once(
                        self.process_state_machine(&mut ic.wait_ready)
                            .map(Event::StateMachine)
                    ),
                    (&mut ic.recv).map(Event::Request),
                )
                    .merge()
            )
            .next()
            .await
            .unwrap();
            match event {
                Event::StateMachine(r) => {
                    if let Err(err) = r {
                        tracing::error!(
                            error = err.as_ref() as &dyn std::error::Error,
                            "shutdown ic error"
                        );
                        return Ok(());
                    }
                }
                Event::Request(req) => match req {
                    ShutdownRpc::WaitReady(rpc) => match &mut self.state {
                        ChannelState::Negotiate(_) => ic.wait_ready.push(rpc),
                        ChannelState::Ready { clients, .. } => {
                            let (send, recv) = mesh::oneshot();
                            clients.retain(|c| !c.is_closed());
                            clients.push(send);
                            rpc.complete(recv);
                        }
                    },
                    ShutdownRpc::Shutdown(rpc) => match self.state {
                        ChannelState::Negotiate(_) => rpc.complete(ShutdownResult::NotReady),
                        ChannelState::Ready { ref mut state, .. } => match state {
                            ReadyState::Ready => {
                                let (input, rpc) = rpc.split();
                                self.pending_shutdown = Some(rpc);
                                *state = ReadyState::SendShutdown(input);
                            }
                            ReadyState::SendShutdown { .. } | ReadyState::WaitShutdown => {
                                rpc.complete(ShutdownResult::AlreadyInProgress)
                            }
                        },
                    },
                },
            }
        }
    }

    async fn process_state_machine(
        &mut self,
        wait_ready: &mut Vec<Rpc<(), mesh::OneshotReceiver<()>>>,
    ) -> anyhow::Result<()> {
        match self.state {
            ChannelState::Negotiate(ref mut state) => {
                if let Some(versions) = self.pipe.negotiate(state, SHUTDOWN_VERSIONS).await? {
                    let clients = wait_ready
                        .drain(..)
                        .map(|rpc| {
                            let (send, recv) = mesh::oneshot();
                            rpc.complete(recv);
                            send
                        })
                        .collect();

                    self.state = ChannelState::Ready {
                        versions,
                        state: ReadyState::Ready,
                        clients,
                    };
                }
            }
            ChannelState::Ready {
                ref mut state,
                ref versions,
                clients: _,
            } => match state {
                ReadyState::Ready => std::future::pending().await,
                ReadyState::SendShutdown(params) => {
                    let mut flags =
                        hyperv_ic_protocol::shutdown::ShutdownFlags::new().with_force(params.force);
                    match params.shutdown_type {
                        ShutdownType::PowerOff => {}
                        ShutdownType::Reboot => flags.set_restart(true),
                        ShutdownType::Hibernate => flags.set_hibernate(true),
                    }

                    let message = Box::new(hyperv_ic_protocol::shutdown::ShutdownMessage {
                        reason_code: hyperv_ic_protocol::shutdown::SHTDN_REASON_FLAG_PLANNED,
                        timeout_secs: 0,
                        flags,
                        message: [0; 2048],
                    });

                    self.pipe
                        .write_message(
                            versions,
                            hyperv_ic_protocol::MessageType::SHUTDOWN,
                            hyperv_ic_protocol::HeaderFlags::new()
                                .with_transaction(true)
                                .with_request(true),
                            message.as_bytes(),
                        )
                        .await?;

                    *state = ReadyState::WaitShutdown;
                }
                ReadyState::WaitShutdown => {
                    let (status, _) = self.pipe.read_response().await?;
                    let result = if status == Status::SUCCESS {
                        ShutdownResult::Ok
                    } else {
                        ShutdownResult::Failed(status.0)
                    };
                    if let Some(send) = self.pending_shutdown.take() {
                        send.complete(result);
                    }
                    *state = ReadyState::Ready;
                }
            },
        }
        Ok(())
    }
}

#[async_trait]
impl SimpleVmbusDevice for ShutdownIc {
    type SavedState = save_restore::state::SavedState;
    type Runner = ShutdownChannel;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "shutdown_ic".to_owned(),
            instance_id: hyperv_ic_protocol::shutdown::INSTANCE_ID,
            interface_id: hyperv_ic_protocol::shutdown::INTERFACE_ID,
            channel_type: ChannelType::Pipe {
                message_mode: true,
                user_defined: Default::default(),
                pipe_flags: Default::default(),
            },
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, runner: Option<&mut Self::Runner>) {
        req.respond().merge(self).merge(runner);
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        ShutdownChannel::new(channel, None)
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut Self::Runner,
    ) -> Result<(), Cancelled> {
        let check = stop.check_handle()?;
        stop.until_stopped(runner.process(self, &check)).await?
    }

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn SaveRestoreSimpleVmbusDevice<SavedState = Self::SavedState, Runner = Self::Runner>,
    > {
        Some(self)
    }
}

mod save_restore {
    use super::*;

    pub mod state {
        use hyperv_ic_protocol;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Copy, Clone, Eq, PartialEq, Protobuf)]
        #[mesh(package = "shutdown_ic")]
        pub struct Version {
            #[mesh(1)]
            pub major: u16,
            #[mesh(2)]
            pub minor: u16,
        }

        impl From<hyperv_ic_protocol::Version> for Version {
            fn from(version: hyperv_ic_protocol::Version) -> Self {
                Self {
                    major: version.major,
                    minor: version.minor,
                }
            }
        }

        impl From<Version> for hyperv_ic_protocol::Version {
            fn from(version: Version) -> Self {
                Self {
                    major: version.major,
                    minor: version.minor,
                }
            }
        }

        #[derive(Copy, Clone, Eq, PartialEq, Protobuf)]
        #[mesh(package = "shutdown_ic")]
        pub struct ShutdownParams {
            #[mesh(1)]
            pub shutdown_type: ShutdownType,
            #[mesh(2)]
            pub force: bool,
        }

        impl From<&hyperv_ic_resources::shutdown::ShutdownParams> for ShutdownParams {
            fn from(params: &hyperv_ic_resources::shutdown::ShutdownParams) -> Self {
                let shutdown_type = match params.shutdown_type {
                    hyperv_ic_resources::shutdown::ShutdownType::PowerOff => ShutdownType::PowerOff,
                    hyperv_ic_resources::shutdown::ShutdownType::Reboot => ShutdownType::Reboot,
                    hyperv_ic_resources::shutdown::ShutdownType::Hibernate => {
                        ShutdownType::Hibernate
                    }
                };
                Self {
                    shutdown_type,
                    force: params.force,
                }
            }
        }

        impl From<&ShutdownParams> for hyperv_ic_resources::shutdown::ShutdownParams {
            fn from(params: &ShutdownParams) -> Self {
                let shutdown_type = match params.shutdown_type {
                    ShutdownType::PowerOff => hyperv_ic_resources::shutdown::ShutdownType::PowerOff,
                    ShutdownType::Reboot => hyperv_ic_resources::shutdown::ShutdownType::Reboot,
                    ShutdownType::Hibernate => {
                        hyperv_ic_resources::shutdown::ShutdownType::Hibernate
                    }
                };
                Self {
                    shutdown_type,
                    force: params.force,
                }
            }
        }

        impl From<ShutdownParams> for hyperv_ic_resources::shutdown::ShutdownParams {
            fn from(params: ShutdownParams) -> Self {
                (&params).into()
            }
        }

        #[derive(Copy, Clone, Eq, PartialEq, Protobuf)]
        #[mesh(package = "shutdown_ic")]
        pub enum ShutdownType {
            #[mesh(1)]
            PowerOff,
            #[mesh(2)]
            Reboot,
            #[mesh(3)]
            Hibernate,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "shutdown_ic")]
        pub struct SavedState {
            #[mesh(1)]
            pub version: Option<(Version, Version)>,
            #[mesh(2)]
            pub shutdown_request: Option<ShutdownParams>,
            #[mesh(3)]
            pub waiting_on_version: bool,
            #[mesh(4)]
            pub waiting_on_shutdown_response: bool,
        }
    }

    impl SaveRestoreSimpleVmbusDevice for ShutdownIc {
        fn save_open(&mut self, runner: &Self::Runner) -> state::SavedState {
            let (versions, shutdown_request, waiting_on_shutdown_response) =
                if let ChannelState::Ready {
                    versions,
                    ref state,
                    clients: _,
                } = runner.state
                {
                    let request = if let ReadyState::SendShutdown(request) = state {
                        Some(request.into())
                    } else {
                        None
                    };
                    let waiting = matches!(state, ReadyState::WaitShutdown);
                    (Some(versions), request, waiting)
                } else {
                    (None, None, false)
                };
            let waiting_on_version = matches!(
                runner.state,
                ChannelState::Negotiate(NegotiateState::WaitVersion)
            );
            state::SavedState {
                version: versions.map(|v| (v.framework_version.into(), v.message_version.into())),
                shutdown_request,
                waiting_on_version,
                waiting_on_shutdown_response,
            }
        }

        fn restore_open(
            &mut self,
            saved_state: Self::SavedState,
            channel: RawAsyncChannel<GpadlRingMem>,
        ) -> Result<Self::Runner, ChannelOpenError> {
            let state = if let Some((framework, message)) = saved_state.version {
                let state = if let Some(request) = saved_state.shutdown_request {
                    ReadyState::SendShutdown(request.into())
                } else if saved_state.waiting_on_shutdown_response {
                    ReadyState::WaitShutdown
                } else {
                    ReadyState::Ready
                };
                ChannelState::Ready {
                    versions: Versions {
                        framework_version: framework.into(),
                        message_version: message.into(),
                    },
                    state,
                    clients: Vec::new(),
                }
            } else {
                ChannelState::Negotiate(if saved_state.waiting_on_version {
                    NegotiateState::WaitVersion
                } else {
                    NegotiateState::SendVersion
                })
            };
            ShutdownChannel::new(channel, Some(state))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use hyperv_ic_protocol::Header;
    use hyperv_ic_protocol::HeaderFlags;
    use hyperv_ic_protocol::MessageType;
    use mesh::rpc::RpcSend;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use task_control::AsyncRun;
    use task_control::TaskControl;
    use test_with_tracing::test;
    use vmbus_async::async_dgram::AsyncRecvExt;
    use vmbus_async::pipe::MessagePipe;
    use vmbus_channel::connected_async_channels;
    use vmbus_channel::test_helpers::gated_channels;
    use vmbus_channel::test_helpers::stop_with_timeout;
    use vmbus_channel::test_helpers::with_timeout;
    use zerocopy::FromBytes;
    use zerocopy::FromZeros;

    struct Worker<M: RingMem> {
        ic: ShutdownIc,
        channel: ShutdownChannel<M>,
    }

    impl<M: RingMem + Unpin + Sync + 'static> AsyncRun<()> for Worker<M> {
        async fn run(&mut self, stop: &mut StopTask<'_>, _: &mut ()) -> Result<(), Cancelled> {
            let check = stop.check_handle()?;
            stop.until_stopped(self.channel.process(&mut self.ic, &check))
                .await?
        }
    }

    fn response() -> Header {
        Header {
            message_type: MessageType::SHUTDOWN,
            status: Status::SUCCESS,
            flags: HeaderFlags::new()
                .with_transaction(true)
                .with_response(true),
            ..FromZeros::new_zeroed()
        }
    }

    #[async_test]
    async fn stop_between_stale_responses_preserves_negotiation(driver: DefaultDriver) {
        let (_thread, worker_driver) = pal_async::DefaultPool::spawn_on_thread("ic-negotiate-stop");
        let (host, guest, gate) = gated_channels(16384);
        let mut guest = MessagePipe::new(guest).unwrap();
        guest.try_send(response().as_bytes()).unwrap();
        guest.try_send(response().as_bytes()).unwrap();
        let (_send, recv) = mesh::channel();
        let mut task = TaskControl::new(Worker {
            ic: ShutdownIc::new(recv),
            channel: ShutdownChannel::new(host, None).unwrap(),
        });
        task.insert(&worker_driver, "shutdown", ());
        assert!(task.start());
        gate.stop(&mut task, &driver).await;
        assert!(matches!(
            &task.task().channel.state,
            ChannelState::Negotiate(NegotiateState::WaitVersion)
        ));
        let mut buffer = [0; 256];
        let n = task
            .task_mut()
            .channel
            .pipe
            .pipe
            .recv(&mut buffer)
            .now_or_never()
            .unwrap()
            .unwrap();
        let (header, _) = Header::read_from_prefix(&buffer[..n]).unwrap();
        assert_eq!(header.message_type, MessageType::SHUTDOWN);
        let n = with_timeout(&driver, guest.recv(&mut buffer))
            .await
            .unwrap();
        let (header, _) = Header::read_from_prefix(&buffer[..n]).unwrap();
        assert_eq!(header.message_type, MessageType::VERSION_NEGOTIATION);
        assert!(task.start());
        stop_with_timeout(&mut task, &driver).await;
        assert!(guest.recv(&mut buffer).now_or_never().is_none());
    }

    #[async_test]
    async fn stop_waiting_for_shutdown_ack_preserves_rpc(driver: DefaultDriver) {
        let (_thread, worker_driver) = pal_async::DefaultPool::spawn_on_thread("ic-shutdown-stop");
        let (host, guest) = connected_async_channels(16384);
        let mut guest = MessagePipe::new(guest).unwrap();
        let (send, recv) = mesh::channel();
        let mut task = TaskControl::new(Worker {
            ic: ShutdownIc::new(recv),
            channel: ShutdownChannel::new(
                host,
                Some(ChannelState::Ready {
                    versions: Versions {
                        framework_version: hyperv_ic_protocol::FRAMEWORK_VERSION_3,
                        message_version: SHUTDOWN_VERSION_3,
                    },
                    state: ReadyState::Ready,
                    clients: Vec::new(),
                }),
            )
            .unwrap(),
        });
        task.insert(&worker_driver, "shutdown", ());
        assert!(task.start());
        let pending = send.call(
            ShutdownRpc::Shutdown,
            ShutdownParams {
                shutdown_type: ShutdownType::PowerOff,
                force: false,
            },
        );
        let mut buffer = [0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
        with_timeout(&driver, guest.recv(&mut buffer))
            .await
            .unwrap();
        stop_with_timeout(&mut task, &driver).await;
        assert!(task.task().channel.pending_shutdown.is_some());
        assert!(matches!(
            &task.task().channel.state,
            ChannelState::Ready {
                state: ReadyState::WaitShutdown,
                ..
            }
        ));
        guest.try_send(response().as_bytes()).unwrap();
        assert!(task.start());
        assert_eq!(
            with_timeout(&driver, pending).await.unwrap(),
            ShutdownResult::Ok
        );
        stop_with_timeout(&mut task, &driver).await;
        assert!(task.task().channel.pending_shutdown.is_none());
        assert!(matches!(
            &task.task().channel.state,
            ChannelState::Ready {
                state: ReadyState::Ready,
                ..
            }
        ));
        assert!(guest.recv(&mut buffer).now_or_never().is_none());
    }
}
