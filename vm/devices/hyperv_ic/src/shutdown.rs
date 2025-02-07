// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The shutdown IC.

use async_trait::async_trait;
use futures::stream::once;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::stream::Merge;
use hyperv_ic_protocol::shutdown::FRAMEWORK_VERSIONS;
use hyperv_ic_protocol::shutdown::SHUTDOWN_VERSIONS;
use hyperv_ic_resources::shutdown::ShutdownParams;
use hyperv_ic_resources::shutdown::ShutdownResult;
use hyperv_ic_resources::shutdown::ShutdownRpc;
use hyperv_ic_resources::shutdown::ShutdownType;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::Rpc;
use std::io::IoSlice;
use std::pin::pin;
use task_control::Cancelled;
use task_control::StopTask;
use thiserror::Error;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SaveRestoreSimpleVmbusDevice;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_channel::RawAsyncChannel;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// A shutdown IC device.
#[derive(InspectMut)]
pub struct ShutdownIc {
    #[inspect(skip)]
    recv: mesh::Receiver<ShutdownRpc>,
    #[inspect(skip)]
    wait_ready: Vec<Rpc<(), ()>>,
}

#[doc(hidden)]
#[derive(InspectMut)]
pub struct ShutdownChannel {
    #[inspect(mut)]
    pipe: MessagePipe<GpadlRingMem>,
    state: ChannelState,
    #[inspect(with = "Option::is_some")]
    pending_shutdown: Option<Rpc<(), ShutdownResult>>,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum ChannelState {
    SendVersion,
    WaitVersion,
    Ready {
        #[inspect(display)]
        framework_version: hyperv_ic_protocol::Version,
        #[inspect(display)]
        message_version: hyperv_ic_protocol::Version,
        state: ReadyState,
    },
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum ReadyState {
    Ready,
    SendShutdown(#[inspect(skip)] ShutdownParams),
    WaitShutdown,
}

#[derive(Debug, Error)]
enum Error {
    #[error("ring buffer error")]
    Ring(#[source] std::io::Error),
    #[error("truncated message")]
    TruncatedMessage,
    #[error("invalid version response")]
    InvalidVersionResponse,
    #[error("no supported versions")]
    NoSupportedVersions,
}

impl ShutdownIc {
    /// Returns a new shutdown IC, using `recv` to receive shutdown requests.
    pub fn new(recv: mesh::Receiver<ShutdownRpc>) -> Self {
        Self {
            recv,
            wait_ready: Vec::new(),
        }
    }

    fn open_channel(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
        restore_state: Option<ChannelState>,
    ) -> Result<ShutdownChannel, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(ShutdownChannel::new(pipe, restore_state))
    }
}

impl ShutdownChannel {
    fn new(pipe: MessagePipe<GpadlRingMem>, restore_state: Option<ChannelState>) -> Self {
        Self {
            pipe,
            state: restore_state.unwrap_or(ChannelState::SendVersion),
            pending_shutdown: None,
        }
    }

    async fn process(&mut self, ic: &mut ShutdownIc) -> Result<(), Error> {
        enum Event {
            StateMachine(Result<(), Error>),
            Request(ShutdownRpc),
        }

        loop {
            let event = pin!((
                once(
                    self.process_state_machine(&mut ic.wait_ready)
                        .map(Event::StateMachine)
                ),
                (&mut ic.recv).map(Event::Request),
            )
                .merge())
            .next()
            .await
            .unwrap();
            match event {
                Event::StateMachine(r) => {
                    r?;
                }
                Event::Request(req) => match req {
                    ShutdownRpc::WaitReady(rpc) => match self.state {
                        ChannelState::SendVersion | ChannelState::WaitVersion => {
                            ic.wait_ready.push(rpc)
                        }
                        ChannelState::Ready { .. } => rpc.complete(()),
                    },
                    ShutdownRpc::Shutdown(rpc) => match self.state {
                        ChannelState::SendVersion | ChannelState::WaitVersion => {
                            rpc.complete(ShutdownResult::NotReady)
                        }
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
        wait_ready: &mut Vec<Rpc<(), ()>>,
    ) -> Result<(), Error> {
        match self.state {
            ChannelState::SendVersion => {
                let message_versions = SHUTDOWN_VERSIONS;

                let message = hyperv_ic_protocol::NegotiateMessage {
                    framework_version_count: FRAMEWORK_VERSIONS.len() as u16,
                    message_version_count: message_versions.len() as u16,
                    ..FromZeros::new_zeroed()
                };

                let header = hyperv_ic_protocol::Header {
                    message_type: hyperv_ic_protocol::MessageType::VERSION_NEGOTIATION,
                    message_size: (size_of_val(&message)
                        + size_of_val(FRAMEWORK_VERSIONS)
                        + size_of_val(message_versions)) as u16,
                    status: 0,
                    transaction_id: 0,
                    flags: hyperv_ic_protocol::HeaderFlags::new()
                        .with_transaction(true)
                        .with_request(true),
                    ..FromZeros::new_zeroed()
                };

                self.pipe
                    .send_vectored(&[
                        IoSlice::new(header.as_bytes()),
                        IoSlice::new(message.as_bytes()),
                        IoSlice::new(FRAMEWORK_VERSIONS.as_bytes()),
                        IoSlice::new(message_versions.as_bytes()),
                    ])
                    .await
                    .map_err(Error::Ring)?;

                self.state = ChannelState::WaitVersion;
            }
            ChannelState::WaitVersion => {
                let (_result, buf) = read_response(&mut self.pipe).await?;
                let (message, rest) =
                    hyperv_ic_protocol::NegotiateMessage::read_from_prefix(buf.as_slice())
                        .map_err(|_| Error::TruncatedMessage)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                if message.framework_version_count != 1 || message.message_version_count != 1 {
                    return Err(Error::NoSupportedVersions);
                }
                let [framework_version, message_version] =
                    <[hyperv_ic_protocol::Version; 2]>::read_from_prefix(rest)
                        .map_err(|_| Error::TruncatedMessage)?
                        .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

                self.state = ChannelState::Ready {
                    framework_version,
                    message_version,
                    state: ReadyState::Ready,
                };
                for rpc in wait_ready.drain(..) {
                    rpc.complete(());
                }
            }
            ChannelState::Ready {
                ref mut state,
                framework_version,
                message_version,
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
                    let header = hyperv_ic_protocol::Header {
                        framework_version,
                        message_type: hyperv_ic_protocol::MessageType::SHUTDOWN,
                        message_size: size_of_val(message.as_ref()) as u16,
                        message_version,
                        status: 0,
                        transaction_id: 0,
                        flags: hyperv_ic_protocol::HeaderFlags::new()
                            .with_transaction(true)
                            .with_request(true),
                        ..FromZeros::new_zeroed()
                    };

                    self.pipe
                        .send_vectored(&[
                            IoSlice::new(header.as_bytes()),
                            IoSlice::new(message.as_bytes()),
                        ])
                        .await
                        .map_err(Error::Ring)?;

                    *state = ReadyState::WaitShutdown;
                }
                ReadyState::WaitShutdown => {
                    let (status, _) = read_response(&mut self.pipe).await?;
                    let result = if status == 0 {
                        ShutdownResult::Ok
                    } else {
                        ShutdownResult::Failed(status)
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

async fn read_response(pipe: &mut MessagePipe<GpadlRingMem>) -> Result<(u32, Vec<u8>), Error> {
    let mut buf = vec![0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
    let n = pipe.recv(&mut buf).await.map_err(Error::Ring)?;
    let buf = &buf[..n];
    let (header, rest) =
        hyperv_ic_protocol::Header::read_from_prefix(buf).map_err(|_| Error::TruncatedMessage)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

    if header.transaction_id != 0 || !header.flags.transaction() || !header.flags.response() {
        return Err(Error::InvalidVersionResponse);
    }

    let rest = rest
        .get(..header.message_size as usize)
        .ok_or(Error::TruncatedMessage)?;

    Ok((header.status, rest.to_vec()))
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
            channel_type: ChannelType::Pipe { message_mode: true },
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
        self.open_channel(channel, None)
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut Self::Runner,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(async {
            match runner.process(self).await {
                Ok(()) => {}
                Err(err) => {
                    tracing::error!(error = &err as &dyn std::error::Error, "shutdown ic error")
                }
            }
        })
        .await
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
            let (version, shutdown_request, waiting_on_shutdown_response) =
                if let ChannelState::Ready {
                    framework_version,
                    message_version,
                    state,
                } = &runner.state
                {
                    let request = if let ReadyState::SendShutdown(request) = state {
                        Some(request.into())
                    } else {
                        None
                    };
                    let waiting = matches!(state, ReadyState::WaitShutdown);
                    (
                        Some(((*framework_version).into(), (*message_version).into())),
                        request,
                        waiting,
                    )
                } else {
                    (None, None, false)
                };
            let waiting_on_version = matches!(runner.state, ChannelState::WaitVersion);
            state::SavedState {
                version,
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
                    framework_version: framework.into(),
                    message_version: message.into(),
                    state,
                }
            } else {
                if saved_state.waiting_on_version {
                    ChannelState::WaitVersion
                } else {
                    ChannelState::SendVersion
                }
            };
            self.open_channel(channel, Some(state))
        }
    }
}
