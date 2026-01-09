// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest Emulation Log - GEL
//!
//! The GEL is the host side of a communication channel that uses VMBUS to
//! send logs from the guest to the host. This is an implementation to support
//! better integration testing within the OpenVMM CI.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use async_trait::async_trait;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::HashMap;
use task_control::StopTask;
use thiserror::Error;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SaveRestoreSimpleVmbusDevice;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_ring::RingMem;
use vmcore::save_restore::NoSavedState;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
enum Error {
    #[error("pipe failed")]
    PipeFailure(#[source] std::io::Error),
    #[error("trace message has invalid size {0}")]
    InvalidTraceSize(u16),
    #[error("invalid payload length {0}")]
    InvalidPayloadSize(usize),
}

/// VMBUS device that implements the host side of the Guest Emulation Log protocol.
#[non_exhaustive]
pub struct GuestEmulationLog {}

impl GuestEmulationLog {
    /// Create a new Host side GEL device.
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl SimpleVmbusDevice for GuestEmulationLog {
    type Runner = GelChannel;
    type SavedState = NoSavedState;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "gel".to_owned(),
            interface_id: get_protocol::GUEST_EMULATION_INTERFACE_TYPE,
            instance_id: get_protocol::GET_LOG_INTERFACE_GUID,
            channel_type: ChannelType::Pipe { message_mode: true },
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, task: Option<&mut GelChannel>) {
        let _ = (req, task);
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(GelChannel::new(pipe))
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        channel: &mut GelChannel,
    ) -> Result<(), task_control::Cancelled> {
        stop.until_stopped(async {
            if let Err(err) = channel.process(self).await {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "trace channel failed"
                );
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

impl SaveRestoreSimpleVmbusDevice for GuestEmulationLog {
    fn save_open(&mut self, _runner: &Self::Runner) -> Self::SavedState {
        NoSavedState
    }

    fn restore_open(
        &mut self,
        NoSavedState: Self::SavedState,
        channel: RawAsyncChannel<GpadlRingMem>,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(GelChannel::new(pipe))
    }
}

#[derive(Deserialize, Debug, Default)]
struct TraceFields<'a> {
    #[serde(borrow)]
    message: Option<Cow<'a, str>>,
    #[serde(flatten, borrow)]
    extra: ExtraFields<'a>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(transparent)]
struct ExtraFields<'a> {
    #[serde(borrow)]
    map: HashMap<Cow<'a, str>, serde_json::value::Value>,
}

impl std::fmt::Display for ExtraFields<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for (k, v) in &self.map {
            map.entry(k, &format_args!("{}", v));
        }
        map.finish()
    }
}

#[derive(Deserialize, Debug)]
struct TraceData<'a> {
    #[serde(borrow)]
    target: Cow<'a, str>,
    #[serde(borrow)]
    level: Cow<'a, str>,
    #[serde(default, borrow)]
    fields: TraceFields<'a>,
    #[serde(flatten, borrow)]
    extra: ExtraFields<'a>,
}

/// The GEL task.
pub struct GelChannel<T: RingMem = GpadlRingMem> {
    channel: MessagePipe<T>,
}

impl<T: RingMem + Unpin> GelChannel<T> {
    fn new(channel: MessagePipe<T>) -> Self {
        Self { channel }
    }

    async fn process(&mut self, _state: &mut GuestEmulationLog) -> Result<(), Error> {
        let mut buffer = vec![0; get_protocol::TRACE_LOGGING_NOTIFICATION_MAX_SIZE];
        loop {
            let n = self
                .channel
                .recv(buffer.as_mut_bytes())
                .await
                .map_err(Error::PipeFailure)?;

            if n == 0 {
                break;
            }

            let buffer = &buffer[..n];

            let (header, buffer) =
                get_protocol::TraceLoggingNotificationHeader::read_from_prefix(buffer)
                    .map_err(|_| Error::InvalidPayloadSize(n))?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

            let message = buffer
                .get(
                    header.message.offset as usize
                        ..(header.message.offset + header.message.size) as usize,
                )
                .ok_or(Error::InvalidTraceSize(n as u16))?;

            match serde_json::from_slice::<'_, TraceData<'_>>(message) {
                Ok(data) => match &*data.level {
                    "ERROR" => {
                        tracing::error!(
                            target: "paravisor_log",
                            inner_target = &*data.target,
                            message = data.fields.message.as_deref(),
                            fields = %data.fields.extra,
                            extra = %data.extra,
                        )
                    }
                    "WARN" => {
                        tracing::warn!(
                            target: "paravisor_log",
                            inner_target = &*data.target,
                            message = data.fields.message.as_deref(),
                            fields = %data.fields.extra,
                            extra = %data.extra,
                        )
                    }
                    "INFO" => {
                        tracing::info!(
                            target: "paravisor_log",
                            inner_target = &*data.target,
                            message = data.fields.message.as_deref(),
                            fields = %data.fields.extra,
                            extra = %data.extra,
                        )
                    }
                    "DEBUG" => {
                        tracing::debug!(
                            target: "paravisor_log",
                            inner_target = &*data.target,
                            message = data.fields.message.as_deref(),
                            fields = %data.fields.extra,
                            extra = %data.extra,
                        )
                    }
                    "TRACE" => {
                        tracing::trace!(
                            target: "paravisor_log",
                            inner_target = &*data.target,
                            message = data.fields.message.as_deref(),
                            fields = %data.fields.extra,
                            extra = %data.extra,
                        )
                    }
                    some_level => {
                        tracing::info!(
                            target: "paravisor_log",
                            inner_level = some_level,
                            inner_target = &*data.target,
                            message = data.fields.message.as_deref(),
                            fields = %data.fields.extra,
                            extra = %data.extra,
                        )
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        target: "paravisor_log",
                        inner_level = ?header.level,
                        error = &err as &dyn std::error::Error,
                        message = String::from_utf8_lossy(message).as_ref(),
                        "failed to parse trace",
                    )
                }
            }
        }
        Ok(())
    }
}
