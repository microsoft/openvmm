// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest Emulation Device - GED
//!
//! The GED is the host side of a communication channel that uses VMBUS to
//! communicate between Guest and Host. This is an implementation to support
//! better integration testing within the OpenVMM CI, and is not at
//! feature-parity with the implementation in Hyper-V.

#![forbid(unsafe_code)]

pub mod resolver;
#[doc(hidden)]
pub mod test_utilities;

use async_trait::async_trait;
use core::mem::size_of;
use futures::FutureExt;
use futures::StreamExt;
use get_protocol::dps_json::HclSecureBootTemplateId;
use get_protocol::dps_json::PcatBootDevice;
use get_protocol::BatteryStatusFlags;
use get_protocol::BatteryStatusNotification;
use get_protocol::HeaderGeneric;
use get_protocol::HostNotifications;
use get_protocol::HostRequests;
use get_protocol::RegisterState;
use get_protocol::SecureBootTemplateType;
use get_protocol::StartVtl0Status;
use get_protocol::UefiConsoleMode;
use get_protocol::VmgsIoStatus;
use get_protocol::MAX_PAYLOAD_SIZE;
use get_resources::ged::FirmwareEvent;
use get_resources::ged::GuestEmulationRequest;
use get_resources::ged::ModifyVtl2SettingsError;
use get_resources::ged::SaveRestoreError;
use get_resources::ged::Vtl0StartError;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use power_resources::PowerRequest;
use power_resources::PowerRequestClient;
use std::io::IoSlice;
use task_control::StopTask;
use thiserror::Error;
use video_core::FramebufferControl;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring::RingMem;
use vmcore::save_restore::SavedStateNotSupported;
use zerocopy::AsBytes;
use zerocopy::FromBytes;
use zerocopy::FromZeroes;
use zerocopy_helpers::FromBytesExt;

