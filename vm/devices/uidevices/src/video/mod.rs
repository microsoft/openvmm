// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A vmbus synthetic video device.

mod protocol;

use async_trait::async_trait;
use guestmem::AccessError;
use guid::Guid;
use mesh::payload::Protobuf;
use std::io::IoSlice;
use task_control::StopTask;
use thiserror::Error;
use video_core::FramebufferControl;
use video_core::FramebufferFormat;
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
use vmcore::save_restore::SavedStateRoot;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Ref;

#[derive(Debug, Error)]
enum Error {
    #[error("out of order packet")]
    UnexpectedPacketOrder,
    #[error("memory access error")]
    Access(#[from] AccessError),
    #[error("unknown message type: {0:#x}")]
    UnknownMessageType(u32),
    #[error("invalid packet")]
    InvalidPacket,
    #[error("channel i/o error")]
    Io(#[source] std::io::Error),
    #[error("failed to accept vmbus channel")]
    Accept(#[from] vmbus_channel::offer::Error),
}

#[derive(Debug)]
enum Request {
    Version(protocol::Version),
    VramLocation {
        user_context: u64,
        address: Option<u64>,
    },
    SituationUpdate {
        user_context: u64,
        situation: protocol::VideoOutputSituation,
    },
    PointerPosition {
        is_visible: bool,
        x: i32,
        y: i32,
    },
    PointerShape,
    Dirt(Vec<protocol::Rectangle>),
    BiosInfo,
    SupportedResolutions {
        maximum_count: u8,
    },
    Capability,
}

fn parse_packet(buf: &[u8]) -> Result<Request, Error> {
    let (header, buf) =
        Ref::<_, protocol::MessageHeader>::from_prefix(buf).map_err(|_| Error::InvalidPacket)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
    let request = match header.typ.to_ne() {
        protocol::MESSAGE_VERSION_REQUEST => {
            let message = protocol::VersionRequestMessage::ref_from_prefix(buf)
                .map_err(|_| Error::InvalidPacket)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::Version(message.version)
        }
        protocol::MESSAGE_VRAM_LOCATION => {
            let message = protocol::VramLocationMessage::ref_from_prefix(buf)
                .map_err(|_| Error::InvalidPacket)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            let address = if message.is_vram_gpa_address_specified != 0 {
                Some(message.vram_gpa_address.into())
            } else {
                None
            };
            Request::VramLocation {
                user_context: message.user_context.into(),
                address,
            }
        }
        protocol::MESSAGE_SITUATION_UPDATE => {
            let message = protocol::SituationUpdateMessage::ref_from_prefix(buf)
                .map_err(|_| Error::InvalidPacket)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::SituationUpdate {
                user_context: message.user_context.into(),
                situation: message.video_output,
            }
        }
        protocol::MESSAGE_POINTER_POSITION => {
            let message = protocol::PointerPositionMessage::ref_from_prefix(buf)
                .map_err(|_| Error::InvalidPacket)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::PointerPosition {
                is_visible: message.is_visible != 0,
                x: message.image_x.into(),
                y: message.image_y.into(),
            }
        }
        protocol::MESSAGE_POINTER_SHAPE => {
            //let message = protocol::PointerShapeMessage::from_bytes(buf).map_err(|_| Error::InvalidPacket)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::PointerShape
        }
        protocol::MESSAGE_DIRT => {
            let (message, buf) = Ref::<_, protocol::DirtMessage>::from_prefix(buf)
                .map_err(|_| Error::InvalidPacket)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::Dirt(
                <[protocol::Rectangle]>::ref_from_prefix_with_elems(
                    buf,
                    message.dirt_count as usize,
                )
                .map_err(|_| Error::InvalidPacket)? // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                .0
                .into(),
            )
        }
        protocol::MESSAGE_BIOS_INFO_REQUEST => Request::BiosInfo,
        protocol::MESSAGE_SUPPORTED_RESOLUTIONS_REQUEST => {
            let message = protocol::SupportedResolutionsRequestMessage::ref_from_prefix(buf)
                .map_err(|_| Error::InvalidPacket)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            Request::SupportedResolutions {
                maximum_count: message.maximum_resolution_count,
            }
        }
        protocol::MESSAGE_CAPABILITY_REQUEST => Request::Capability,
        typ => return Err(Error::UnknownMessageType(typ)),
    };
    Ok(request)
}

/// Vmbus synthetic video device.
pub struct Video {
    control: Box<dyn FramebufferControl>,
}

impl Video {
    /// Creates a new video device.
    pub fn new(control: Box<dyn FramebufferControl>) -> anyhow::Result<Self> {
        Ok(Self { control })
    }
}

/// The video device saved state.
#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "ui.synthvid")]
pub struct SavedState(ChannelState);

/// The video task.
pub struct VideoChannel {
    channel: MessagePipe<GpadlRingMem>,
    state: ChannelState,
    packet_buf: PacketBuffer,
}

#[derive(Debug, Copy, Clone, Protobuf)]
#[mesh(package = "ui.synthvid")]
struct Version {
    #[mesh(1)]
    major: u16,
    #[mesh(2)]
    minor: u16,
}

impl From<protocol::Version> for Version {
    fn from(version: protocol::Version) -> Self {
        Self {
            major: version.major(),
            minor: version.minor(),
        }
    }
}

impl From<Version> for protocol::Version {
    fn from(version: Version) -> Self {
        Self::new(version.major, version.minor)
    }
}

#[derive(Debug, Clone, Protobuf, Default)]
#[mesh(package = "ui.synthvid")]
enum ChannelState {
    #[mesh(1)]
    #[default]
    ReadVersion,
    #[mesh(2)]
    WriteVersion {
        #[mesh(1)]
        version: Version,
    },
    #[mesh(3)]
    Active {
        #[mesh(1)]
        version: Version,
        #[mesh(2)]
        substate: ActiveState,
    },
}

#[derive(Debug, Clone, Protobuf)]
#[mesh(package = "ui.synthvid")]
enum ActiveState {
    #[mesh(1)]
    ReadRequest,
    #[mesh(2)]
    SendVramAck {
        #[mesh(1)]
        user_context: u64,
    },
    #[mesh(3)]
    SendSituationUpdateAck {
        #[mesh(1)]
        user_context: u64,
    },
    #[mesh(4)]
    SendBiosInfo,
    #[mesh(5)]
    SendSupportedResolutions {
        #[mesh(1)]
        maximum_count: u8,
    },
    #[mesh(6)]
    SendCapability,
}

struct PacketBuffer {
    buf: Vec<u8>,
}

impl PacketBuffer {
    fn new() -> Self {
        Self {
            buf: vec![0; protocol::MAX_VMBUS_PACKET_SIZE],
        }
    }

    async fn recv_packet(
        &mut self,
        reader: &mut (impl AsyncRecv + Unpin),
    ) -> Result<Request, Error> {
        let n = match reader.recv(&mut self.buf).await {
            Ok(n) => n,
            Err(e) => return Err(Error::Io(e)),
        };
        let buf = &self.buf[..n];
        parse_packet(buf)
    }
}

#[async_trait]
impl SimpleVmbusDevice for Video {
    type Runner = VideoChannel;
    type SavedState = SavedState;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "video".to_owned(),
            interface_id: Guid {
                data1: 0xda0a7802,
                data2: 0xe377,
                data3: 0x4aac,
                data4: [0x8e, 0x77, 0x5, 0x58, 0xeb, 0x10, 0x73, 0xf8],
            },
            instance_id: Guid {
                data1: 0x5620e0c7,
                data2: 0x8062,
                data3: 0x4dce,
                data4: [0xae, 0xb7, 0x52, 0xc, 0x7e, 0xf7, 0x61, 0x71],
            },
            mmio_megabytes: 8,
            channel_type: ChannelType::Device { pipe_packets: true },
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, task: Option<&mut VideoChannel>) {
        let mut resp = req.respond();
        if let Some(this) = task {
            let (version, state) = match &this.state {
                ChannelState::ReadVersion => (None, "read_version"),
                ChannelState::WriteVersion { version } => (Some(*version), "write_version"),
                ChannelState::Active { version, substate } => (
                    Some(*version),
                    match substate {
                        ActiveState::ReadRequest => "read_request",
                        ActiveState::SendVramAck { .. } => "send_vram_ack",
                        ActiveState::SendSituationUpdateAck { .. } => "send_situation_update_ack",
                        ActiveState::SendBiosInfo => "send_bios_info",
                        ActiveState::SendSupportedResolutions { .. } => {
                            "send_supported_resolutions"
                        }
                        ActiveState::SendCapability => "send_capability",
                    },
                ),
            };
            resp.field("state", state)
                .field(
                    "version",
                    version.map(|v| format!("{}.{}", v.major, v.minor)),
                )
                .field_mut("channel", &mut this.channel);
        }
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(VideoChannel::new(pipe, ChannelState::default()))
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        channel: &mut VideoChannel,
    ) -> Result<(), task_control::Cancelled> {
        stop.until_stopped(async {
            match channel.process(&mut self.control).await {
                Ok(()) => {}
                Err(err) => tracing::error!(error = &err as &dyn std::error::Error, "video error"),
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

impl SaveRestoreSimpleVmbusDevice for Video {
    fn save_open(&mut self, runner: &Self::Runner) -> Self::SavedState {
        SavedState(runner.state.clone())
    }

    fn restore_open(
        &mut self,
        state: Self::SavedState,
        channel: RawAsyncChannel<GpadlRingMem>,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(VideoChannel::new(pipe, state.0))
    }
}

impl VideoChannel {
    fn new(channel: MessagePipe<GpadlRingMem>, state: ChannelState) -> Self {
        Self {
            channel,
            state,
            packet_buf: PacketBuffer::new(),
        }
    }

    async fn send_packet<T: IntoBytes + ?Sized + Immutable + KnownLayout>(
        writer: &mut (impl AsyncSend + Unpin),
        typ: u32,
        packet: &T,
    ) -> Result<(), Error> {
        let header = protocol::MessageHeader {
            typ: typ.into(),
            size: (size_of_val(packet) as u32).into(),
        };
        writer
            .send_vectored(&[
                IoSlice::new(header.as_bytes()),
                IoSlice::new(packet.as_bytes()),
            ])
            .await
            .map_err(Error::Io)?;

        Ok(())
    }

    async fn process(
        &mut self,
        framebuffer: &mut Box<dyn FramebufferControl>,
    ) -> Result<(), Error> {
        let mut channel = &mut self.channel;
        loop {
            match &mut self.state {
                ChannelState::ReadVersion => {
                    let version = if let Request::Version(version) =
                        self.packet_buf.recv_packet(&mut channel).await?
                    {
                        version.into()
                    } else {
                        return Err(Error::UnexpectedPacketOrder);
                    };
                    self.state = ChannelState::WriteVersion { version };
                }
                ChannelState::WriteVersion { version } => {
                    let server_version = Version {
                        major: protocol::VERSION_MAJOR,
                        minor: protocol::VERSION_MINOR_BLUE,
                    };
                    let is_accepted = if version.major == server_version.major {
                        protocol::ACCEPTED_WITH_VERSION_EXCHANGE
                    } else {
                        0
                    };
                    Self::send_packet(
                        &mut channel,
                        protocol::MESSAGE_VERSION_RESPONSE,
                        &protocol::VersionResponseMessage {
                            version: (*version).into(),
                            is_accepted,
                            max_video_outputs: 1,
                        },
                    )
                    .await?;
                    if is_accepted != 0 {
                        tracelimit::info_ratelimited!(?version, "video negotiation succeeded");
                        self.state = ChannelState::Active {
                            version: *version,
                            substate: ActiveState::ReadRequest,
                        };
                    } else {
                        tracelimit::warn_ratelimited!(?version, "video negotiation failed");
                        self.state = ChannelState::ReadVersion;
                    }
                }
                ChannelState::Active {
                    version: _,
                    substate,
                } => {
                    match *substate {
                        ActiveState::ReadRequest => {
                            let packet = self.packet_buf.recv_packet(&mut channel).await?;
                            match packet {
                                Request::VramLocation {
                                    user_context,
                                    address,
                                } => {
                                    framebuffer.unmap().await;
                                    if let Some(address) = address {
                                        // N.B. The mapping is preserved until explicitly torn
                                        //      down--UEFI may open the channel, establish the
                                        //      mapping, close the channel, and expect the guest
                                        //      to continue to render to the framebuffer.
                                        framebuffer.map(address).await;
                                    }
                                    *substate = ActiveState::SendVramAck { user_context };
                                }
                                Request::SituationUpdate {
                                    user_context,
                                    situation,
                                } => {
                                    framebuffer
                                        .set_format(FramebufferFormat {
                                            width: u32::from(situation.width_pixels) as usize,
                                            height: u32::from(situation.height_pixels) as usize,
                                            bytes_per_line: u32::from(situation.pitch_bytes)
                                                as usize,
                                            offset: u32::from(situation.primary_surface_vram_offset)
                                                as usize,
                                        })
                                        .await;
                                    *substate =
                                        ActiveState::SendSituationUpdateAck { user_context };
                                }
                                Request::PointerPosition { is_visible, x, y } => {
                                    let _ = (is_visible, x, y);
                                }
                                Request::PointerShape => {}
                                Request::Dirt(_rects) => {
                                    // TODO: Support dirt requests
                                }
                                Request::BiosInfo => {
                                    *substate = ActiveState::SendBiosInfo;
                                }
                                Request::SupportedResolutions { maximum_count } => {
                                    *substate =
                                        ActiveState::SendSupportedResolutions { maximum_count };
                                }
                                Request::Capability => {
                                    *substate = ActiveState::SendCapability;
                                }
                                Request::Version(_) => return Err(Error::UnexpectedPacketOrder),
                            }
                        }
                        ActiveState::SendVramAck { user_context } => {
                            Self::send_packet(
                                &mut channel,
                                protocol::MESSAGE_VRAM_LOCATION_ACK,
                                &protocol::VramLocationAckMessage {
                                    user_context: user_context.into(),
                                },
                            )
                            .await?;
                            *substate = ActiveState::ReadRequest;
                        }
                        ActiveState::SendSituationUpdateAck { user_context } => {
                            Self::send_packet(
                                &mut channel,
                                protocol::MESSAGE_SITUATION_UPDATE_ACK,
                                &protocol::SituationUpdateAckMessage {
                                    user_context: user_context.into(),
                                },
                            )
                            .await?;
                            *substate = ActiveState::ReadRequest;
                        }
                        ActiveState::SendBiosInfo => {
                            Self::send_packet(
                                &mut channel,
                                protocol::MESSAGE_BIOS_INFO_RESPONSE,
                                &protocol::BiosInfoResponseMessage {
                                    stop_device_supported: 1.into(),
                                    reserved: [0; 12],
                                },
                            )
                            .await?;
                            *substate = ActiveState::ReadRequest;
                        }
                        ActiveState::SendSupportedResolutions { maximum_count } => {
                            if maximum_count < protocol::MAXIMUM_RESOLUTIONS_COUNT {
                                Self::send_packet(
                                    &mut channel,
                                    protocol::MESSAGE_SUPPORTED_RESOLUTIONS_RESPONSE,
                                    &protocol::SupportedResolutionsResponseMessage {
                                        edid_block: protocol::EDID_BLOCK,
                                        resolution_count: 0,
                                        default_resolution_index: 0,
                                        is_standard: 0,
                                    },
                                )
                                .await?;
                            } else {
                                const RESOLUTIONS: &[(u16, u16)] = &[(1024, 768), (1280, 1024)];

                                let mut packet = Vec::new();
                                packet.extend_from_slice(
                                    protocol::SupportedResolutionsResponseMessage {
                                        edid_block: protocol::EDID_BLOCK,
                                        resolution_count: RESOLUTIONS.len().try_into().unwrap(),
                                        default_resolution_index: 0,
                                        is_standard: 0,
                                    }
                                    .as_bytes(),
                                );
                                for r in RESOLUTIONS {
                                    packet.extend_from_slice(
                                        protocol::ScreenInfo {
                                            width: r.0.into(),
                                            height: r.1.into(),
                                        }
                                        .as_bytes(),
                                    );
                                }
                                Self::send_packet(
                                    &mut channel,
                                    protocol::MESSAGE_SUPPORTED_RESOLUTIONS_RESPONSE,
                                    packet.as_slice(),
                                )
                                .await?;
                            }
                            *substate = ActiveState::ReadRequest;
                        }
                        ActiveState::SendCapability => {
                            Self::send_packet(
                                &mut channel,
                                protocol::MESSAGE_CAPABILITY_RESPONSE,
                                &protocol::CapabilityResponseMessage {
                                    lock_on_disconnect: 0.into(),
                                    reserved: [0.into(); 15],
                                },
                            )
                            .await?;
                            *substate = ActiveState::ReadRequest;
                        }
                    }
                }
            }
        }
    }
}
