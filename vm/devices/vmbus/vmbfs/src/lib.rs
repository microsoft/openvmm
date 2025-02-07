// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate provides the Vmbfs (vmbus file system) device, used to provide a
//! simple, read-only virtual file system to the Windows boot loader and the
//! Hyper-V UEFI firmware.
//!
//! This device is primarily used to provide an initial machine configuration
//! (IMC) hive to Windows. This hive can contain registry entries to apply to
//! the system hive during boot, making it easier to customize an image without
//! directly modifying its disk.
//!
//! It is also used for Windows container boot, but this is not currently
//! supported.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod backing;
mod protocol;
pub mod resolver;
pub mod single_file_backing;

use async_trait::async_trait;
use inspect::Inspect;
use inspect::InspectMut;
use std::io::IoSlice;
use task_control::Cancelled;
use task_control::StopTask;
use thiserror::Error;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmcore::save_restore::SavedStateNotSupported;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// A vmbfs device.
#[derive(InspectMut)]
pub struct VmbfsDevice {
    #[inspect(mut)]
    backing: Box<dyn backing::VmbfsIo>,
}

impl VmbfsDevice {
    /// Creates a new vmbfs device, with the files provided by `backing`.
    pub fn new(backing: Box<dyn backing::VmbfsIo>) -> Self {
        Self { backing }
    }
}

#[async_trait]
impl SimpleVmbusDevice for VmbfsDevice {
    type SavedState = SavedStateNotSupported;
    type Runner = VmbfsChannel;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "vmbfs".to_owned(),
            channel_type: vmbus_channel::bus::ChannelType::Device { pipe_packets: true },
            // For now, always offer the IMC instance. To support
            // boot-over-vmbfs, change this to BOOT_INSTANCE.
            instance_id: protocol::IMC_INSTANCE,
            interface_id: protocol::INTERFACE_TYPE,
            ..OfferParams::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, runner: Option<&mut Self::Runner>) {
        req.respond().merge(self).merge(runner);
    }

    fn open(
        &mut self,
        channel: vmbus_channel::RawAsyncChannel<GpadlRingMem>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, vmbus_channel::channel::ChannelOpenError> {
        Ok(VmbfsChannel {
            state: State::VersionRequest,
            pipe: vmbus_async::pipe::MessagePipe::new(channel)?,
            buf: vec![0; protocol::MAX_MESSAGE_SIZE],
        })
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut Self::Runner,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(runner.process(self)).await
    }

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn vmbus_channel::simple::SaveRestoreSimpleVmbusDevice<
            SavedState = Self::SavedState,
            Runner = Self::Runner,
        >,
    > {
        None
    }
}

#[doc(hidden)] // used as an associated type in a trait but not part of the public API
#[derive(InspectMut)]
pub struct VmbfsChannel {
    state: State,
    #[inspect(mut)]
    pipe: vmbus_async::pipe::MessagePipe<GpadlRingMem>,
    buf: Vec<u8>,
}

#[derive(Inspect)]
enum State {
    VersionRequest,
    Ready,
}

#[derive(Debug)]
enum Request {
    Version(protocol::Version),
    GetFileInfo(String),
    ReadFile {
        byte_count: u32,
        offset: u64,
        path: String,
    },
}

impl VmbfsChannel {
    async fn process(&mut self, dev: &mut VmbfsDevice) {
        match self.process_inner(dev).await {
            Ok(()) => {}
            Err(err) => {
                tracing::error!(error = &err as &dyn std::error::Error, "vmbfs failed");
            }
        }
    }

    async fn process_inner(&mut self, dev: &mut VmbfsDevice) -> Result<(), DeviceError> {
        loop {
            self.pipe
                .wait_write_ready(protocol::MAX_MESSAGE_SIZE)
                .await
                .map_err(DeviceError::Pipe)?;

            match self.state {
                State::VersionRequest => match self.read_message().await? {
                    Request::Version(version) => {
                        let ok = match version {
                            protocol::Version::WIN10 => true,
                            version => {
                                tracing::debug!(?version, "unsupported version");
                                false
                            }
                        };
                        self.pipe
                            .try_send_vectored(&[
                                IoSlice::new(
                                    protocol::MessageHeader {
                                        message_type: protocol::MessageType::VERSION_RESPONSE,
                                        reserved: 0,
                                    }
                                    .as_bytes(),
                                ),
                                IoSlice::new(
                                    protocol::VersionResponse {
                                        status: if ok {
                                            protocol::VersionStatus::SUPPORTED
                                        } else {
                                            protocol::VersionStatus::UNSUPPORTED
                                        },
                                    }
                                    .as_bytes(),
                                ),
                            ])
                            .map_err(DeviceError::Pipe)?;

                        if ok {
                            self.state = State::Ready;
                        }
                    }
                    _ => return Err(DeviceError::UnexpectedMessage),
                },
                State::Ready => match self.read_message().await? {
                    Request::GetFileInfo(path) => {
                        self.handle_get_file_info(dev, &path)?;
                    }
                    Request::ReadFile {
                        byte_count,
                        offset,
                        path,
                    } => self.handle_read_file(dev, &path, offset, byte_count)?,
                    _ => return Err(DeviceError::UnexpectedMessage),
                },
            }
        }
    }

