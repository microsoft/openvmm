// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The shutdown IC client.

#![forbid(unsafe_code)]

pub use hyperv_ic_protocol::shutdown::INSTANCE_ID;
pub use hyperv_ic_protocol::shutdown::INTERFACE_ID;

use guid::Guid;
use hyperv_ic_protocol::FRAMEWORK_VERSION_1;
use hyperv_ic_protocol::FRAMEWORK_VERSION_3;
use hyperv_ic_protocol::Status;
use hyperv_ic_protocol::shutdown::SHUTDOWN_VERSION_1;
use hyperv_ic_protocol::shutdown::SHUTDOWN_VERSION_3;
use hyperv_ic_protocol::shutdown::SHUTDOWN_VERSION_3_1;
use hyperv_ic_protocol::shutdown::SHUTDOWN_VERSION_3_2;
use hyperv_ic_resources::shutdown::ShutdownParams;
use hyperv_ic_resources::shutdown::ShutdownResult;
use hyperv_ic_resources::shutdown::ShutdownType;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use std::io::IoSlice;
use std::mem::size_of_val;
use task_control::Cancelled;
use task_control::StopTask;
use thiserror::Error;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_relay_intercept_device::OfferResponse;
use vmbus_relay_intercept_device::SaveRestoreSimpleVmbusClientDevice;
use vmbus_relay_intercept_device::SimpleVmbusClientDevice;
use vmbus_relay_intercept_device::SimpleVmbusClientDeviceAsync;
use vmbus_relay_intercept_device::ring_buffer::MemoryBlockRingBuffer;
use vmbus_ring::RingMem;
use vmcore::save_restore::NoSavedState;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// A shutdown IC client device.
#[derive(InspectMut)]
pub struct ShutdownGuestIc {
    #[inspect(skip)]
    send_shutdown_notification: mesh::Sender<Rpc<ShutdownParams, ShutdownResult>>,
    #[inspect(skip)]
    recv_shutdown_notification: Option<mesh::Receiver<Rpc<ShutdownParams, ShutdownResult>>>,
}

#[derive(Debug, Inspect)]
#[inspect(tag = "channel_state")]
enum ShutdownGuestChannelState {
    NegotiateVersion,
    Running {
        #[inspect(display)]
        framework_version: hyperv_ic_protocol::Version,
        #[inspect(display)]
        message_version: hyperv_ic_protocol::Version,
    },
}

/// Established channel between guest and host.
#[derive(InspectMut)]
pub struct ShutdownGuestChannel {
    /// Current state.
    state: ShutdownGuestChannelState,
    /// Vmbus pipe to the host.
    #[inspect(mut)]
    pipe: MessagePipe<MemoryBlockRingBuffer>,
}

#[derive(Debug, Error)]
enum Error {
    #[error("ring buffer error")]
    Ring(#[source] std::io::Error),
    #[error("truncated message")]
    TruncatedMessage,
}

impl ShutdownGuestIc {
    /// Returns a new shutdown IC client device.
    pub fn new() -> Self {
        let (send_shutdown_notification, recv_shutdown_notification) = mesh::channel();
        Self {
            send_shutdown_notification,
            recv_shutdown_notification: Some(recv_shutdown_notification),
        }
    }

    /// Returns the notifier that will receive any shutdown requests from the host.
    pub fn get_shutdown_notifier(&mut self) -> mesh::Receiver<Rpc<ShutdownParams, ShutdownResult>> {
        self.recv_shutdown_notification
            .take()
            .expect("can only be called once")
    }
}

impl ShutdownGuestChannel {
    fn new(pipe: MessagePipe<MemoryBlockRingBuffer>) -> Self {
        Self {
            state: ShutdownGuestChannelState::NegotiateVersion,
            pipe,
        }
    }

