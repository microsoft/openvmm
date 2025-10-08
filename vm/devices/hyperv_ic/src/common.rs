// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common code for IC implementations.

use anyhow::Context as _;
use hyperv_ic_protocol::FRAMEWORK_VERSION_1;
use hyperv_ic_protocol::FRAMEWORK_VERSION_3;
use hyperv_ic_protocol::HeaderFlags;
use hyperv_ic_protocol::MessageType;
use hyperv_ic_protocol::Status;
use hyperv_ic_protocol::Version;
use inspect::Inspect;
use inspect::InspectMut;
use std::io::IoSlice;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Supported framework versions.
const FRAMEWORK_VERSIONS: &[Version] = &[FRAMEWORK_VERSION_1, FRAMEWORK_VERSION_3];

#[derive(InspectMut)]
pub(crate) struct IcPipe {
    #[inspect(mut)]
    pub pipe: MessagePipe<GpadlRingMem>,
    #[inspect(skip)]
    buf: Vec<u8>,
}

#[derive(Inspect, Default)]
pub(crate) enum NegotiateState {
    #[default]
    SendVersion,
    WaitVersion,
    Invalid,
}

#[derive(Copy, Clone, Debug, Inspect)]
pub(crate) struct Versions {
    #[inspect(display)]
    pub framework_version: Version,
    #[inspect(display)]
    pub message_version: Version,
}

impl IcPipe {
    pub fn new(raw: RawAsyncChannel<GpadlRingMem>) -> Result<Self, std::io::Error> {
        let pipe = MessagePipe::new(raw)?;
        let buf = vec![0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
        Ok(Self { pipe, buf })
    }

    pub async fn negotiate(
        &mut self,
        state: &mut NegotiateState,
        message_versions: &[Version],
    ) -> anyhow::Result<Option<Versions>> {
        match state {
            NegotiateState::SendVersion => {
                let message = hyperv_ic_protocol::NegotiateMessage {
                    framework_version_count: FRAMEWORK_VERSIONS.len() as u16,
                    message_version_count: message_versions.len() as u16,
                    ..FromZeros::new_zeroed()
                };

                let header = hyperv_ic_protocol::Header {
                    message_type: MessageType::VERSION_NEGOTIATION,
                    message_size: (size_of_val(&message)
                        + size_of_val(FRAMEWORK_VERSIONS)
                        + size_of_val(message_versions)) as u16,
                    status: Status::SUCCESS,
                    transaction_id: 0,
                    flags: HeaderFlags::new().with_transaction(true).with_request(true),
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
                    .context("ring buffer error")?;

                *state = NegotiateState::WaitVersion;
                Ok(None)
            }
            NegotiateState::WaitVersion => {
                let (_result, buf) = self.read_response().await?;
                let (message, rest) = hyperv_ic_protocol::NegotiateMessage::read_from_prefix(buf)
                    .ok()
                    .context("missing negotiate message")?;
                if message.framework_version_count != 1 || message.message_version_count != 1 {
                    anyhow::bail!("no supported versions");
                }
                let ([framework_version, message_version], _) =
                    <[Version; 2]>::read_from_prefix(rest)
                        .ok()
                        .context("missing version table")?;

                *state = NegotiateState::Invalid;
                Ok(Some(Versions {
                    framework_version,
                    message_version,
                }))
            }
            NegotiateState::Invalid => {
                unreachable!()
            }
        }
    }

    pub async fn write_message(
        &mut self,
        versions: &Versions,
        message_type: MessageType,
        flags: HeaderFlags,
        message: &[u8],
    ) -> anyhow::Result<()> {
        let header = hyperv_ic_protocol::Header {
            framework_version: versions.framework_version,
            message_type,
            message_size: message.len() as u16,
            message_version: versions.message_version,
            status: Status::SUCCESS,
            transaction_id: 0,
            flags,
            ..FromZeros::new_zeroed()
        };

        self.pipe
            .send_vectored(&[IoSlice::new(header.as_bytes()), IoSlice::new(message)])
            .await
            .context("ring buffer error")
    }

    pub async fn read_response(&mut self) -> anyhow::Result<(Status, &[u8])> {
        let n = self
            .pipe
            .recv(&mut self.buf)
            .await
            .context("ring buffer error")?;
        let buf = &self.buf[..n];
        let (header, rest) = hyperv_ic_protocol::Header::read_from_prefix(buf)
            .ok()
            .context("missing header")?;

        if header.transaction_id != 0 || !header.flags.transaction() || !header.flags.response() {
            anyhow::bail!("invalid transaction response");
        }

        let rest = rest
            .get(..header.message_size as usize)
            .context("missing message body")?;

        Ok((header.status, rest))
    }
}
