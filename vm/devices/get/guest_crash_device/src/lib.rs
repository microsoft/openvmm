// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the Underhill guest crash device, used by
//! `underhill_crash` to send user-mode crash dumps to the host.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use anyhow::anyhow;
use async_trait::async_trait;
use get_protocol::crash;
use get_protocol::crash::CRASHDUMP_GUID;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::FailableRpc;
use mesh::rpc::PendingFailableRpc;
use mesh::rpc::RpcSend;
use std::fs::File;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use task_control::Cancelled;
use task_control::StopTask;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SaveRestoreSimpleVmbusDevice;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmcore::save_restore::SavedStateNotSupported;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// The crash device.
#[derive(InspectMut)]
pub struct GuestCrashDevice {
    #[inspect(skip)]
    request_dump: mesh::Sender<FailableRpc<mesh::OneshotReceiver<()>, File>>,
    max_dump_size: u64,
}

/// The internal guest crash channel.
#[derive(InspectMut)]
pub struct GuestCrashChannel {
    #[inspect(mut)]
    pipe: GuestCrashPipe,
    state: ProtocolState,
}

#[derive(InspectMut)]
struct GuestCrashPipe {
    #[inspect(flatten, mut)]
    pipe: MessagePipe<GpadlRingMem>,
}

impl GuestCrashPipe {
    fn send<T: IntoBytes + Immutable + KnownLayout>(&mut self, data: &T) -> std::io::Result<()> {
        self.pipe.try_send(data.as_bytes())
    }

    async fn recv<'a>(&mut self, data: &'a mut [u8]) -> std::io::Result<&'a [u8]> {
        let n = self.pipe.recv(data).await?;
        Ok(&data[..n])
    }

    async fn recv_message<'a>(
        &mut self,
        data: &'a mut [u8],
    ) -> anyhow::Result<(crash::Header, &'a [u8])> {
        let message = self.recv(data).await?;
        let header = crash::Header::read_from_prefix(message)
            .map_err(|_| anyhow!("truncated message"))?
            .0;
        Ok((header, message))
    }
}

enum ProtocolState {
    Init,
    DumpRequested {
        activity_id: Guid,
        done: mesh::OneshotSender<()>,
        state: DumpState,
    },
    Failed {
        activity_id: Guid,
    },
}

enum DumpState {
    OpeningFile {
        recv: PendingFailableRpc<File>,
    },
    Writing {
        file: File,
        payload: Option<(u64, u32)>,
    },
}

impl Inspect for ProtocolState {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.ignore(); // TODO
    }
}

impl GuestCrashDevice {
    /// Makes a new crash device.
    ///
    /// When the guest requests a crash dump, the device will send a request to
    /// `request_dump` to retrieve the file to write to. When the dump completes
    /// successfully, the device will send an empty message to the provided
    /// oneshot channel.
    pub fn new(
        request_dump: mesh::Sender<FailableRpc<mesh::OneshotReceiver<()>, File>>,
        max_dump_size: u64,
    ) -> Self {
        Self {
            request_dump,
            max_dump_size,
        }
    }

    /// Deconstructs the object, returning the original resources passed to
    /// `new`.
    pub fn into_inner(
        self,
    ) -> (
        mesh::Sender<FailableRpc<mesh::OneshotReceiver<()>, File>>,
        u64,
    ) {
        (self.request_dump, self.max_dump_size)
    }
}

#[async_trait]
impl SimpleVmbusDevice for GuestCrashDevice {
    type SavedState = SavedStateNotSupported;
    type Runner = GuestCrashChannel;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "guest_crash".into(),
            instance_id: CRASHDUMP_GUID,
            interface_id: CRASHDUMP_GUID,
            channel_type: vmbus_channel::bus::ChannelType::Pipe { message_mode: true },
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, runner: Option<&mut Self::Runner>) {
        req.respond().merge(self).merge(runner);
    }

    fn open(
        &mut self,
        channel: vmbus_channel::RawAsyncChannel<GpadlRingMem>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(GuestCrashChannel {
            pipe: GuestCrashPipe { pipe },
            state: ProtocolState::Init,
        })
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut Self::Runner,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(self.process(runner)).await
    }

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn SaveRestoreSimpleVmbusDevice<SavedState = Self::SavedState, Runner = Self::Runner>,
    > {
        None
    }
}

impl GuestCrashDevice {
    async fn process(&mut self, channel: &mut GuestCrashChannel) {
        if let Err(err) = self.process_inner(channel).await {
            tracing::error!(
                error = err.as_ref() as &dyn std::error::Error,
                "guest crash failure"
            );
        }
    }