    async fn process(&mut self, ic: &mut ShutdownGuestIc) -> Result<(), Error> {
        loop {
            match read_from_pipe(&mut self.pipe).await {
                Ok(buf) => {
                    self.handle_host_message(&buf, ic).await;
                }
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        err = &err as &dyn std::error::Error,
                        "reading shutdown packet from host",
                    );
                }
            }
        }
    }

    async fn handle_host_message(&mut self, buf: &[u8], ic: &ShutdownGuestIc) {
        // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
        let (header, rest) = match hyperv_ic_protocol::Header::read_from_prefix(buf).ok() {
            Some((h, r)) => (h, r),
            None => {
                tracelimit::error_ratelimited!("invalid shutdown packet from host",);
                return;
            }
        };
        match header.message_type {
            hyperv_ic_protocol::MessageType::VERSION_NEGOTIATION => {
                // Version negotiation can happen multiple times due to various
                // state changes on the host. This message triggers a reset
                // of the current state.
                self.state = ShutdownGuestChannelState::NegotiateVersion;
                if let Err(err) = self.handle_version_negotiation(&header, rest).await {
                    tracelimit::error_ratelimited!(
                        err = &err as &dyn std::error::Error,
                        "Failed version negotiation"
                    );
                }
            }
            hyperv_ic_protocol::MessageType::SHUTDOWN
                if matches!(self.state, ShutdownGuestChannelState::Running { .. }) =>
            {
                if let Err(err) = self.handle_shutdown_notification(&header, rest, ic).await {
                    tracelimit::error_ratelimited!(
                        err = &err as &dyn std::error::Error,
                        "Failed processing shutdown message"
                    );
                }
            }
            _ => {
                tracelimit::error_ratelimited!(r#type = ?header.message_type, state = ?self.state, "Unrecognized packet");
            }
        }
    }

    fn find_latest_supported_version<'a>(
        buf: &'a [u8],
        count: usize,
        supported: &[hyperv_ic_protocol::Version],
    ) -> (Option<hyperv_ic_protocol::Version>, &'a [u8]) {
        let mut rest = buf;
        let mut next_version;
        let mut latest_version = None;
        for _ in 0..count {
            // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
            (next_version, rest) = match hyperv_ic_protocol::Version::read_from_prefix(rest).ok() {
                Some((n, r)) => (n, r),
                None => {
                    tracelimit::error_ratelimited!("truncated message version list");
                    return (latest_version, rest);
                }
            };
            for known in supported {
                if known.major == next_version.major && known.minor == next_version.minor {
                    if latest_version.is_some() {
                        if next_version.major >= latest_version.unwrap().major {
                            if next_version.major > latest_version.unwrap().major
                                || next_version.minor > latest_version.unwrap().minor
                            {
                                latest_version = Some(next_version);
                            }
                        }
                    } else {
                        latest_version = Some(next_version);
                    }
                }
            }
        }
        (latest_version, rest)
    }

    async fn handle_version_negotiation(
        &mut self,
        header: &hyperv_ic_protocol::Header,
        msg: &[u8],
    ) -> Result<(), Error> {
        const FRAMEWORK_VERSIONS: &[hyperv_ic_protocol::Version] =
            &[FRAMEWORK_VERSION_1, FRAMEWORK_VERSION_3];

        const SHUTDOWN_VERSIONS: &[hyperv_ic_protocol::Version] = &[
            SHUTDOWN_VERSION_1,
            SHUTDOWN_VERSION_3,
            SHUTDOWN_VERSION_3_1,
            SHUTDOWN_VERSION_3_2,
        ];

        let (prefix, rest) = hyperv_ic_protocol::NegotiateMessage::read_from_prefix(msg)
            .map_err(|_| Error::TruncatedMessage)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        let (latest_framework_version, rest) = Self::find_latest_supported_version(
            rest,
            prefix.framework_version_count as usize,
            FRAMEWORK_VERSIONS,
        );
        let framework_version = if let Some(version) = latest_framework_version {
            version
        } else {
            tracelimit::error_ratelimited!("Unsupported framework version");
            FRAMEWORK_VERSIONS[FRAMEWORK_VERSIONS.len() - 1]
        };
        let (latest_message_version, _) = Self::find_latest_supported_version(
            rest,
            prefix.message_version_count as usize,
            SHUTDOWN_VERSIONS,
        );
        let message_version = if let Some(version) = latest_message_version {
            version
        } else {
            tracelimit::error_ratelimited!("Unsupported message version");
            SHUTDOWN_VERSIONS[SHUTDOWN_VERSIONS.len() - 1]
        };

        let message = hyperv_ic_protocol::NegotiateMessage {
            framework_version_count: 1,
            message_version_count: 1,
            ..FromZeros::new_zeroed()
        };
        let response = hyperv_ic_protocol::Header {
            message_type: hyperv_ic_protocol::MessageType::VERSION_NEGOTIATION,
            message_size: (size_of_val(&message)
                + size_of_val(&framework_version)
                + size_of_val(&message_version)) as u16,
            status: Status::SUCCESS,
            transaction_id: header.transaction_id,
            flags: hyperv_ic_protocol::HeaderFlags::new()
                .with_transaction(header.flags.transaction())
                .with_response(true),
            ..FromZeros::new_zeroed()
        };
        self.pipe
            .send_vectored(&[
                IoSlice::new(response.as_bytes()),
                IoSlice::new(message.as_bytes()),
                IoSlice::new(framework_version.as_bytes()),
                IoSlice::new(message_version.as_bytes()),
            ])
            .await
            .map_err(Error::Ring)?;

        tracing::info!(%framework_version, %message_version, "version negotiated");
        self.state = ShutdownGuestChannelState::Running {
            framework_version,
            message_version,
        };
        Ok(())
    }

    async fn handle_shutdown_notification(
        &mut self,
        header: &hyperv_ic_protocol::Header,
        buf: &[u8],
        ic: &ShutdownGuestIc,
    ) -> Result<(), Error> {
        let ShutdownGuestChannelState::Running {
            framework_version,
            message_version,
        } = &self.state
        else {
            panic!("Shutdown message processing while in invalid state");
        };

        let message = hyperv_ic_protocol::shutdown::ShutdownMessage::read_from_prefix(buf)
            .map_err(|_| Error::TruncatedMessage)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        let shutdown_type = if message.flags.restart() {
            ShutdownType::Reboot
        } else if message.flags.hibernate() {
            ShutdownType::Hibernate
        } else {
            ShutdownType::PowerOff
        };
        let params = ShutdownParams {
            shutdown_type,
            force: message.flags.force(),
        };

        // Notify the internal listener and wait for a response.
        let status = match ic.send_shutdown_notification.call(|x| x, params).await {
            Ok(ShutdownResult::Ok) => Status::SUCCESS,
            Ok(ShutdownResult::Failed(x)) => Status(x),
            Ok(ShutdownResult::NotReady) | Ok(ShutdownResult::AlreadyInProgress) | Err(_) => {
                Status::FAIL
            }
        };

        // Respond to the request.
        let response = hyperv_ic_protocol::Header {
            framework_version: *framework_version,
            message_version: *message_version,
            message_type: hyperv_ic_protocol::MessageType::SHUTDOWN,
            message_size: 0,
            status,
            transaction_id: header.transaction_id,
            flags: hyperv_ic_protocol::HeaderFlags::new()
                .with_transaction(header.flags.transaction())
                .with_response(true),
            ..FromZeros::new_zeroed()
        };
        self.pipe
            .send(response.as_bytes())
            .await
            .map_err(Error::Ring)
    }
}

