// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module contains headers, constants, and structs pertinent to the Mouse device
mod protocol;

use async_trait::async_trait;
use futures::StreamExt;
use input_core::InputSource;
use input_core::MouseData;
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
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SaveRestoreSimpleVmbusDevice;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_ring::RingMem;
use vmcore::save_restore::SavedStateRoot;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

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

enum Request {
    ProtocolRequest(u32),
    DeviceInfoAck,
}

//HID consts- specific to setting up a HID mouse device
const HID_DEVICE_ATTRIBUTES: protocol::HidAttributes = protocol::HidAttributes {
    size: size_of::<protocol::HidAttributes>() as u32,
    vendor_id: protocol::HID_VENDOR_ID,
    product_id: protocol::HID_PRODUCT_ID,
    version_id: protocol::HID_VERSION_ID,
    padding: [0; 11],
};

const HID_DESCRIPTOR: protocol::HidDescriptor = protocol::HidDescriptor {
    length: size_of::<protocol::HidDescriptor>() as u8,
    descriptor_type: 0x21,
    hid: 0x101,
    country: 0x00,
    num_descriptors: 1,
    descriptor_list: protocol::HidDescriptorList {
        report_type: 0x22,
        report_length: 67,
    },
};

const MSG_DEVICE_INFO_LENGTH: u32 = size_of::<protocol::HidAttributes>() as u32
    + size_of::<protocol::HidDescriptor>() as u32
    + HID_DESCRIPTOR.descriptor_list.report_length as u32;

async fn recv_packet(reader: &mut (impl AsyncRecv + Unpin)) -> Result<Request, Error> {
    let mut buf = [0; 64];
    let n = match reader.recv(&mut buf).await {
        Ok(n) => n,
        Err(e) => return Err(Error::Io(e)),
    };

    let buf = &buf[..n];
    let (header, buf) =
        protocol::MessageHeader::read_from_prefix(buf).map_err(|_| Error::BadPacket)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
    let request = match header.message_type {
        protocol::SYNTHHID_PROTOCOL_REQUEST => {
            let message = protocol::MessageProtocolRequest::read_from_prefix(buf)
                .map_err(|_| Error::BadPacket)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::ProtocolRequest(message.version)
        }
        protocol::SYNTHHID_INIT_DEVICE_INFO_ACK => {
            // We don't need the message contents, but we do still want to ensure it's valid.
            let _message = protocol::MessageDeviceInfoAck::read_from_prefix(buf)
                .map_err(|_| Error::BadPacket)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::DeviceInfoAck
        }
        typ => return Err(Error::UnknownMessageType(typ)),
    };
    Ok(request)
}