    async fn process_inner(&mut self, channel: &mut GuestCrashChannel) -> anyhow::Result<()> {
        let mut buffer = vec![0; 16384];
        loop {
            channel.pipe.pipe.wait_write_ready(256).await?;

            match &mut channel.state {
                ProtocolState::Init => {
                    let (header, _message) = channel.pipe.recv_message(&mut buffer).await?;
                    match header.message_type {
                        crash::MessageType::REQUEST_GET_CAPABILITIES_V1 => {
                            channel.pipe.send(&crash::DumpCapabilitiesResponseV1 {
                                header: crash::Header {
                                    message_type: crash::MessageType::RESPONSE_GET_CAPABILITIES_V1,
                                    ..header
                                },
                                capabilities: crash::Capabilities::new().with_linux_config_v1(true),
                            })?;
                        }
                        crash::MessageType::REQUEST_GET_NIX_DUMP_CONFIG_V1 => {
                            channel.pipe.send(&crash::DumpConfigResponseV1 {
                                header: crash::Header {
                                    message_type:
                                        crash::MessageType::RESPONSE_GET_NIX_DUMP_CONFIG_V1,
                                    ..header
                                },
                                config: crash::ConfigV1 {
                                    max_dump_size: self.max_dump_size,
                                    dump_type: crash::DumpType::ELF,
                                },
                            })?;
                        }
                        crash::MessageType::REQUEST_NIX_DUMP_START_V1 => {
                            let (send, recv) = mesh::oneshot();
                            let recv = self.request_dump.call_failable(|x| x, recv);
                            channel.state = ProtocolState::DumpRequested {
                                activity_id: header.activity_id,
                                done: send,
                                state: DumpState::OpeningFile { recv },
                            };
                        }
                        message_type => anyhow::bail!("invalid message type {message_type:?}"),
                    }
                }
                &mut ProtocolState::DumpRequested {
                    state: ref mut state @ DumpState::OpeningFile { .. },
                    activity_id,
                    ..
                } => {
                    let DumpState::OpeningFile { recv } = state else {
                        unreachable!()
                    };
                    let status = match recv.await {
                        Ok(file) => {
                            *state = DumpState::Writing {
                                file,
                                payload: None,
                            };
                            0
                        }
                        Err(err) => {
                            channel.state = ProtocolState::Failed { activity_id };
                            tracing::error!(
                                err = &err as &dyn std::error::Error,
                                "failed to open crash dump file"
                            );
                            -1
                        }
                    };
                    channel.pipe.send(&crash::DumpStartResponseV1 {
                        header: crash::Header {
                            message_type: crash::MessageType::RESPONSE_NIX_DUMP_START_V1,
                            activity_id,
                        },
                        status,
                    })?;
                    continue;
                }
                &mut ProtocolState::DumpRequested {
                    state:
                        DumpState::Writing {
                            ref mut file,
                            ref mut payload,
                            ..
                        },
                    activity_id,
                    ..
                } => {
                    if let Some((offset, size)) = *payload {
                        // Read the payload message.
                        let message = channel.pipe.recv(&mut buffer).await?;
                        if size as usize != message.len() {
                            anyhow::bail!("size mismatch");
                        }
                        if self.max_dump_size < offset || self.max_dump_size - offset < size as u64
                        {
                            anyhow::bail!("dump file out of range");
                        }

                        match file
                            .seek(SeekFrom::Start(offset))
                            .and_then(|_| file.write_all(message))
                        {
                            Ok(()) => {
                                *payload = None;
                            }
                            Err(err) => {
                                tracing::error!(
                                    error = &err as &dyn std::error::Error,
                                    "failed to write crash data"
                                );
                                channel.pipe.send(&crash::DumpWriteResponseV1 {
                                    header: crash::Header {
                                        activity_id,
                                        message_type:
                                            crash::MessageType::RESPONSE_NIX_DUMP_WRITE_V1,
                                    },
                                    status: -1,
                                })?;
                                channel.state = ProtocolState::Failed { activity_id };
                            }
                        }
                    } else {
                        let (header, message) = channel.pipe.recv_message(&mut buffer).await?;
                        match header.message_type {
                            crash::MessageType::REQUEST_NIX_DUMP_WRITE_V1 => {
                                let request = crash::DumpWriteRequestV1::read_from_prefix(message)
                                    .map_err(|_| anyhow!("truncated message"))? // TODO: zerocopy: anyhow! (https://github.com/microsoft/openvmm/issues/759)
                                    .0;
                                *payload = Some((request.offset, request.size));
                            }
                            crash::MessageType::REQUEST_NIX_DUMP_COMPLETE_V1 => {
                                // Notify the VMM that the crash is done being written.
                                let ProtocolState::DumpRequested { done, .. } =
                                    std::mem::replace(&mut channel.state, ProtocolState::Init)
                                else {
                                    unreachable!()
                                };
                                done.send(());
                            }
                            message_type => anyhow::bail!("invalid message type {message_type:?}"),
                        }
                    }
                }
                &mut ProtocolState::Failed { activity_id } => {
                    let (header, _message) = channel.pipe.recv_message(&mut buffer).await?;
                    match header.message_type {
                        crash::MessageType::REQUEST_NIX_DUMP_WRITE_V1 => {
                            channel.pipe.send(&crash::DumpWriteResponseV1 {
                                header: crash::Header {
                                    activity_id,
                                    message_type: crash::MessageType::RESPONSE_NIX_DUMP_WRITE_V1,
                                },
                                status: -1,
                            })?;
                        }
                        crash::MessageType::REQUEST_NIX_DUMP_COMPLETE_V1 => {}
                        message_type => anyhow::bail!("invalid message type {message_type:?}"),
                    }
                }
            }
        }
    }
}
