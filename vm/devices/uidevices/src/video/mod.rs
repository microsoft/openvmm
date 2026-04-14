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

use video_core::DirtyRect;

/// Vmbus synthetic video device.
pub struct Video {
    control: Box<dyn FramebufferControl>,
    /// Channel to forward dirty rectangles from the guest to the VNC worker.
    dirt_send: Option<mesh::Sender<Vec<DirtyRect>>>,
}

impl Video {
    /// Creates a new video device.
    pub fn new(
        control: Box<dyn FramebufferControl>,
        dirt_send: Option<mesh::Sender<Vec<DirtyRect>>>,
    ) -> anyhow::Result<Self> {
        Ok(Self { control, dirt_send })
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
            match channel.process(&mut self.control, &self.dirt_send).await {
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
        dirt_send: &Option<mesh::Sender<Vec<DirtyRect>>>,
    ) -> Result<(), Error> {
        process_channel(
            &mut self.channel,
            &mut self.state,
            &mut self.packet_buf,
            framebuffer,
            dirt_send,
        )
        .await
    }
}

async fn process_channel(
    channel: &mut (impl AsyncRecv + AsyncSend + Unpin),
    state: &mut ChannelState,
    packet_buf: &mut PacketBuffer,
    framebuffer: &mut Box<dyn FramebufferControl>,
    dirt_send: &Option<mesh::Sender<Vec<DirtyRect>>>,
) -> Result<(), Error> {
    loop {
        match state {
            ChannelState::ReadVersion => {
                let version =
                    if let Request::Version(version) = packet_buf.recv_packet(channel).await? {
                        version.into()
                    } else {
                        return Err(Error::UnexpectedPacketOrder);
                    };
                *state = ChannelState::WriteVersion { version };
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
                VideoChannel::send_packet(
                    channel,
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
                    *state = ChannelState::Active {
                        version: *version,
                        substate: ActiveState::ReadRequest,
                    };
                } else {
                    tracelimit::warn_ratelimited!(?version, "video negotiation failed");
                    *state = ChannelState::ReadVersion;
                }
            }
            ChannelState::Active {
                version: _,
                substate,
            } => match *substate {
                ActiveState::ReadRequest => {
                    let packet = packet_buf.recv_packet(channel).await?;
                    match packet {
                        Request::VramLocation {
                            user_context,
                            address,
                        } => {
                            framebuffer.unmap().await;
                            if let Some(address) = address {
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
                                    bytes_per_line: u32::from(situation.pitch_bytes) as usize,
                                    offset: u32::from(situation.primary_surface_vram_offset)
                                        as usize,
                                })
                                .await;
                            *substate = ActiveState::SendSituationUpdateAck { user_context };
                        }
                        Request::PointerPosition { is_visible, x, y } => {
                            let _ = (is_visible, x, y);
                        }
                        Request::PointerShape => {}
                        Request::Dirt(rects) => {
                            if let Some(send) = dirt_send {
                                let dirty: Vec<DirtyRect> = rects
                                    .iter()
                                    .map(|r| DirtyRect {
                                        left: r.left.into(),
                                        top: r.top.into(),
                                        right: r.right.into(),
                                        bottom: r.bottom.into(),
                                    })
                                    .collect();
                                send.send(dirty);
                            }
                        }
                        Request::BiosInfo => {
                            *substate = ActiveState::SendBiosInfo;
                        }
                        Request::SupportedResolutions { maximum_count } => {
                            *substate = ActiveState::SendSupportedResolutions { maximum_count };
                        }
                        Request::Capability => {
                            *substate = ActiveState::SendCapability;
                        }
                        Request::Version(_) => return Err(Error::UnexpectedPacketOrder),
                    }
                }
                ActiveState::SendVramAck { user_context } => {
                    VideoChannel::send_packet(
                        channel,
                        protocol::MESSAGE_VRAM_LOCATION_ACK,
                        &protocol::VramLocationAckMessage {
                            user_context: user_context.into(),
                        },
                    )
                    .await?;
                    *substate = ActiveState::ReadRequest;
                }
                ActiveState::SendSituationUpdateAck { user_context } => {
                    VideoChannel::send_packet(
                        channel,
                        protocol::MESSAGE_SITUATION_UPDATE_ACK,
                        &protocol::SituationUpdateAckMessage {
                            user_context: user_context.into(),
                        },
                    )
                    .await?;
                    *substate = ActiveState::ReadRequest;
                }
                ActiveState::SendBiosInfo => {
                    VideoChannel::send_packet(
                        channel,
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
                        VideoChannel::send_packet(
                            channel,
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
                        VideoChannel::send_packet(
                            channel,
                            protocol::MESSAGE_SUPPORTED_RESOLUTIONS_RESPONSE,
                            packet.as_slice(),
                        )
                        .await?;
                    }
                    *substate = ActiveState::ReadRequest;
                }
                ActiveState::SendCapability => {
                    VideoChannel::send_packet(
                        channel,
                        protocol::MESSAGE_CAPABILITY_RESPONSE,
                        &protocol::CapabilityResponseMessage {
                            lock_on_disconnect: 0.into(),
                            reserved: [0.into(); 15],
                        },
                    )
                    .await?;
                    *substate = ActiveState::ReadRequest;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use framebuffer::FRAMEBUFFER_SIZE;
    use guestmem::MappableGuestMemory;
    use guestmem::MappedMemoryRegion;
    use guestmem::MemoryMapper;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::task::Task;
    use sparse_mmap::AsMappableRef;
    use sparse_mmap::SparseMapping;
    use sparse_mmap::alloc_shared_memory;
    use std::io::ErrorKind;
    use std::sync::Arc;
    use vmbus_async::pipe::connected_message_pipes;
    use vmbus_ring::RingMem;

    struct TestGuestMemory;

    impl MappableGuestMemory for TestGuestMemory {
        fn map_to_guest(&mut self, _gpa: u64, _writable: bool) -> std::io::Result<()> {
            Ok(())
        }

        fn unmap_from_guest(&mut self) {}
    }

    struct TestMappedRegion(SparseMapping);

    impl MappedMemoryRegion for TestMappedRegion {
        fn map(
            &self,
            offset: usize,
            section: &dyn AsMappableRef,
            file_offset: u64,
            len: usize,
            writable: bool,
        ) -> std::io::Result<()> {
            self.0.map_file(offset, len, section, file_offset, writable)
        }

        fn unmap(&self, offset: usize, len: usize) -> std::io::Result<()> {
            self.0.unmap(offset, len)
        }
    }

    struct TestMemoryMapper;

    impl MemoryMapper for TestMemoryMapper {
        fn new_region(
            &self,
            len: usize,
            _debug_name: String,
        ) -> std::io::Result<(Box<dyn MappableGuestMemory>, Arc<dyn MappedMemoryRegion>)> {
            Ok((
                Box::new(TestGuestMemory),
                Arc::new(TestMappedRegion(SparseMapping::new(len)?)),
            ))
        }
    }

    fn framebuffer_fixture() -> (
        framebuffer::FramebufferDevice,
        Box<dyn FramebufferControl>,
        framebuffer::View,
    ) {
        let vram = alloc_shared_memory(FRAMEBUFFER_SIZE, "video-test").unwrap();
        let (fb, access) = framebuffer::framebuffer(vram, FRAMEBUFFER_SIZE, 0).unwrap();
        let device =
            framebuffer::FramebufferDevice::new(Box::new(TestMemoryMapper), fb, None).unwrap();
        let control: Box<dyn FramebufferControl> = Box::new(device.control());
        let view = access.view().unwrap();
        (device, control, view)
    }

    async fn send_packet<T: IntoBytes + Immutable + KnownLayout>(
        writer: &mut (impl AsyncSend + Unpin),
        typ: u32,
        packet: &T,
    ) {
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
            .unwrap();
    }

    async fn send_dirt_packet(
        writer: &mut (impl AsyncSend + Unpin),
        rects: &[protocol::Rectangle],
    ) {
        let header = protocol::MessageHeader {
            typ: protocol::MESSAGE_DIRT.into(),
            size: ((size_of::<protocol::DirtMessage>() + size_of_val(rects)) as u32).into(),
        };
        let dirt = protocol::DirtMessage {
            video_output: 0,
            dirt_count: rects.len().try_into().unwrap(),
        };
        writer
            .send_vectored(&[
                IoSlice::new(header.as_bytes()),
                IoSlice::new(dirt.as_bytes()),
                IoSlice::new(rects.as_bytes()),
            ])
            .await
            .unwrap();
    }

    async fn recv_bytes(reader: &mut (impl AsyncRecv + Unpin + Send)) -> Vec<u8> {
        let mut packet = vec![0; protocol::MAX_VSP_TO_VSC_MESSAGE_SIZE.max(512)];
        let n = reader.recv(&mut packet).await.unwrap();
        packet.truncate(n);
        packet
    }

    fn parse_header(packet: &[u8]) -> (protocol::MessageHeader, &[u8]) {
        let (header, rest) = Ref::<_, protocol::MessageHeader>::from_prefix(packet).unwrap();
        (*header, rest)
    }

    fn start_worker<T: RingMem + 'static + Unpin + Send + Sync>(
        driver: &DefaultDriver,
        mut control: Box<dyn FramebufferControl>,
        dirt_send: Option<mesh::Sender<Vec<DirtyRect>>>,
        mut channel: MessagePipe<T>,
    ) -> Task<Result<(), Error>> {
        driver.spawn("video worker", async move {
            let mut state = ChannelState::ReadVersion;
            let mut packet_buf = PacketBuffer::new();
            process_channel(
                &mut channel,
                &mut state,
                &mut packet_buf,
                &mut control,
                &dirt_send,
            )
            .await
            .or_else(|e| match e {
                Error::Io(err) if err.kind() == ErrorKind::ConnectionReset => Ok(()),
                _ => Err(e),
            })
        })
    }

    #[async_test]
    async fn test_channel_updates_framebuffer_and_forwards_dirt(driver: DefaultDriver) {
        let (host, mut guest) = connected_message_pipes(16384);
        let (_device, control, mut view) = framebuffer_fixture();
        let (dirt_send, mut dirt_recv) = mesh::channel();
        let worker = start_worker(&driver, control, Some(dirt_send), host);

        let version = protocol::Version::new(protocol::VERSION_MAJOR, protocol::VERSION_MINOR_BLUE);
        send_packet(
            &mut guest,
            protocol::MESSAGE_VERSION_REQUEST,
            &protocol::VersionRequestMessage { version },
        )
        .await;

        let packet = recv_bytes(&mut guest).await;
        let (header, rest) = parse_header(&packet);
        assert_eq!(header.typ.to_ne(), protocol::MESSAGE_VERSION_RESPONSE);
        let response = protocol::VersionResponseMessage::ref_from_prefix(rest)
            .unwrap()
            .0;
        assert_eq!(response.version.major(), protocol::VERSION_MAJOR);
        assert_eq!(response.version.minor(), protocol::VERSION_MINOR_BLUE);
        assert_eq!(
            response.is_accepted,
            protocol::ACCEPTED_WITH_VERSION_EXCHANGE
        );
        assert_eq!(response.max_video_outputs, 1);

        send_packet(
            &mut guest,
            protocol::MESSAGE_VRAM_LOCATION,
            &protocol::VramLocationMessage {
                user_context: 0x1234u64.into(),
                is_vram_gpa_address_specified: 1,
                vram_gpa_address: 0x4000u64.into(),
            },
        )
        .await;

        let packet = recv_bytes(&mut guest).await;
        let (header, rest) = parse_header(&packet);
        assert_eq!(header.typ.to_ne(), protocol::MESSAGE_VRAM_LOCATION_ACK);
        let ack = protocol::VramLocationAckMessage::ref_from_prefix(rest)
            .unwrap()
            .0;
        assert_eq!(ack.user_context.to_ne(), 0x1234);

        send_packet(
            &mut guest,
            protocol::MESSAGE_SITUATION_UPDATE,
            &protocol::SituationUpdateMessage {
                user_context: 0x5678u64.into(),
                video_output_count: 1,
                video_output: protocol::VideoOutputSituation {
                    active: 1,
                    primary_surface_vram_offset: 0.into(),
                    depth_bits: 32,
                    width_pixels: 800u32.into(),
                    height_pixels: 600u32.into(),
                    pitch_bytes: (800u32 * 4).into(),
                },
            },
        )
        .await;

        let packet = recv_bytes(&mut guest).await;
        let (header, rest) = parse_header(&packet);
        assert_eq!(header.typ.to_ne(), protocol::MESSAGE_SITUATION_UPDATE_ACK);
        let ack = protocol::SituationUpdateAckMessage::ref_from_prefix(rest)
            .unwrap()
            .0;
        assert_eq!(ack.user_context.to_ne(), 0x5678);
        assert_eq!(view.resolution(), (800, 600));

        let rects = [
            protocol::Rectangle {
                left: 1.into(),
                top: 2.into(),
                right: 30.into(),
                bottom: 40.into(),
            },
            protocol::Rectangle {
                left: 100.into(),
                top: 120.into(),
                right: 140.into(),
                bottom: 180.into(),
            },
        ];
        send_dirt_packet(&mut guest, &rects).await;

        let dirt = dirt_recv.recv().await.unwrap();
        assert_eq!(dirt.len(), 2);
        assert_eq!(dirt[0].left, 1);
        assert_eq!(dirt[0].top, 2);
        assert_eq!(dirt[0].right, 30);
        assert_eq!(dirt[0].bottom, 40);
        assert_eq!(dirt[1].left, 100);
        assert_eq!(dirt[1].top, 120);
        assert_eq!(dirt[1].right, 140);
        assert_eq!(dirt[1].bottom, 180);

        drop(guest);
        worker.await.unwrap();
    }

    #[async_test]
    async fn test_channel_reports_bios_resolutions_and_capability(driver: DefaultDriver) {
        let (host, mut guest) = connected_message_pipes(16384);
        let (_device, control, _view) = framebuffer_fixture();
        let worker = start_worker(&driver, control, None, host);

        send_packet(
            &mut guest,
            protocol::MESSAGE_VERSION_REQUEST,
            &protocol::VersionRequestMessage {
                version: protocol::Version::new(
                    protocol::VERSION_MAJOR,
                    protocol::VERSION_MINOR_BLUE,
                ),
            },
        )
        .await;
        let _ = recv_bytes(&mut guest).await;

        send_packet(
            &mut guest,
            protocol::MESSAGE_BIOS_INFO_REQUEST,
            &protocol::BiosInfoRequestMessage {},
        )
        .await;
        let packet = recv_bytes(&mut guest).await;
        let (header, rest) = parse_header(&packet);
        assert_eq!(header.typ.to_ne(), protocol::MESSAGE_BIOS_INFO_RESPONSE);
        let bios = protocol::BiosInfoResponseMessage::ref_from_prefix(rest)
            .unwrap()
            .0;
        assert_eq!(bios.stop_device_supported.to_ne(), 1);

        send_packet(
            &mut guest,
            protocol::MESSAGE_SUPPORTED_RESOLUTIONS_REQUEST,
            &protocol::SupportedResolutionsRequestMessage {
                maximum_resolution_count: protocol::MAXIMUM_RESOLUTIONS_COUNT,
            },
        )
        .await;
        let packet = recv_bytes(&mut guest).await;
        let (header, rest) = parse_header(&packet);
        assert_eq!(
            header.typ.to_ne(),
            protocol::MESSAGE_SUPPORTED_RESOLUTIONS_RESPONSE
        );
        let (response, rest) =
            Ref::<_, protocol::SupportedResolutionsResponseMessage>::from_prefix(rest).unwrap();
        assert_eq!(response.resolution_count as usize, 2);
        let (screens, tail) = <[protocol::ScreenInfo]>::ref_from_prefix_with_elems(
            rest,
            response.resolution_count as usize,
        )
        .unwrap();
        assert!(tail.is_empty());
        assert_eq!(screens[0].width.to_ne(), 1024);
        assert_eq!(screens[0].height.to_ne(), 768);
        assert_eq!(screens[1].width.to_ne(), 1280);
        assert_eq!(screens[1].height.to_ne(), 1024);

        send_packet(
            &mut guest,
            protocol::MESSAGE_CAPABILITY_REQUEST,
            &protocol::CapabilityRequestMessage {},
        )
        .await;
        let packet = recv_bytes(&mut guest).await;
        let (header, rest) = parse_header(&packet);
        assert_eq!(header.typ.to_ne(), protocol::MESSAGE_CAPABILITY_RESPONSE);
        let capability = protocol::CapabilityResponseMessage::ref_from_prefix(rest)
            .unwrap()
            .0;
        assert_eq!(capability.lock_on_disconnect.to_ne(), 0);

        drop(guest);
        worker.await.unwrap();
    }

    #[async_test]
    async fn test_channel_rejects_out_of_order_request(driver: DefaultDriver) {
        let (host, mut guest) = connected_message_pipes(16384);
        let (_device, control, _view) = framebuffer_fixture();
        let worker = start_worker(&driver, control, None, host);

        send_packet(
            &mut guest,
            protocol::MESSAGE_BIOS_INFO_REQUEST,
            &protocol::BiosInfoRequestMessage {},
        )
        .await;

        let err = worker.await.unwrap_err();
        assert!(matches!(err, Error::UnexpectedPacketOrder));
    }
}