async fn send_packet<T: IntoBytes + Immutable + KnownLayout>(
    writer: &mut (impl AsyncSend + Unpin),
    typ: u32,
    size: u32,
    packet: &T,
) -> Result<(), Error> {
    match writer
        .send_vectored(&[
            IoSlice::new(
                protocol::MessageHeader {
                    message_type: typ,
                    message_size: size,
                }
                .as_bytes(),
            ),
            IoSlice::new(packet.as_bytes()),
        ])
        .await
    {
        Ok(_) => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Vmbus synthetic mouse device.
pub struct Mouse {
    source: Box<dyn InputSource<MouseData>>,
}

impl Mouse {
    /// Creates a new mouse device.
    pub fn new(source: Box<dyn InputSource<MouseData>>) -> Self {
        Self { source }
    }

    /// Extracts the mouse input receiver.
    pub fn into_source(self) -> Box<dyn InputSource<MouseData>> {
        self.source
    }
}

#[derive(Debug, Clone, Protobuf, Default)]
#[mesh(package = "ui.synthmouse")]
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
    SendDeviceInfo {
        #[mesh(1)]
        version: u32,
    },
    #[mesh(4)]
    ReadDeviceInfoAck {
        #[mesh(1)]
        version: u32,
    },
    #[mesh(5)]
    Active {
        #[mesh(1)]
        version: u32,
    },
}

/// Mouse saved state.
#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "ui.synthmouse")]
pub struct SavedState(#[mesh(1)] ChannelState);

/// The mouse task.
pub struct MouseChannel<T: RingMem = GpadlRingMem> {
    channel: MessagePipe<T>,
    state: ChannelState,
}

#[async_trait]
impl SimpleVmbusDevice for Mouse {
    type Runner = MouseChannel;
    type SavedState = SavedState;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "mouse".to_owned(),
            interface_id: protocol::INTERFACE_GUID,
            instance_id: protocol::INSTANCE_GUID,
            channel_type: ChannelType::Device { pipe_packets: true },
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, channel: Option<&mut MouseChannel>) {
        let mut resp = req.respond();
        if let Some(channel) = channel {
            let (version, state) = match &channel.state {
                ChannelState::ReadVersion => (None, "read_version"),
                ChannelState::WriteVersion { version } => (Some(*version), "write_version"),
                ChannelState::SendDeviceInfo { version } => (Some(*version), "send_device_info"),
                ChannelState::ReadDeviceInfoAck { version } => {
                    (Some(*version), "read_device_info_ack")
                }
                ChannelState::Active { version } => (Some(*version), "active"),
            };
            resp.field("state", state).field("version", version);
        }
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(MouseChannel::new(pipe, ChannelState::default()))
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        channel: &mut MouseChannel,
    ) -> Result<(), task_control::Cancelled> {
        stop.until_stopped(async {
            match channel.process(self).await {
                Ok(()) => {}
                Err(err) => tracing::error!(error = &err as &dyn std::error::Error, "mouse error"),
            }
        })
        .await
    }

    async fn close(&mut self) {
        self.source.set_active(false).await;
    }

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn SaveRestoreSimpleVmbusDevice<SavedState = Self::SavedState, Runner = Self::Runner>,
    > {
        Some(self)
    }
}

impl SaveRestoreSimpleVmbusDevice for Mouse {
    fn save_open(&mut self, runner: &Self::Runner) -> Self::SavedState {
        SavedState(runner.state.clone())
    }

    fn restore_open(
        &mut self,
        state: Self::SavedState,
        channel: RawAsyncChannel<GpadlRingMem>,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(MouseChannel::new(pipe, state.0))
    }
}

impl<T: RingMem + Unpin> MouseChannel<T> {
    fn new(channel: MessagePipe<T>, state: ChannelState) -> Self {
        Self { channel, state }
    }

    //responds to input from the VNC server and sends mouse information to the guest
    async fn process(&mut self, mouse: &mut Mouse) -> Result<(), Error> {
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
                    let accepted = version == protocol::SYNTHHID_INPUT_VERSION;
                    send_packet(
                        &mut send,
                        protocol::SYNTHHID_PROTOCOL_RESPONSE,
                        size_of::<protocol::MessageProtocolResponse>() as u32,
                        &protocol::MessageProtocolResponse {
                            version_requested: version,
                            accepted: accepted.into(),
                        },
                    )
                    .await?;
                    if accepted {
                        tracelimit::info_ratelimited!(version, "mouse negotiated");
                        self.state = ChannelState::SendDeviceInfo { version };
                    } else {
                        tracelimit::warn_ratelimited!(version, "unknown mouse version");
                        self.state = ChannelState::ReadVersion;
                    }
                }
                ChannelState::SendDeviceInfo { version } => {
                    let mut aligned_report_descriptor = [0u8; 128];
                    aligned_report_descriptor[..67].copy_from_slice(&protocol::REPORT_DESCRIPTOR);
                    let device_info_packet = protocol::MessageDeviceInfo {
                        device_attributes: HID_DEVICE_ATTRIBUTES,
                        descriptor_info: HID_DESCRIPTOR,
                        report_descriptor: aligned_report_descriptor,
                    };
                    send_packet(
                        &mut send,
                        protocol::SYNTHHID_INIT_DEVICE_INFO,
                        MSG_DEVICE_INFO_LENGTH,
                        &device_info_packet,
                    )
                    .await?;
                    self.state = ChannelState::ReadDeviceInfoAck { version };
                }
                ChannelState::ReadDeviceInfoAck { version } => {
                    if !matches!(recv_packet(&mut recv).await?, Request::DeviceInfoAck) {
                        return Err(Error::UnexpectedPacketOrder);
                    }
                    tracelimit::info_ratelimited!("mouse HID device info sent and acknowledged");
                    self.state = ChannelState::Active { version };
                }
                ChannelState::Active { version: _ } => {
                    mouse.source.set_active(true).await;
                    let send_fut = pin!(async {
                        while let Some(mouse_data) = mouse.source.next().await {
                            post_mouse_packet(mouse_data, &mut send).await?;
                        }
                        Ok(())
                    });
                    let recv_fut = pin!(async {
                        recv_packet(&mut recv).await?;
                        Result::<(), _>::Err(Error::UnexpectedPacketOrder)
                    });

                    futures::future::try_join(send_fut, recv_fut).await?;
                }
            }
        }
    }
}