/// Host GET errors
#[derive(Debug, Error)]
enum Error {
    // Note that this is never logged, as it is converted back to
    // `task_control::Cancelled` at the bottom of the task's stack.
    #[error("cancelled")]
    Cancelled(task_control::Cancelled),
    #[error("vmbus channel i/o error")]
    Vmbus(#[source] std::io::Error),
    #[error("accepting vmbus channel")]
    Accept(#[from] vmbus_channel::offer::Error),
    #[error("message too small")]
    MessageTooSmall,
    #[error("serializing device platform settings v2")]
    SerializeDpsV2(#[source] serde_json::Error),
    #[error("invalid packet sequence")]
    InvalidSequence,
    #[error("failed to parse host request")]
    HostRequest,
    #[error("invalid header version: {0:?}")]
    HeaderVersion(get_protocol::MessageVersions),
    #[error("data was received with an invalid field value")]
    InvalidFieldValue,
    #[error("large device platform settings v2 is currently unimplemented")]
    LargeDpsV2Unimplemented,
}

impl From<task_control::Cancelled> for Error {
    fn from(value: task_control::Cancelled) -> Self {
        Error::Cancelled(value)
    }
}

// TODO: swap out the adhoc in-memory VMGS with a real VMGS backend.
const VMGS_CAPACITY: usize = 4194816; // 4 MB
const VMGS_SECTOR_SIZE: usize = 512;

/// Settings to enable in the guest.
#[derive(Debug, Clone, Inspect)]
pub struct GuestConfig {
    /// Firmware configuration.
    pub firmware: GuestFirmwareConfig,
    /// Enable COM1 for VTL0 and the VMBUS redirector in VTL2.
    pub com1: bool,
    /// Enable COM2 for VTL0 and the VMBUS redirector in VTL2.
    pub com2: bool,
    /// Enable vmbus redirection.
    pub vmbus_redirection: bool,
    /// Enable the TPM.
    pub enable_tpm: bool,
    /// The encoded VTL2 settings document.
    #[inspect(with = "Option::is_some")]
    pub vtl2_settings: Option<Vec<u8>>,
    /// Enable secure boot.
    pub secure_boot_enabled: bool,
    /// Secure boot template to use.
    #[inspect(debug)]
    pub secure_boot_template: SecureBootTemplateType,
    /// Enable battery.
    pub enable_battery: bool,
}

#[derive(Debug, Clone, Inspect)]
#[inspect(external_tag)]
pub enum GuestFirmwareConfig {
    Uefi {
        /// Tell UEFI to consider booting from VPCI.
        enable_vpci_boot: bool,
        /// Enable UEFI firmware debugging for VTL0.
        firmware_debug: bool,
        /// Disable the UEFI frontpage which will cause the VM to shutdown instead when unable to boot.
        disable_frontpage: bool,
        /// Where to send UEFI console output
        #[inspect(debug)]
        console_mode: UefiConsoleMode,
    },
    Pcat {
        #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsDebug)")]
        boot_order: [PcatBootDevice; 4],
    },
}

/// Events the guest can log to the host via the GET.
#[derive(Debug)]
pub enum GuestEvent {
    BootSuccess,
    BootSuccessSecureBootFailed,
    BootFailure,
    BootFailureSecureBootFailed,
    NoBootDevice,
    AttestationFailed,
    VmgsFileClear,
    VmgsInitFailed,
    VmgsInvalidFormat,
    VmgsCorruptFormat,
    KeyNotReleased,
    DekDecryptionFailed,
    WatchdogTimeoutReset,
    BootAttempt,
}

/// VMBUS device that implements the host side of the Guest Emulation Transport protocol.
#[derive(InspectMut)]
pub struct GuestEmulationDevice {
    config: GuestConfig,

    #[inspect(skip)]
    power_client: PowerRequestClient,
    #[inspect(skip)]
    firmware_event_send: Option<mesh::MpscSender<FirmwareEvent>>,
    #[inspect(skip)]
    framebuffer_control: Option<Box<dyn FramebufferControl>>,
    #[inspect(skip)]
    guest_request_recv: mesh::Receiver<GuestEmulationRequest>,
    #[inspect(skip)]
    waiting_for_vtl0_start: Vec<mesh::OneshotSender<Result<(), Vtl0StartError>>>,

    #[inspect(skip)]
    vmgs: Vec<u8>,
    #[inspect(with = "Option::is_some")]
    save_restore_buf: Option<Vec<u8>>,
    last_save_restore_buf_len: usize,
}

impl GuestEmulationDevice {
    /// Create a new Host side GET device.
    pub fn new(
        config: GuestConfig,
        power_client: PowerRequestClient,
        firmware_event_send: Option<mesh::MpscSender<FirmwareEvent>>,
        guest_request_recv: mesh::Receiver<GuestEmulationRequest>,
        framebuffer_control: Option<Box<dyn FramebufferControl>>,
    ) -> Self {
        Self {
            config,
            power_client,
            firmware_event_send,
            framebuffer_control,
            guest_request_recv,
            vmgs: vec![0; VMGS_CAPACITY],
            save_restore_buf: None,
            waiting_for_vtl0_start: Vec::new(),
            last_save_restore_buf_len: 0,
        }
    }

    fn send_event(&self, event: FirmwareEvent) {
        if let Some(sender) = &self.firmware_event_send {
            sender.send(event);
        }
    }

    fn vmgs(&mut self, sector_offset: u64, sector_count: u32) -> Option<&mut [u8]> {
        let start = usize::try_from(sector_offset)
            .ok()?
            .checked_mul(VMGS_SECTOR_SIZE)?;
        let len = usize::try_from(sector_count)
            .ok()?
            .checked_mul(VMGS_SECTOR_SIZE)?;
        let end = start.checked_add(len)?;
        self.vmgs.get_mut(start..end)
    }
}

#[async_trait]
impl SimpleVmbusDevice for GuestEmulationDevice {
    type Runner = GedChannel;
    type SavedState = SavedStateNotSupported;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "get".to_owned(),
            interface_id: get_protocol::GUEST_EMULATION_INTERFACE_TYPE,
            instance_id: get_protocol::GUEST_EMULATION_INTERFACE_INSTANCE,
            channel_type: ChannelType::Pipe { message_mode: true },
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, mut channel: Option<&mut GedChannel>) {
        req.respond().merge(self).field_mut("channel", &mut channel);
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(GedChannel::new(pipe))
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        task_state: &mut GedChannel,
    ) -> Result<(), task_control::Cancelled> {
        match task_state.process(stop, self).await {
            Ok(()) => Ok(()),
            Err(Error::Cancelled(err)) => Err(err),
            Err(err) => {
                tracing::error!(error = &err as &dyn std::error::Error, "ged error");
                Ok(())
            }
        }
    }

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn vmbus_channel::simple::SaveRestoreSimpleVmbusDevice<
            SavedState = Self::SavedState,
            Runner = Self::Runner,
        >,
    > {
        // TODO
        None
    }
}

/// The GED task.
#[derive(InspectMut)]
pub struct GedChannel<T: RingMem = GpadlRingMem> {
    #[inspect(mut)]
    channel: MessagePipe<T>,
    #[inspect(skip)]
    state: GedState,
    #[inspect(with = "Option::is_some")]
    save: Option<InProgressSave>,
    #[inspect(with = "Option::is_some")]
    vtl0_start_report: Option<Result<(), Vtl0StartError>>,
    #[inspect(with = "Option::is_some")]
    modify: Option<mesh::OneshotSender<Result<(), ModifyVtl2SettingsError>>>,
}

struct InProgressSave {
    response: mesh::OneshotSender<Result<(), SaveRestoreError>>,
    buffer: Vec<u8>,
}

enum GedState {
    Init,
    Ready,
    SendingRestore { written: usize },
}

impl<T: RingMem + Unpin> GedChannel<T> {
    fn new(channel: MessagePipe<T>) -> Self {
        Self {
            channel,
            save: None,
            state: GedState::Init,
            vtl0_start_report: None,
            modify: None,
        }
    }

    async fn process(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut GuestEmulationDevice,
    ) -> Result<(), Error> {
        tracing::trace!("Begin GetChannel process()");

        loop {
            // Wait for enough space for a response packet.
            stop.until_stopped(
                self.channel
                    .wait_write_ready(get_protocol::MAX_MESSAGE_SIZE),
            )
            .await?
            .map_err(Error::Vmbus)?;

            match &mut self.state {
                GedState::Init => {
                    // Negotiate the version
                    let mut version_request = get_protocol::VersionRequest::new_zeroed();
                    stop.until_stopped(self.channel.recv_exact(version_request.as_bytes_mut()))
                        .await?
                        .map_err(Error::Vmbus)?;

                    if version_request.message_header.message_id != HostRequests::VERSION {
                        return Err(Error::InvalidSequence);
                    }

                    let version_response = get_protocol::VersionResponse::new(true);

                    self.channel
                        .try_send(version_response.as_bytes())
                        .map_err(Error::Vmbus)?;

                    tracing::info!("version negotiated successfully!");
                    self.state = GedState::Ready;

                    // Send a hardcoded battery status update
                    //
                    // TODO: Need to subscribe to WNF to get real battery notifications from the host
                    // and query NT for the host battery status details.
                    //
                    // For now, we just hardcode an initial arbitrary battery status update
                    // to the guest for testing battery presence in our VMM tests.
                    let _ = self.send_hardcoded_battery_update();
                }
                GedState::Ready => {
                    let mut message_buf = [0; get_protocol::MAX_MESSAGE_SIZE];
                    futures::select! { // merge semantics
                        pipe_input = self.channel.recv(&mut message_buf).fuse() => {
                            let bytes_read = pipe_input.map_err(Error::Vmbus)?;
                            self.handle_pipe_input(&message_buf[..bytes_read], state).await?;
                        },
                        guest_request = state.guest_request_recv.select_next_some() => {
                            self.handle_guest_request_input(state, guest_request)?;
                        }
                        _ = stop.fuse() => {
                            return Err(Error::Cancelled(task_control::Cancelled));
                        }
                    }
                }
                GedState::SendingRestore { written } => {
                    let buffer = state
                        .save_restore_buf
                        .as_ref()
                        .ok_or(Error::InvalidSequence)?;

                    let saved_state_size = buffer.len();
                    if *written >= saved_state_size {
                        self.state = GedState::Ready;
                        state.last_save_restore_buf_len = saved_state_size;
                        state.save_restore_buf = None;
                        continue;
                    }

                    const HEADER_SIZE: usize =
                        size_of::<get_protocol::RestoreGuestVtl2StateResponse>();

                    let mut message = vec![0; HEADER_SIZE + MAX_PAYLOAD_SIZE];

                    let status_code = if *written + MAX_PAYLOAD_SIZE >= saved_state_size {
                        get_protocol::GuestVtl2SaveRestoreStatus::SUCCESS
                    } else {
                        get_protocol::GuestVtl2SaveRestoreStatus::MORE_DATA
                    };

                    let payload_len = (saved_state_size - *written).min(MAX_PAYLOAD_SIZE);

                    let host_response_header = get_protocol::RestoreGuestVtl2StateResponse::new(
                        payload_len.try_into().unwrap(),
                        status_code,
                    );

                    tracing::debug!(
                        ?status_code,
                        written,
                        saved_state_size,
                        payload_len,
                        "more data"
                    );

                    message.clear();
                    message.extend_from_slice(host_response_header.as_bytes());
                    message.extend_from_slice(&buffer[*written..][..payload_len]);
                    self.channel.try_send(&message).map_err(Error::Vmbus)?;

                    *written += payload_len;
                }
            }
        }
    }

    async fn handle_pipe_input(
        &mut self,
        message_buf: &[u8],
        state: &mut GuestEmulationDevice,
    ) -> Result<(), Error> {
        let header =
            get_protocol::HeaderRaw::read_from_prefix(message_buf).ok_or(Error::MessageTooSmall)?;

        if header.message_version != get_protocol::MessageVersions::HEADER_VERSION_1 {
            return Err(Error::HeaderVersion(header.message_version));
        }

        match header.message_type {
            get_protocol::MessageTypes::HOST_NOTIFICATION => self.handle_host_notification(
                header.try_into().expect("validated message type"),
                message_buf,
                state,
            )?,
            get_protocol::MessageTypes::HOST_REQUEST => {
                self.handle_host_request(
                    header.try_into().expect("validated message type"),
                    message_buf,
                    state,
                )
                .await?
            }
            _ => {
                return Err(Error::HostRequest);
            }
        }
        Ok(())
    }

    fn handle_guest_request_input(
        &mut self,
        state: &mut GuestEmulationDevice,
        guest_request: GuestEmulationRequest,
    ) -> Result<(), Error> {
        match guest_request {
            GuestEmulationRequest::WaitForConnect(rpc) => rpc.handle_sync(|()| ()),
            GuestEmulationRequest::WaitForVtl0Start(Rpc((), response)) => {
                if let Some(result) = self.vtl0_start_report.clone() {
                    response.send(result);
                } else {
                    state.waiting_for_vtl0_start.push(response);
                }
            }
            GuestEmulationRequest::ModifyVtl2Settings(Rpc(data, response)) => {
                if self.modify.is_some() {
                    response.send(Err(ModifyVtl2SettingsError::OperationInProgress));
                    return Ok(());
                }

                // TODO: support larger payloads.
                if data.len() > MAX_PAYLOAD_SIZE {
                    response.send(Err(ModifyVtl2SettingsError::LargeSettingsNotSupported));
                    return Ok(());
                }

                let header = get_protocol::ModifyVtl2SettingsRev1Notification {
                    message_header: HeaderGeneric::new(
                        get_protocol::GuestNotifications::MODIFY_VTL2_SETTINGS_REV1,
                    ),
                    size: data.len() as u32,
                    payload_state: get_protocol::LargePayloadState::END,
                };

                self.channel
                    .try_send_vectored(&[IoSlice::new(header.as_bytes()), IoSlice::new(&data)])
                    .map_err(Error::Vmbus)?;

                self.modify = Some(response);
            }
            GuestEmulationRequest::SaveGuestVtl2State(Rpc((), response)) => {
                let r = (|| {
                    if self.save.is_some() {
                        return Err(SaveRestoreError::OperationInProgress);
                    }

                    // After sending the notification, we expect to get a
                    // HostRequest to save state. All further handling is done
                    // in that path after we receive the request.
                    let save_notif_packet = get_protocol::SaveGuestVtl2StateNotification {
                        message_header: HeaderGeneric::new(
                            get_protocol::GuestNotifications::SAVE_GUEST_VTL2_STATE,
                        ),
                        correlation_id: Guid::ZERO,
                        capabilities_flags: 0,
                        timeout_hint_secs: 60,
                    };

                    self.channel
                        .try_send(save_notif_packet.as_bytes())
                        .map_err(|err| SaveRestoreError::Io(RemoteError::new(err)))?;

                    Ok(())
                })();
                match r {
                    Ok(()) => {
                        self.save = Some(InProgressSave {
                            response,
                            buffer: Vec::new(),
                        })
                    }
                    Err(err) => response.send(Err(err)),
                }
            }
        };
        Ok(())
    }

    async fn handle_host_request(
        &mut self,
        header: get_protocol::HeaderHostRequest,
        message_buf: &[u8],
        state: &mut GuestEmulationDevice,
    ) -> Result<(), Error> {
        match header.message_id {
            HostRequests::TIME => self.handle_time()?,
            HostRequests::BIOS_BOOT_FINALIZE => self.handle_bios_boot_finalize(message_buf)?,
            HostRequests::VMGS_GET_DEVICE_INFO => self.handle_vmgs_get_device_info()?,
            HostRequests::VMGS_READ => self.handle_vmgs_read(state, message_buf)?,
            HostRequests::VMGS_WRITE => self.handle_vmgs_write(state, message_buf)?,
            HostRequests::VMGS_FLUSH => self.handle_vmgs_flush()?,
            HostRequests::GUEST_STATE_PROTECTION => {
                self.handle_guest_state_protection(message_buf)?
            }
            HostRequests::GUEST_STATE_PROTECTION_BY_ID => {
                self.handle_guest_state_protection_by_id()?;
            }
            HostRequests::DEVICE_PLATFORM_SETTINGS_V2 => {
                self.handle_device_platform_settings_v2(state)?
            }
            HostRequests::SAVE_GUEST_VTL2_STATE => {
                self.handle_save_guest_vtl2_state(message_buf, state)?
            }
            HostRequests::RESTORE_GUEST_VTL2_STATE => self.handle_restore_guest_vtl2_state(),
            HostRequests::MAP_FRAMEBUFFER => {
                self.handle_map_framebuffer(state, message_buf).await?
            }
            HostRequests::UNMAP_FRAMEBUFFER => self.handle_unmap_framebuffer(state).await?,
            HostRequests::CREATE_RAM_GPA_RANGE => self.handle_create_ram_gpa_range(message_buf)?,
            HostRequests::RESET_RAM_GPA_RANGE => self.handle_reset_ram_gpa_range(message_buf)?,
            _ => {
                tracing::error!(message_id = ?header.message_id, "unexpected message");
                return Err(Error::InvalidSequence);
            }
        };
        Ok(())
    }

    fn handle_bios_boot_finalize(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let msg = get_protocol::BiosBootFinalizeRequest::read_from_prefix(message_buf)
            .ok_or(Error::MessageTooSmall)?;

        tracing::trace!(?msg, "Bios Boot Finalize request");

        let response = get_protocol::BiosBootFinalizeResponse::new();
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_time(&mut self) -> Result<(), Error> {
        const WINDOWS_EPOCH: time::OffsetDateTime = time::macros::datetime!(1601-01-01 0:00 UTC);

        // utc in TimeResponse is in units of 100ns since the windows epoch
        let now_utc = time::OffsetDateTime::now_utc();
        let since_win_epoch = now_utc - WINDOWS_EPOCH;
        let since_win_epoch: i64 = (since_win_epoch.whole_nanoseconds() / 100)
            .try_into()
            .unwrap();

        // time_zone is in minutes between UTC and local time (as stored
        // in a windows TIME_ZONE_INFORMATION struct)
        let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let time_zone = local_offset.whole_minutes();
        let response = get_protocol::TimeResponse::new(0, since_win_epoch, time_zone, false);

        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_vmgs_get_device_info(&mut self) -> Result<(), Error> {
        let response = get_protocol::VmgsGetDeviceInfoResponse::new(
            VmgsIoStatus::SUCCESS,
            VMGS_CAPACITY as u64,
            VMGS_SECTOR_SIZE as u16,
            VMGS_SECTOR_SIZE as u16,
            512,
        );
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_vmgs_read(
        &mut self,
        state: &mut GuestEmulationDevice,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let message = get_protocol::VmgsReadRequest::read_from_prefix(message_buf)
            .ok_or(Error::MessageTooSmall)?;

        let (status, payload) = if let Some(vmgs) = state.vmgs(message.offset, message.length) {
            if vmgs.len() > MAX_PAYLOAD_SIZE {
                return Err(Error::InvalidFieldValue);
            }
            (VmgsIoStatus::SUCCESS, &*vmgs)
        } else {
            (VmgsIoStatus::DEVICE_ERROR, &[] as _)
        };

        let response = get_protocol::VmgsReadResponse::new(status);
        self.channel
            .try_send_vectored(&[IoSlice::new(response.as_bytes()), IoSlice::new(payload)])
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_vmgs_write(
        &mut self,
        state: &mut GuestEmulationDevice,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let (message, rest) = get_protocol::VmgsWriteRequest::read_from_prefix_split(message_buf)
            .ok_or(Error::MessageTooSmall)?;

        let status = if let Some(vmgs) = state.vmgs(message.offset, message.length) {
            if vmgs.len() > MAX_PAYLOAD_SIZE {
                return Err(Error::InvalidFieldValue);
            }
            vmgs.copy_from_slice(rest.get(..vmgs.len()).ok_or(Error::MessageTooSmall)?);
            VmgsIoStatus::SUCCESS
        } else {
            VmgsIoStatus::DEVICE_ERROR
        };

        let response = get_protocol::VmgsWriteResponse::new(status);
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_vmgs_flush(&mut self) -> Result<(), Error> {
        let response = get_protocol::VmgsFlushResponse::new(VmgsIoStatus::SUCCESS);
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_guest_state_protection(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let _message = get_protocol::GuestStateProtectionRequest::read_from_prefix(
            &message_buf.as_bytes()[..size_of::<get_protocol::GuestStateProtectionRequest>()],
        )
        .ok_or(Error::MessageTooSmall)?;

        let mut response = get_protocol::GuestStateProtectionResponse::new_zeroed();
        response.message_header = HeaderGeneric::new(HostRequests::GUEST_STATE_PROTECTION);

        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_guest_state_protection_by_id(&mut self) -> Result<(), Error> {
        let response = get_protocol::GuestStateProtectionByIdResponse {
            message_header: HeaderGeneric::new(HostRequests::GUEST_STATE_PROTECTION_BY_ID),
            ..get_protocol::GuestStateProtectionByIdResponse::new_zeroed()
        };
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_save_guest_vtl2_state(
        &mut self,
        message_buf: &[u8],
        state: &mut GuestEmulationDevice,
    ) -> Result<(), Error> {
        // Add chunked message to the accumulated state and check header
        // to see if we should expect more; if so then wait for more,
        // if not then signal the completion and return.
        // TODO: more state consistency checks.

        let save = self.save.as_mut().ok_or(Error::InvalidSequence)?;
        let (request_header, remaining) =
            get_protocol::SaveGuestVtl2StateRequest::read_from_prefix_split(message_buf)
                .ok_or(Error::MessageTooSmall)?;
        let r = match request_header.save_status {
            get_protocol::GuestVtl2SaveRestoreStatus::MORE_DATA => {
                save.buffer.extend_from_slice(remaining);
                None
            }
            get_protocol::GuestVtl2SaveRestoreStatus::SUCCESS => {
                save.buffer.extend_from_slice(remaining);

                tracing::debug!("Received all guest VTL2 save state");

                // Send response and then notify completion.
                let response = get_protocol::SaveGuestVtl2StateResponse::new(
                    get_protocol::GuestVtl2SaveRestoreStatus::SUCCESS,
                );
                self.channel
                    .try_send(response.as_bytes())
                    .map_err(Error::Vmbus)?;

                tracing::debug!("Notifying completion channel that save guest VTL2 op is complete");

                Some(Ok(()))
            }
            get_protocol::GuestVtl2SaveRestoreStatus::FAILURE => {
                Some(Err(SaveRestoreError::GuestError))
            }
            _ => {
                return Err(Error::InvalidFieldValue);
            }
        };
        if let Some(r) = r {
            let save = self.save.take().unwrap();
            if r.is_ok() {
                state.save_restore_buf = Some(save.buffer);
            }
            save.response.send(r);
        }
        Ok(())
    }

    fn handle_restore_guest_vtl2_state(&mut self) {
        self.state = GedState::SendingRestore { written: 0 };
    }

    async fn handle_map_framebuffer(
        &mut self,
        state: &mut GuestEmulationDevice,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let response = get_protocol::MapFramebufferResponse::new(
            if let Some(framebuffer_control) = state.framebuffer_control.as_mut() {
                let message = get_protocol::MapFramebufferRequest::read_from_prefix(message_buf)
                    .ok_or(Error::MessageTooSmall)?;
                let gpa = message.gpa;
                tracing::debug!("Received map framebuffer request from guest {:#x}", gpa);
                framebuffer_control.map(gpa).await;
                get_protocol::MapFramebufferStatus::SUCCESS
            } else {
                tracing::warn!("Guest requested framebuffer mapping but no framebuffer control was provided to the GET");
                get_protocol::MapFramebufferStatus::FAILURE
            },
        );
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    async fn handle_unmap_framebuffer(
        &mut self,
        state: &mut GuestEmulationDevice,
    ) -> Result<(), Error> {
        let response = get_protocol::UnmapFramebufferResponse::new(
            if let Some(framebuffer_control) = state.framebuffer_control.as_mut() {
                tracing::debug!("Received unmap framebuffer request from guest");
                framebuffer_control.unmap().await;
                get_protocol::UnmapFramebufferStatus::SUCCESS
            } else {
                tracing::warn!("Guest requested framebuffer mapping but no framebuffer control was provided to the GET");
                get_protocol::UnmapFramebufferStatus::FAILURE
            },
        );
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_create_ram_gpa_range(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let request = get_protocol::CreateRamGpaRangeRequest::read_from_prefix(message_buf)
            .ok_or(Error::MessageTooSmall)?;

        tracing::info!(?request, "create ram gpa range request");

        let response = get_protocol::CreateRamGpaRangeResponse::new(
            get_protocol::CreateRamGpaRangeStatus::FAILED,
        );
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_reset_ram_gpa_range(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let _request = get_protocol::ResetRamGpaRangeRequest::read_from_prefix(message_buf)
            .ok_or(Error::MessageTooSmall)?;
        let response = get_protocol::ResetRamGpaRangeResponse::new();
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_host_notification(
        &mut self,
        header: get_protocol::HeaderHostNotification,
        message_buf: &[u8],
        state: &mut GuestEmulationDevice,
    ) -> Result<(), Error> {
        match header.message_id {
            HostNotifications::POWER_OFF => {
                self.handle_power_off(state);
            }
            HostNotifications::RESET => {
                self.handle_reset(state);
            }
            HostNotifications::EVENT_LOG => {
                self.handle_event_log(state, message_buf)?;
            }
            HostNotifications::RESTORE_GUEST_VTL2_STATE_COMPLETED => {
                self.handle_restore_guest_vtl2_state_completed(message_buf)?;
            }
            HostNotifications::START_VTL0_COMPLETED => {
                self.handle_start_vtl0_completed(state, message_buf)?;
            }
            HostNotifications::VTL_CRASH => {
                self.handle_vtl_crash(message_buf)?;
            }
            HostNotifications::TRIPLE_FAULT => {
                self.handle_triple_fault(state, message_buf)?;
            }
            HostNotifications::MODIFY_VTL2_SETTINGS_COMPLETED => {
                self.handle_modify_vtl2_settings_completed(message_buf)?;
            }
            _ => {
                return Err(Error::InvalidFieldValue);
            }
        }
        Ok(())
    }

    fn handle_power_off(&mut self, state: &mut GuestEmulationDevice) {
        state.power_client.power_request(PowerRequest::PowerOff);
    }

    fn handle_reset(&mut self, state: &mut GuestEmulationDevice) {
        state.power_client.power_request(PowerRequest::Reset);
    }

    fn handle_event_log(
        &mut self,
        state: &mut GuestEmulationDevice,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let msg = get_protocol::EventLogNotification::read_from_prefix(message_buf)
            .ok_or(Error::MessageTooSmall)?;
        tracing::trace!("[Event Log] {:?}", msg);
        let event = match msg.event_log_id {
            get_protocol::EventLogId::BOOT_SUCCESS => GuestEvent::BootSuccess,
            get_protocol::EventLogId::BOOT_SUCCESS_SECURE_BOOT_FAILED => {
                GuestEvent::BootSuccessSecureBootFailed
            }
            get_protocol::EventLogId::BOOT_FAILURE => GuestEvent::BootFailure,
            get_protocol::EventLogId::BOOT_FAILURE_SECURE_BOOT_FAILED => {
                GuestEvent::BootFailureSecureBootFailed
            }
            get_protocol::EventLogId::NO_BOOT_DEVICE => GuestEvent::NoBootDevice,
            get_protocol::EventLogId::ATTESTATION_FAILED => GuestEvent::AttestationFailed,
            get_protocol::EventLogId::VMGS_FILE_CLEAR => GuestEvent::VmgsFileClear,
            get_protocol::EventLogId::VMGS_INIT_FAILED => GuestEvent::VmgsInitFailed,
            get_protocol::EventLogId::VMGS_INVALID_FORMAT => GuestEvent::VmgsInvalidFormat,
            get_protocol::EventLogId::VMGS_CORRUPT_FORMAT => GuestEvent::VmgsCorruptFormat,
            get_protocol::EventLogId::KEY_NOT_RELEASED => GuestEvent::KeyNotReleased,
            get_protocol::EventLogId::DEK_DECRYPTION_FAILED => GuestEvent::DekDecryptionFailed,
            get_protocol::EventLogId::BOOT_ATTEMPT => GuestEvent::BootAttempt,
            get_protocol::EventLogId::WATCHDOG_TIMEOUT_RESET => GuestEvent::WatchdogTimeoutReset,
            _ => {
                // TODO: logged but ignored for now.
                tracing::error!(event_log_id = msg.event_log_id.0, "unknown event log id");
                return Ok(());
            }
        };
        tracing::info!(?event, "GET event");
        match event {
            GuestEvent::BootAttempt => state.send_event(FirmwareEvent::BootAttempt),
            GuestEvent::BootSuccess | GuestEvent::BootSuccessSecureBootFailed => {
                state.send_event(FirmwareEvent::BootSuccess);
            }
            GuestEvent::BootFailure | GuestEvent::BootFailureSecureBootFailed => {
                state.send_event(FirmwareEvent::BootFailed);
            }
            GuestEvent::NoBootDevice => state.send_event(FirmwareEvent::NoBootDevice),
            // Other events have no analogous common firmware event yet, don't forward them.
            _ => {}
        }
        Ok(())
    }

    fn handle_restore_guest_vtl2_state_completed(
        &mut self,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let message =
            get_protocol::RestoreGuestVtl2StateHostNotification::read_from_prefix(message_buf)
                .ok_or(Error::MessageTooSmall)?;
        let success = match message.status {
            get_protocol::GuestVtl2SaveRestoreStatus::SUCCESS => true,
            get_protocol::GuestVtl2SaveRestoreStatus::FAILURE => false,
            _ => return Err(Error::InvalidFieldValue),
        };
        tracing::info!(success, "restore vtl2 complete");
        Ok(())
    }

    fn handle_start_vtl0_completed(
        &mut self,
        state: &mut GuestEmulationDevice,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let (message, remaining) =
            get_protocol::StartVtl0CompleteNotification::read_from_prefix_split(message_buf)
                .ok_or(Error::MessageTooSmall)?;
        let expected_len = message.result_document_size as usize;
        if remaining.len() != expected_len {
            return Err(Error::InvalidFieldValue);
        }
        let result = match message.status {
            StartVtl0Status::SUCCESS => {
                tracing::info!("guest reported vtl0 started successfully");
                Ok(())
            }
            StartVtl0Status::FAILURE => {
                let err = Vtl0StartError(String::from_utf8_lossy(remaining).into_owned());
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "guest reported vtl0 failed to start"
                );
                state.power_client.power_request(PowerRequest::PowerOff);
                Err(err)
            }
            _ => return Err(Error::InvalidFieldValue),
        };
        for response in state.waiting_for_vtl0_start.drain(..) {
            response.send(result.clone());
        }
        self.vtl0_start_report = Some(result);
        Ok(())
    }

    fn handle_vtl_crash(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let msg = get_protocol::VtlCrashNotification::read_from_prefix(message_buf)
            .ok_or(Error::MessageTooSmall)?;
        tracing::info!("Guest has reported a system crash {msg:x?}");
        Ok(())
    }

    fn handle_triple_fault(
        &mut self,
        state: &mut GuestEmulationDevice,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let (msg, remaining) =
            get_protocol::TripleFaultNotification::read_from_prefix_split(message_buf)
                .ok_or(Error::MessageTooSmall)?;
        let expected_len = msg.register_count as usize * size_of::<RegisterState>();
        if remaining.len() != expected_len {
            return Err(Error::InvalidFieldValue);
        }
        let registers = RegisterState::slice_from(remaining).unwrap();
        tracing::info!("Guest has reported a triple fault {msg:x?} {registers:?}");
        // TODO report and translate registers
        state
            .power_client
            .power_request(PowerRequest::TripleFault { vp: msg.vp_index });
        Ok(())
    }

    fn handle_modify_vtl2_settings_completed(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let (msg, remaining) =
            get_protocol::ModifyVtl2SettingsCompleteNotification::read_from_prefix_split(
                message_buf,
            )
            .ok_or(Error::MessageTooSmall)?;

        let modify = self.modify.take().ok_or(Error::InvalidSequence)?;
        let r = match msg.modify_status {
            get_protocol::ModifyVtl2SettingsStatus::SUCCESS => Ok(()),
            get_protocol::ModifyVtl2SettingsStatus::FAILURE => {
                let errors = std::str::from_utf8(
                    remaining
                        .get(..msg.result_document_size as usize)
                        .ok_or(Error::MessageTooSmall)?,
                )
                .map_err(|_| Error::InvalidFieldValue)?;

                Err(ModifyVtl2SettingsError::Guest(errors.to_owned()))
            }
            _ => return Err(Error::InvalidFieldValue),
        };
        modify.send(r);
        Ok(())
    }

    fn handle_device_platform_settings_v2(
        &mut self,
        state: &mut GuestEmulationDevice,
    ) -> Result<(), Error> {
        let vpci_boot_enabled;
        let enable_firmware_debugging;
        let disable_frontpage;
        let firmware_mode_is_pcat;
        let pcat_boot_device_order;
        let uefi_console_mode;
        match state.config.firmware {
            GuestFirmwareConfig::Uefi {
                enable_vpci_boot,
                firmware_debug,
                disable_frontpage: v_disable_frontpage,
                console_mode,
            } => {
                vpci_boot_enabled = enable_vpci_boot;
                enable_firmware_debugging = firmware_debug;
                disable_frontpage = v_disable_frontpage;
                firmware_mode_is_pcat = false;
                pcat_boot_device_order = None;
                uefi_console_mode = Some(console_mode);
            }
            GuestFirmwareConfig::Pcat { boot_order } => {
                vpci_boot_enabled = false;
                enable_firmware_debugging = false;
                disable_frontpage = false;
                firmware_mode_is_pcat = true;
                pcat_boot_device_order = Some(boot_order);
                uefi_console_mode = None;
            }
        }

        let json = get_protocol::dps_json::DevicePlatformSettingsV2Json {
            v1: get_protocol::dps_json::HclDevicePlatformSettings {
                com1: get_protocol::dps_json::HclUartSettings {
                    enable_port: state.config.com1,
                    debugger_mode: false,
                    enable_vmbus_redirector: state.config.com1,
                },
                com2: get_protocol::dps_json::HclUartSettings {
                    enable_port: state.config.com2,
                    debugger_mode: false,
                    enable_vmbus_redirector: state.config.com2,
                },
                enable_firmware_debugging,
                enable_tpm: state.config.enable_tpm,
                secure_boot_enabled: state.config.secure_boot_enabled,
                secure_boot_template_id: match state.config.secure_boot_template {
                    SecureBootTemplateType::SECURE_BOOT_DISABLED => HclSecureBootTemplateId::None,
                    SecureBootTemplateType::MICROSOFT_WINDOWS => {
                        HclSecureBootTemplateId::MicrosoftWindows
                    }
                    SecureBootTemplateType::MICROSOFT_UEFI_CERTIFICATE_AUTHORITY => {
                        HclSecureBootTemplateId::MicrosoftUEFICertificateAuthority
                    }
                    _ => panic!("Invalid secure boot template"),
                },
                enable_battery: state.config.enable_battery,
                console_mode: uefi_console_mode.unwrap_or(UefiConsoleMode::DEFAULT).0,
                ..Default::default()
            },
            v2: get_protocol::dps_json::HclDevicePlatformSettingsV2 {
                r#static: get_protocol::dps_json::HclDevicePlatformSettingsV2Static {
                    disable_frontpage,
                    vmbus_redirection_enabled: state.config.vmbus_redirection,
                    vtl2_settings: state.config.vtl2_settings.clone(),
                    firmware_mode_is_pcat,
                    // no_persist_secrets must be set to True in order to skip attestation.
                    no_persistent_secrets: true,
                    legacy_memory_map: false,
                    pause_after_boot_failure: false,
                    pxe_ip_v6: false,
                    measure_additional_pcrs: true,
                    disable_sha384_pcr: false,
                    media_present_enabled_by_default: false,
                    memory_protection_mode: 0,
                    vpci_boot_enabled,
                    vpci_instance_filter: None,
                    num_lock_enabled: false,
                    pcat_boot_device_order,
                    smbios: Default::default(),
                    watchdog_enabled: false,
                    always_relay_host_mmio: false,
                    imc_enabled: false,
                },
                dynamic: get_protocol::dps_json::HclDevicePlatformSettingsV2Dynamic {
                    is_servicing_scenario: state.save_restore_buf.is_some(),
                    ..Default::default()
                },
            },
        };

        let json_data = serde_json::to_vec(&json).map_err(Error::SerializeDpsV2)?;

        // TODO: we'll need this when we actively support VMs with lots of disks
        // and/or NICs. This is because the primary thing that makes the DPSv2
        // payload grow in size is the embedded Vtl2Settings payload (which is
        // dependant on the number of attached disks/NICs)
        if json_data.len() > MAX_PAYLOAD_SIZE {
            return Err(Error::LargeDpsV2Unimplemented);
        }

        // Protocol wart: the request is marked as
        // `HostRequests::DEVICE_PLATFORM_SETTINGS_V2`, but the response must be
        // `HostRequests::DEVICE_PLATFORM_SETTINGS_V2_REV1`
        let response = get_protocol::DevicePlatformSettingsResponseV2Rev1 {
            message_header: HeaderGeneric::new(HostRequests::DEVICE_PLATFORM_SETTINGS_V2_REV1),
            size: json_data.len() as u32,
            payload_state: get_protocol::LargePayloadState::END,
        };

        self.channel
            .try_send_vectored(&[IoSlice::new(response.as_bytes()), IoSlice::new(&json_data)])
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn send_hardcoded_battery_update(&mut self) -> Result<(), Error> {
        let mut flags = BatteryStatusFlags::new();
        flags.set_ac_online(true);
        flags.set_battery_present(true);
        flags.set_charging(true);
        flags.set_discharging(false);
        flags.set_reserved(0);

        let response = BatteryStatusNotification::new(flags, 1000, 950, 1);
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }
}
