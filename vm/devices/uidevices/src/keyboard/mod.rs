// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Vmbus synthetic keyboard device.

mod protocol;

use async_trait::async_trait;
use futures::StreamExt;
use input_core::InputSource;
use input_core::KeyboardData;
use mesh::payload::Protobuf;
use std::io::IoSlice;
use std::pin::pin;
use task_control::StopTask;
use thiserror::Error;
use vmbus_async::async_dgram::AsyncRecv;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSend;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SaveRestoreSimpleVmbusDevice;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_ring::RingMem;
use vmcore::save_restore::SavedStateRoot;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[derive(Debug)]
enum Request {
    ProtocolRequest(u32),
    SetLedIndicators,
}

#[derive(Debug, Error)]
enum Error {
    #[error("channel i/o error")]
    Io(#[source] std::io::Error),
    #[error("received out of order packet")]
    UnexpectedPacketOrder,
    #[error("bad packet")]
    BadPacket,
    #[error("unknown message type")]
    UnknownMessageType(u32),
    #[error("accepting vmbus channel")]
    Accept(#[from] vmbus_channel::offer::Error),
}

async fn recv_packet(reader: &mut impl AsyncRecv) -> Result<Request, Error> {
    let mut buf = [0; 64];
    let n = reader.recv(&mut buf).await.map_err(Error::Io)?;
    let buf = &buf[..n];
    let (header, buf) =
        protocol::MessageHeader::read_from_prefix(buf).map_err(|_| Error::BadPacket)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
    let request = match header.message_type {
        protocol::MESSAGE_PROTOCOL_REQUEST => {
            let message = protocol::MessageProtocolRequest::read_from_prefix(buf)
                .map_err(|_| Error::BadPacket)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::ProtocolRequest(message.version)
        }
        protocol::MESSAGE_SET_LED_INDICATORS => {
            // We don't have any actual LEDs to set, so check the message for validity but ignore its contents.
            let _message = protocol::MessageLedIndicatorsState::read_from_prefix(buf)
                .map_err(|_| Error::BadPacket)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::SetLedIndicators
        }
        typ => return Err(Error::UnknownMessageType(typ)),
    };
    Ok(request)
}

async fn send_packet<T: IntoBytes + Immutable + KnownLayout>(
    writer: &mut impl AsyncSend,
    typ: u32,
    packet: &T,
) -> Result<(), Error> {
    writer
        .send_vectored(&[
            IoSlice::new(protocol::MessageHeader { message_type: typ }.as_bytes()),
            IoSlice::new(packet.as_bytes()),
        ])
        .await
        .map_err(Error::Io)?;
    Ok(())
}

/// A vmbus synthetic keyboard.
pub struct Keyboard {
    source: Box<dyn InputSource<KeyboardData>>,
}

impl Keyboard {
    /// Creates a new keyboard.
    pub fn new(source: Box<dyn InputSource<KeyboardData>>) -> Self {
        Self { source }
    }

    /// Extracts the keyboard input source.
    pub fn into_source(self) -> Box<dyn InputSource<KeyboardData>> {
        self.source
    }
}

#[async_trait]
impl SimpleVmbusDevice for Keyboard {
    type Runner = KeyboardChannel<GpadlRingMem>;
    type SavedState = SavedState;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "keyboard".to_owned(),
            interface_id: protocol::INTERFACE_GUID,
            instance_id: protocol::INSTANCE_GUID,
            ..Default::default()
        }
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new_raw(channel)?;
        Ok(KeyboardChannel::new(pipe, ChannelState::default()))
    }

    async fn close(&mut self) {
        self.source.set_active(false).await;
    }