// Transforms MouseData from the vnc server to an HID input report (mouse packet) by scaling coordinates and marking button flags
async fn post_mouse_packet(
    mouse_data: MouseData,
    channel: &mut (impl AsyncSend + Unpin),
) -> Result<(), Error> {
    let mut scrolled = protocol::ScrollType::NoChange;
    let mut mouse_packet: protocol::MousePacket = FromZeros::new_zeroed();
    mouse_packet.x = mouse_data.x;
    mouse_packet.y = mouse_data.y;

    let button_masks = [
        protocol::HID_MOUSE_BUTTON_LEFT,
        protocol::HID_MOUSE_BUTTON_MIDDLE,
        protocol::HID_MOUSE_BUTTON_RIGHT,
    ];

    #[expect(clippy::needless_range_loop)] // rare case of a clippy misfire
    for i in 0..protocol::MOUSE_NUMBER_BUTTONS {
        if ((1u8 << i) & mouse_data.button_mask) == (1u8 << i) {
            if i < 3 {
                mouse_packet.button_data |= button_masks[i];
            }
            if i == 3 {
                //button 4 is a mouse wheel up click
                scrolled = protocol::ScrollType::Up;
            }
            if i == 4 {
                //button 5 is a mouse wheel down click
                scrolled = protocol::ScrollType::Down;
            }
        }
    }

    //b/c we want to use the ScrollType enum to move the z in a + or - direction, we cast it into an i16
    if scrolled as i16 != 0 {
        mouse_packet.z = scrolled as i16;
    }
    send_packet(
        channel,
        protocol::SYNTHHID_PROTOCOL_INPUT_REPORT,
        size_of::<protocol::MessageInputReport>() as u32,
        &mouse_packet,
    )
    .await
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
    use vmbus_async::pipe::connected_message_pipes;

    #[derive(Debug)]
    enum Packet {
        ProtocolResponse(protocol::MessageProtocolResponse),
        DeviceInfo(protocol::MessageDeviceInfo),
    }

    async fn recv_packet(read: &mut (dyn AsyncRecv + Unpin + Send)) -> Option<Packet> {
        let mut packet = [0; 256];
        let n = read.recv(&mut packet).await.unwrap();
        if n == 0 {
            return None;
        }
        let packet = &packet[..n];
        let (header, rest) = protocol::MessageHeader::read_from_prefix(packet).unwrap(); // TODO: zerocopy: unwrap (https://github.com/microsoft/openvmm/issues/759)
        Some(match header.message_type {
            protocol::SYNTHHID_PROTOCOL_RESPONSE => {
                Packet::ProtocolResponse(FromBytes::read_from_prefix(rest).unwrap().0)
                // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            }
            protocol::SYNTHHID_INIT_DEVICE_INFO => {
                Packet::DeviceInfo(FromBytes::read_from_prefix(rest).unwrap().0)
                // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            }
            _ => panic!("unknown packet type {}", header.message_type),
        })
    }

    fn start_worker<T: RingMem + 'static + Unpin + Send + Sync>(
        driver: &DefaultDriver,
        mut mouse: Mouse,
        channel: MessagePipe<T>,
    ) -> Task<Result<(), Error>> {
        driver.spawn("mouse worker", async move {
            MouseChannel::new(channel, Default::default())
                .process(&mut mouse)
                .await
                .or_else(|e| match e {
                    Error::Io(err) if err.kind() == ErrorKind::ConnectionReset => Ok(()),
                    _ => Err(e),
                })
        })
    }

    #[async_test]
    async fn test_channel_working(driver: DefaultDriver) {
        let (host, mut guest) = connected_message_pipes(16384);
        let (source, _sink) = input_pair();
        let worker = start_worker(&driver, Mouse::new(Box::new(source)), host);

        send_packet(
            &mut guest,
            protocol::SYNTHHID_PROTOCOL_REQUEST,
            size_of::<protocol::MessageProtocolRequest>() as u32,
            &protocol::MessageProtocolRequest {
                version: protocol::SYNTHHID_INPUT_VERSION,
            },
        )
        .await
        .unwrap();

        match recv_packet(&mut guest).await.unwrap() {
            Packet::ProtocolResponse(protocol::MessageProtocolResponse {
                version_requested: protocol::SYNTHHID_INPUT_VERSION,
                accepted: 1,
            }) => (),
            p => panic!("unexpected {:?}", p),
        }

        match recv_packet(&mut guest).await.unwrap() {
            Packet::DeviceInfo(protocol::MessageDeviceInfo {
                device_attributes: _,
                descriptor_info: _,
                report_descriptor: _,
            }) => (),
            p => panic!("unexpected {:?}", p),
        }

        drop(guest);
        worker.await.unwrap();
    }

    #[async_test]
    async fn test_channel_negotiation_failed(driver: DefaultDriver) {
        let (host, mut guest) = connected_message_pipes(16384);
        let (source, _sink) = input_pair();
        let worker = start_worker(&driver, Mouse::new(Box::new(source)), host);

        send_packet(
            &mut guest,
            protocol::SYNTHHID_PROTOCOL_REQUEST,
            size_of::<protocol::MessageProtocolRequest>() as u32,
            &protocol::MessageProtocolRequest { version: 0xbadf00d },
        )
        .await
        .unwrap();

        let mut failed = false;
        match recv_packet(&mut guest).await.unwrap() {
            Packet::ProtocolResponse(protocol::MessageProtocolResponse {
                version_requested: protocol::SYNTHHID_INPUT_VERSION,
                accepted: 0,
            }) => (),
            _ => failed = true,
        }

        assert_eq!(failed, true);

        drop(guest);
        worker.await.unwrap();
    }
}
