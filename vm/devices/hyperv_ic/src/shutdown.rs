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
                    let supported = match params.shutdown_type {
                        ShutdownType::PowerOff => true,
                        ShutdownType::Reboot => versions.message_version >= SHUTDOWN_VERSION_3_1,
                        ShutdownType::Hibernate => versions.message_version >= SHUTDOWN_VERSION_3_2,
                    };
                    if !supported {
                        if let Some(send) = self.pending_shutdown.take() {
                            send.complete(ShutdownResult::Failed(Status::NOT_SUPPORTED.0));
                        }
                        *state = ReadyState::Ready;
                        return Ok(());
                    }

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
                    let Some((message_type, status, _)) =
                        self.pipe.read_message(Some(versions)).await?
                    else {
                        return Ok(());
                    };
                    if message_type != hyperv_ic_protocol::MessageType::SHUTDOWN {
                        tracelimit::warn_ratelimited!(
                            ?message_type,
                            "dropping unexpected message while waiting for shutdown acknowledgment"
                        );
                        return Ok(());
                    }
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
            /// Defaults to zero when absent in downlevel snapshots, matching the
            /// transaction ID used by older implementations.
            #[mesh(5)]
            pub transaction_id: u8,
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
                transaction_id: runner.pipe.transaction_id,
            }
        }

        fn restore_open(
            &mut self,
            saved_state: state::SavedState,
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
            let mut runner = ShutdownChannel::new(channel, Some(state))?;
            runner.pipe.transaction_id = saved_state.transaction_id;
            Ok(runner)
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
    use vmbus_channel::gpadl::GpadlId;
    use vmbus_channel::gpadl::GpadlMap;
    use vmbus_channel::gpadl_ring::AlignedGpadlView;
    use vmbus_channel::test_helpers::gated_channels;
    use vmbus_channel::test_helpers::stop_with_timeout;
    use vmbus_channel::test_helpers::with_timeout;
    use vmbus_ring::IncomingRing;
    use vmbus_ring::OutgoingRing;
    use vmbus_ring::gparange::MultiPagedRangeBuf;
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
            framework_version: hyperv_ic_protocol::FRAMEWORK_VERSION_3,
            message_version: SHUTDOWN_VERSION_3,
            message_type: MessageType::SHUTDOWN,
            status: Status::SUCCESS,
            flags: HeaderFlags::new()
                .with_transaction(true)
                .with_response(true),
            ..FromZeros::new_zeroed()
        }
    }

    #[async_test]
    async fn shutdown_operations_require_supported_versions(driver: DefaultDriver) {
        let (_thread, worker_driver) =
            pal_async::DefaultPool::spawn_on_thread("ic-shutdown-version");
        for (message_version, reboot_supported, hibernate_supported) in [
            (SHUTDOWN_VERSION_1, false, false),
            (SHUTDOWN_VERSION_3, false, false),
            (SHUTDOWN_VERSION_3_1, true, false),
            (SHUTDOWN_VERSION_3_2, true, true),
        ] {
            let versions = Versions {
                framework_version: hyperv_ic_protocol::FRAMEWORK_VERSION_3,
                message_version,
            };
            let (host, guest) = connected_async_channels(16384);
            let mut guest = MessagePipe::new(guest).unwrap();
            let (send, recv) = mesh::channel();
            let mut task = TaskControl::new(Worker {
                ic: ShutdownIc::new(recv),
                channel: ShutdownChannel::new(
                    host,
                    Some(ChannelState::Ready {
                        versions,
                        state: ReadyState::Ready,
                        clients: Vec::new(),
                    }),
                )
                .unwrap(),
            });
            task.insert(&worker_driver, "shutdown", ());
            assert!(task.start());

            for force in [false, true] {
                for (shutdown_type, supported) in [
                    (ShutdownType::Reboot, reboot_supported),
                    (ShutdownType::Hibernate, hibernate_supported),
                    (ShutdownType::PowerOff, true),
                ] {
                    let restart = matches!(shutdown_type, ShutdownType::Reboot);
                    let hibernate = matches!(shutdown_type, ShutdownType::Hibernate);
                    let pending = send.call(
                        ShutdownRpc::Shutdown,
                        ShutdownParams {
                            shutdown_type,
                            force,
                        },
                    );
                    let mut buffer = [0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
                    if supported {
                        let size = with_timeout(&driver, guest.recv(&mut buffer))
                            .await
                            .unwrap();
                        let (header, body) = Header::read_from_prefix(&buffer[..size]).unwrap();
                        assert_eq!(header.message_type, MessageType::SHUTDOWN);
                        assert_eq!(header.framework_version, versions.framework_version);
                        assert_eq!(header.message_version, message_version);
                        assert_eq!(
                            header.message_size as usize,
                            size_of::<hyperv_ic_protocol::shutdown::ShutdownMessage>()
                        );
                        let message =
                            hyperv_ic_protocol::shutdown::ShutdownMessage::read_from_bytes(body)
                                .unwrap();
                        assert_eq!(message.flags.force(), force);
                        assert_eq!(message.flags.restart(), restart);
                        assert_eq!(message.flags.hibernate(), hibernate);
                        let mut ack = response();
                        ack.framework_version = versions.framework_version;
                        ack.message_version = message_version;
                        ack.transaction_id = header.transaction_id;
                        guest.try_send(ack.as_bytes()).unwrap();
                        assert_eq!(
                            with_timeout(&driver, pending).await.unwrap(),
                            ShutdownResult::Ok
                        );
                    } else {
                        assert_eq!(
                            with_timeout(&driver, pending).await.unwrap(),
                            ShutdownResult::Failed(Status::NOT_SUPPORTED.0),
                            "unexpected result for version {message_version}, restart={restart}, hibernate={hibernate}, force={force}"
                        );
                    }
                    assert!(guest.recv(&mut buffer).now_or_never().is_none());
                }
            }

            stop_with_timeout(&mut task, &driver).await;
            assert!(task.task().channel.pending_shutdown.is_none());
            assert!(matches!(
                &task.task().channel.state,
                ChannelState::Ready {
                    state: ReadyState::Ready,
                    ..
                }
            ));
        }
    }

    #[async_test]
    async fn shutdown_ignores_unrelated_responses(driver: DefaultDriver) {
        let (_thread, worker_driver) =
            pal_async::DefaultPool::spawn_on_thread("ic-shutdown-response");
        for status in [Status::SUCCESS, Status::FAIL, Status::NOT_SUPPORTED] {
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
            let mut pending = send.call(
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

            for unrelated_status in [Status::SUCCESS, Status::FAIL] {
                for mut unrelated in [
                    Header {
                        message_type: MessageType::TIME_SYNC,
                        ..response()
                    },
                    Header {
                        transaction_id: 1,
                        ..response()
                    },
                    Header {
                        framework_version: hyperv_ic_protocol::FRAMEWORK_VERSION_1,
                        ..response()
                    },
                    Header {
                        message_version: SHUTDOWN_VERSION_3_1,
                        ..response()
                    },
                    Header {
                        message_type: MessageType::VERSION_NEGOTIATION,
                        framework_version: hyperv_ic_protocol::Version::new(0, 0),
                        message_version: hyperv_ic_protocol::Version::new(0, 0),
                        ..response()
                    },
                ] {
                    unrelated.status = unrelated_status;
                    guest.try_send(unrelated.as_bytes()).unwrap();
                    with_timeout(
                        &driver,
                        task.task_mut()
                            .channel
                            .process_state_machine(&mut Vec::new()),
                    )
                    .await
                    .unwrap();
                    assert!(task.task().channel.pending_shutdown.is_some());
                    assert!((&mut pending).now_or_never().is_none());
                    assert!(matches!(
                        &task.task().channel.state,
                        ChannelState::Ready {
                            state: ReadyState::WaitShutdown,
                            ..
                        }
                    ));
                }
            }

            let mut ack = response();
            ack.status = status;
            guest.try_send(ack.as_bytes()).unwrap();
            assert!(task.start());
            let expected = if status == Status::SUCCESS {
                ShutdownResult::Ok
            } else {
                ShutdownResult::Failed(status.0)
            };
            assert_eq!(with_timeout(&driver, pending).await.unwrap(), expected);
            stop_with_timeout(&mut task, &driver).await;
            assert!(task.task().channel.pending_shutdown.is_none());
            assert!(matches!(
                &task.task().channel.state,
                ChannelState::Ready {
                    state: ReadyState::Ready,
                    ..
                }
            ));
        }
    }

    #[async_test]
    async fn restore_preserves_transaction_id(driver: DefaultDriver) {
        let (_send, recv) = mesh::channel();
        let mut ic = ShutdownIc::new(recv);
        for waiting_on_version in [false, true] {
            for transaction_id in [0, 42, u8::MAX] {
                let versions = Versions {
                    framework_version: hyperv_ic_protocol::FRAMEWORK_VERSION_3,
                    message_version: SHUTDOWN_VERSION_3,
                };
                let (host, _guest) = save_restore_channels();
                let state = if waiting_on_version {
                    ChannelState::Negotiate(NegotiateState::WaitVersion)
                } else {
                    ChannelState::Ready {
                        versions,
                        state: ReadyState::WaitShutdown,
                        clients: Vec::new(),
                    }
                };
                let mut channel = ShutdownChannel::new(host, Some(state)).unwrap();
                channel.pipe.transaction_id = transaction_id;
                let saved = mesh::payload::encode(ic.save_open(&channel));
                let saved = mesh::payload::decode(&saved).unwrap();

                let (host, guest) = save_restore_channels();
                let mut guest = MessagePipe::new(guest).unwrap();
                let mut restored = ic.restore_open(saved, host).unwrap();
                assert_eq!(restored.pipe.transaction_id, transaction_id);
                let mut buffer = [0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
                assert!(guest.recv(&mut buffer).now_or_never().is_none());

                let mut ack = response();
                let body = if waiting_on_version {
                    ack.message_type = MessageType::VERSION_NEGOTIATION;
                    ack.framework_version = hyperv_ic_protocol::Version::new(0, 0);
                    ack.message_version = hyperv_ic_protocol::Version::new(0, 0);
                    let message = hyperv_ic_protocol::NegotiateMessage {
                        framework_version_count: 1,
                        message_version_count: 1,
                        reserved: 0,
                    };
                    let table = [versions.framework_version, versions.message_version];
                    [message.as_bytes(), table.as_bytes()].concat()
                } else {
                    Vec::new()
                };
                ack.message_size = body.len() as u16;
                ack.transaction_id = transaction_id.wrapping_add(1);
                guest.try_send(&[ack.as_bytes(), &body].concat()).unwrap();
                with_timeout(&driver, restored.process_state_machine(&mut Vec::new()))
                    .await
                    .unwrap();
                assert!(matches!(
                    &restored.state,
                    ChannelState::Negotiate(NegotiateState::WaitVersion)
                        | ChannelState::Ready {
                            state: ReadyState::WaitShutdown,
                            ..
                        }
                ));
                assert!(guest.recv(&mut buffer).now_or_never().is_none());

                ack.transaction_id = transaction_id;
                guest.try_send(&[ack.as_bytes(), &body].concat()).unwrap();
                with_timeout(&driver, restored.process_state_machine(&mut Vec::new()))
                    .await
                    .unwrap();
                let ChannelState::Ready {
                    versions: restored_versions,
                    state,
                    ..
                } = &mut restored.state
                else {
                    panic!("restored negotiation did not complete");
                };
                assert_eq!(
                    restored_versions.framework_version,
                    versions.framework_version
                );
                assert_eq!(restored_versions.message_version, versions.message_version);
                assert!(matches!(state, ReadyState::Ready));
                *state = ReadyState::SendShutdown(ShutdownParams {
                    shutdown_type: ShutdownType::PowerOff,
                    force: false,
                });
                with_timeout(&driver, restored.process_state_machine(&mut Vec::new()))
                    .await
                    .unwrap();
                let size = with_timeout(&driver, guest.recv(&mut buffer))
                    .await
                    .unwrap();
                let (header, _) = Header::read_from_prefix(&buffer[..size]).unwrap();
                assert_eq!(header.message_type, MessageType::SHUTDOWN);
                assert_eq!(header.transaction_id, transaction_id.wrapping_add(1));
            }
        }
    }

    #[async_test]
    async fn restore_legacy_shutdown_transaction(driver: DefaultDriver) {
        #[derive(mesh::payload::Protobuf)]
        #[mesh(package = "shutdown_ic")]
        struct LegacySavedState {
            #[mesh(1)]
            version: Option<(save_restore::state::Version, save_restore::state::Version)>,
            #[mesh(2)]
            shutdown_request: Option<save_restore::state::ShutdownParams>,
            #[mesh(3)]
            waiting_on_version: bool,
            #[mesh(4)]
            waiting_on_shutdown_response: bool,
        }

        let legacy = LegacySavedState {
            version: Some((
                hyperv_ic_protocol::FRAMEWORK_VERSION_3.into(),
                SHUTDOWN_VERSION_3.into(),
            )),
            shutdown_request: None,
            waiting_on_version: false,
            waiting_on_shutdown_response: true,
        };
        let bytes = mesh::payload::encode(legacy);
        let saved: save_restore::state::SavedState = mesh::payload::decode(&bytes).unwrap();
        assert_eq!(saved.transaction_id, 0);
        let (_send, recv) = mesh::channel();
        let mut ic = ShutdownIc::new(recv);
        let (host, guest) = save_restore_channels();
        let mut guest = MessagePipe::new(guest).unwrap();
        let mut restored = ic.restore_open(saved, host).unwrap();
        guest.try_send(response().as_bytes()).unwrap();
        with_timeout(&driver, restored.process_state_machine(&mut Vec::new()))
            .await
            .unwrap();
        assert!(matches!(
            restored.state,
            ChannelState::Ready {
                state: ReadyState::Ready,
                ..
            }
        ));
    }

    fn save_restore_channels() -> (RawAsyncChannel<GpadlRingMem>, RawAsyncChannel<GpadlRingMem>) {
        let memory = guestmem::GuestMemory::allocate(10 * 4096);
        let map = GpadlMap::new();
        map.add(
            GpadlId(1),
            MultiPagedRangeBuf::from_range_buffer(1, vec![10 * 4096, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9])
                .unwrap(),
        );
        let rings = AlignedGpadlView::new(map.view().map(GpadlId(1)).unwrap()).unwrap();
        let (host_ring, guest_ring) = rings.split(5).ok().unwrap();
        let (host, guest) = connected_async_channels(16384);
        (
            RawAsyncChannel {
                in_ring: IncomingRing::new(GpadlRingMem::new(host_ring.clone(), &memory).unwrap())
                    .unwrap(),
                out_ring: OutgoingRing::new(
                    GpadlRingMem::new(guest_ring.clone(), &memory).unwrap(),
                )
                .unwrap(),
                signal: host.signal,
            },
            RawAsyncChannel {
                in_ring: IncomingRing::new(GpadlRingMem::new(guest_ring, &memory).unwrap())
                    .unwrap(),
                out_ring: OutgoingRing::new(GpadlRingMem::new(host_ring, &memory).unwrap())
                    .unwrap(),
                signal: guest.signal,
            },
        )
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
        task.task_mut().channel.pipe.transaction_id = 41;
        assert!(task.start());
        let pending = send.call(
            ShutdownRpc::Shutdown,
            ShutdownParams {
                shutdown_type: ShutdownType::PowerOff,
                force: false,
            },
        );
        let mut buffer = [0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
        let size = with_timeout(&driver, guest.recv(&mut buffer))
            .await
            .unwrap();
        let (header, _) = Header::read_from_prefix(&buffer[..size]).unwrap();
        assert_eq!(header.transaction_id, 42);
        stop_with_timeout(&mut task, &driver).await;
        assert!(task.task().channel.pending_shutdown.is_some());
        assert_eq!(
            task.task().channel.pipe.transaction_id,
            header.transaction_id
        );
        assert!(matches!(
            &task.task().channel.state,
            ChannelState::Ready {
                state: ReadyState::WaitShutdown,
                ..
            }
        ));
        let mut ack = response();
        ack.transaction_id = header.transaction_id;
        guest.try_send(ack.as_bytes()).unwrap();
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