async fn read_from_pipe<T: RingMem>(pipe: &mut MessagePipe<T>) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
    let n = pipe.recv(&mut buf).await.map_err(Error::Ring)?;
    let buf = &buf[..n];
    Ok(buf.to_vec())
}

impl SimpleVmbusClientDevice for ShutdownGuestIc {
    type SavedState = NoSavedState;
    type Runner = ShutdownGuestChannel;

    fn instance_id(&self) -> Guid {
        INSTANCE_ID
    }

    fn offer(&self, _offer: &vmbus_core::protocol::OfferChannel) -> OfferResponse {
        OfferResponse::Open
    }

    fn inspect(&mut self, req: inspect::Request<'_>, runner: Option<&mut Self::Runner>) {
        req.respond().merge(self).merge(runner);
    }

    fn open(
        &mut self,
        _channel_idx: u16,
        channel: RawAsyncChannel<MemoryBlockRingBuffer>,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(ShutdownGuestChannel::new(pipe))
    }

    fn close(&mut self, _channel_idx: u16) {}

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn SaveRestoreSimpleVmbusClientDevice<
            SavedState = Self::SavedState,
            Runner = Self::Runner,
        >,
    > {
        None
    }
}

impl SimpleVmbusClientDeviceAsync for ShutdownGuestIc {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut Self::Runner,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(async {
            match runner.process(self).await {
                Ok(()) => {}
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "shutdown ic relay error"
                    )
                }
            }
        })
        .await
    }
}