    fn inspect(&mut self, req: inspect::Request<'_>, channel: Option<&mut Self::Runner>) {
        let mut resp = req.respond();
        if let Some(channel) = channel {
            let (version, state) = match &channel.state {
                ChannelState::ReadVersion => (None, "read_version"),
                ChannelState::WriteVersion { version } => (Some(*version), "write_version"),
                ChannelState::Active { version } => (Some(*version), "active"),
            };
            resp.field("state", state).field("version", version);
        }
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        channel: &mut KeyboardChannel,
    ) -> Result<(), task_control::Cancelled> {
        stop.until_stopped(async {
            match channel.process(self).await {
                Ok(()) => {}
                Err(err) => {
                    tracing::error!(error = &err as &dyn std::error::Error, "keyboard error")
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

impl SaveRestoreSimpleVmbusDevice for Keyboard {
    fn save_open(&mut self, runner: &Self::Runner) -> Self::SavedState {
        SavedState(runner.state.clone())
    }

    fn restore_open(
        &mut self,
        state: Self::SavedState,
        channel: RawAsyncChannel<GpadlRingMem>,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new_raw(channel)?;
        Ok(KeyboardChannel::new(pipe, state.0))
    }
}

/// Keyboard saved state.
#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "ui.synthkbd")]
pub struct SavedState(#[mesh(1)] ChannelState);

/// The keyboard task.
pub struct KeyboardChannel<T: RingMem = GpadlRingMem> {
    channel: MessagePipe<T>,
    state: ChannelState,
}

#[derive(Debug, Clone, Protobuf, Default)]
#[mesh(package = "ui.synthkbd")]
enum ChannelState {
    #[mesh(1)]
    #[default]
    ReadVersion,
    #[mesh(2)]
    WriteVersion {
        #[mesh(1)]
        version: u32,
    },
    #[mesh(3)]
    Active {
        #[mesh(1)]
        version: u32,
    },
}

impl<T: RingMem + Unpin> KeyboardChannel<T> {
    fn new(channel: MessagePipe<T>, state: ChannelState) -> Self {
        Self { channel, state }
    }

    async fn process(&mut self, keyboard: &mut Keyboard) -> Result<(), Error> {
        let (mut recv, mut send) = MessagePipe::split(&mut self.channel);
        loop {
            match self.state {
                ChannelState::ReadVersion => {
                    if let Request::ProtocolRequest(version) = recv_packet(&mut recv).await? {
                        self.state = ChannelState::WriteVersion { version };
                    } else {
                        return Err(Error::UnexpectedPacketOrder);
                    }
                }
                ChannelState::WriteVersion { version } => {
                    let accepted = version == protocol::VERSION_WIN8;
                    send_packet(
                        &mut send,
                        protocol::MESSAGE_PROTOCOL_RESPONSE,
                        &protocol::MessageProtocolResponse {
                            accepted: accepted.into(),
                        },
                    )
                    .await?;
                    if accepted {
                        tracelimit::info_ratelimited!(version, "keyboard negotiated, version");
                        self.state = ChannelState::Active { version };
                    } else {
                        tracelimit::warn_ratelimited!(version, "unknown keyboard version");
                        self.state = ChannelState::ReadVersion;
                    }
                }
                ChannelState::Active { version: _ } => loop {
                    keyboard.source.set_active(true).await;
                    let send_fut = pin!(async {
                        while let Some(input) = keyboard.source.next().await {
                            let mut flags = 0;
                            match input.code >> 8 {
                                0xe0 => {
                                    flags |= protocol::KEYSTROKE_IS_E0;
                                }
                                0xe1 => {
                                    flags |= protocol::KEYSTROKE_IS_E1;
                                }
                                _ => (),
                            }
                            if !input.make {
                                flags |= protocol::KEYSTROKE_IS_BREAK;
                            }
                            send_packet(
                                &mut send,
                                protocol::MESSAGE_EVENT,
                                &protocol::MessageKeystroke {
                                    make_code: input.code & 0x7f,
                                    padding: 0,
                                    flags,
                                },
                            )
                            .await?;
                        }
                        Ok(())
                    });

                    let recv_fut = pin!(async {
                        loop {
                            match recv_packet(&mut recv).await? {
                                Request::SetLedIndicators => (),
                                _ => return Err(Error::UnexpectedPacketOrder),
                            }
                        }
                        #[expect(unreachable_code)]
                        Ok(())
                    });

                    futures::future::try_join(send_fut, recv_fut).await?;
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_core::mesh_input::input_pair;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::task::Task;
    use std::io::ErrorKind;
    use test_with_tracing::test;
    use tracing_helpers::ErrorValueExt;
    use vmbus_async::pipe::connected_raw_message_pipes;

    #[derive(Debug)]
    enum Packet {
        ProtocolResponse(protocol::MessageProtocolResponse),
        Event(protocol::MessageKeystroke),
    }

    async fn recv_packet(read: &mut (dyn AsyncRecv + Unpin + Send + Sync)) -> Option<Packet> {
        let mut packet = [0; protocol::MAXIMUM_MESSAGE_SIZE];
        let n = read.recv(&mut packet).await.unwrap();
        if n == 0 {
            return None;
        }
        let packet = &packet[..n];
        let (header, rest) = protocol::MessageHeader::read_from_prefix(packet).unwrap(); // TODO: zerocopy: unwrap (https://github.com/microsoft/openvmm/issues/759)
        Some(match header.message_type {
            protocol::MESSAGE_PROTOCOL_RESPONSE => {
                Packet::ProtocolResponse(FromBytes::read_from_prefix(rest).unwrap().0)
                // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            }
            protocol::MESSAGE_EVENT => Packet::Event(FromBytes::read_from_prefix(rest).unwrap().0), // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            _ => panic!("unknown packet type {}", header.message_type),
        })
    }

    fn start_worker<T: RingMem + 'static + Unpin + Send + Sync>(
        driver: &DefaultDriver,
        mut keyboard: Keyboard,
        channel: MessagePipe<T>,
    ) -> Task<Result<(), Error>> {
        driver.spawn("keyboard worker", async move {
            let mut channel = KeyboardChannel::new(channel, ChannelState::ReadVersion);
            channel.process(&mut keyboard).await.or_else(|e| match e {
                Error::Io(err) if err.kind() == ErrorKind::ConnectionReset => {
                    tracing::info!("closed");
                    Ok(())
                }
                _ => {
                    tracing::error!(error = e.as_error());
                    Err(e)
                }
            })
        })
    }

    #[async_test]
    async fn test_channel_working(driver: DefaultDriver) {
        let (host, mut guest) = connected_raw_message_pipes(16384);
        let (source, mut sink) = input_pair();
        let worker = start_worker(&driver, Keyboard::new(Box::new(source)), host);

        send_packet(
            &mut guest,
            protocol::MESSAGE_PROTOCOL_REQUEST,
            &protocol::MessageProtocolRequest {
                version: protocol::VERSION_WIN8,
            },
        )
        .await
        .unwrap();

        match recv_packet(&mut guest).await.unwrap() {
            Packet::ProtocolResponse(protocol::MessageProtocolResponse { accepted: 1 }) => (),
            p => panic!("unexpected {:?}", p),
        }

        let events = [(3, false), (5, true)];

        for &(code, make) in &events {
            sink.send(KeyboardData { code, make });
        }

        for event in &events {
            match recv_packet(&mut guest).await.unwrap() {
                Packet::Event(protocol::MessageKeystroke {
                    make_code,
                    padding: _padding,
                    flags,
                }) => {
                    assert_eq!(make_code, event.0);
                    assert_eq!(
                        flags,
                        if event.1 {
                            0
                        } else {
                            protocol::KEYSTROKE_IS_BREAK
                        }
                    );
                }
                p => panic!("unexpected {:?}", p),
            }
        }
        drop(guest);
        worker.await.unwrap()
    }

    #[async_test]
    async fn test_channel_negotiation_failed(driver: DefaultDriver) {
        let (host, mut guest) = connected_raw_message_pipes(16384);
        let (source, _sink) = input_pair();
        let worker = start_worker(&driver, Keyboard::new(Box::new(source)), host);

        send_packet(
            &mut guest,
            protocol::MESSAGE_PROTOCOL_REQUEST,
            &protocol::MessageProtocolRequest { version: 0xbadf00d },
        )
        .await
        .unwrap();

        match recv_packet(&mut guest).await.unwrap() {
            Packet::ProtocolResponse(protocol::MessageProtocolResponse { accepted: 0 }) => (),
            p => panic!("unexpected {:?}", p),
        }

        drop(guest);
        worker.await.unwrap();
    }
}