    fn handle_get_file_info(
        &mut self,
        dev: &mut VmbfsDevice,
        path: &str,
    ) -> Result<(), DeviceError> {
        let response = match dev.backing.file_info(path) {
            Ok(info) => protocol::GetFileInfoResponse {
                status: protocol::Status::SUCCESS,
                // Consider supporting RDMA read in the future for better
                // performance.
                flags: protocol::FileInfoFlags::new().with_directory(info.directory),
                file_size: info.file_size,
            },
            Err(err) => protocol::GetFileInfoResponse {
                status: err.to_protocol(),
                flags: protocol::FileInfoFlags::new(),
                file_size: 0,
            },
        };
        self.pipe
            .try_send_vectored(&[
                IoSlice::new(
                    protocol::MessageHeader {
                        message_type: protocol::MessageType::GET_FILE_INFO_RESPONSE,
                        reserved: 0,
                    }
                    .as_bytes(),
                ),
                IoSlice::new(response.as_bytes()),
            ])
            .map_err(DeviceError::Pipe)?;
        Ok(())
    }

    fn handle_read_file(
        &mut self,
        dev: &mut VmbfsDevice,
        path: &str,
        offset: u64,
        byte_count: u32,
    ) -> Result<(), DeviceError> {
        if byte_count > protocol::MAX_READ_SIZE as u32 {
            return Err(DeviceError::ReadTooLarge);
        }
        let buf = &mut self.buf[..byte_count as usize];
        let (status, buf) = match dev.backing.read_file(path, offset, buf) {
            Ok(()) => (protocol::Status::SUCCESS, &*buf),
            Err(err) => (err.to_protocol(), &[] as _),
        };
        self.pipe
            .try_send_vectored(&[
                IoSlice::new(
                    protocol::MessageHeader {
                        message_type: protocol::MessageType::READ_FILE_RESPONSE,
                        reserved: 0,
                    }
                    .as_bytes(),
                ),
                IoSlice::new(protocol::ReadFileResponse { status }.as_bytes()),
                IoSlice::new(buf),
            ])
            .map_err(DeviceError::Pipe)?;
        Ok(())
    }

    async fn read_message(&mut self) -> Result<Request, DeviceError> {
        let n = self
            .pipe
            .recv(&mut self.buf)
            .await
            .map_err(DeviceError::Pipe)?;

        let buf = &self.buf[..n];
        let (header, buf) =
            protocol::MessageHeader::read_from_prefix(buf).map_err(|_| DeviceError::TooShort)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let request = match header.message_type {
            protocol::MessageType::VERSION_REQUEST => {
                let version = protocol::VersionRequest::read_from_prefix(buf)
                    .map_err(|_| DeviceError::TooShort)?
                    .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                Request::Version(version.requested_version)
            }
            protocol::MessageType::GET_FILE_INFO_REQUEST => Request::GetFileInfo(parse_path(buf)?),
            protocol::MessageType::READ_FILE_REQUEST => {
                let (read, buf) = protocol::ReadFileRequest::read_from_prefix(buf)
                    .map_err(|_| DeviceError::TooShort)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                Request::ReadFile {
                    byte_count: read.byte_count,
                    offset: read.offset.get(),
                    path: parse_path(buf)?,
                }
            }
            ty => return Err(DeviceError::InvalidMessageType(ty)),
        };

        tracing::trace!(?request, "message");
        Ok(request)
    }
}

fn parse_path(buf: &[u8]) -> Result<String, DeviceError> {
    let buf = <[u16]>::ref_from_bytes(buf).map_err(|_| DeviceError::Unaligned)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
    if buf.contains(&0) {
        return Err(DeviceError::NullTerminatorInPath);
    }
    let path = String::from_utf16(buf).map_err(|_| DeviceError::InvalidUtf16Path)?;
    Ok(path.replace('\\', "/"))
}

#[derive(Debug, Error)]
enum DeviceError {
    #[error("vmbus pipe error")]
    Pipe(#[source] std::io::Error),
    #[error("message too short")]
    TooShort,
    #[error("unaligned message")]
    Unaligned,
    #[error("invalid utf-16 path")]
    InvalidUtf16Path,
    #[error("null terminator in path")]
    NullTerminatorInPath,
    #[error("unexpected message")]
    UnexpectedMessage,
    #[error("invalid message type: {0:#x?}")]
    InvalidMessageType(protocol::MessageType),
    #[error("read too large")]
    ReadTooLarge,
}
