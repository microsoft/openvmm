// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest Emulation Device - GED
//!
//! The GED is the host side of a communication channel that uses VMBUS to
//! communicate between Guest and Host. This is an implementation to support
//! better integration testing within the OpenVMM CI, and is not at
//! feature-parity with the implementation in Hyper-V.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

#[cfg(feature = "test_utilities")]
pub mod test_utilities;

pub use test_igvm_agent_lib::IgvmAgentAction;
pub use test_igvm_agent_lib::IgvmAgentTestPlan;
pub use test_igvm_agent_lib::IgvmAgentTestSetting;

use async_trait::async_trait;
use core::mem::size_of;
use disk_backend::Disk;
use futures::FutureExt;
use futures::StreamExt;
use get_protocol::BatteryStatusFlags;
use get_protocol::BatteryStatusNotification;
use get_protocol::GspCleartextContent;
use get_protocol::GspExtendedStatusFlags;
use get_protocol::HeaderGeneric;
use get_protocol::HostNotifications;
use get_protocol::HostRequests;
use get_protocol::MAX_PAYLOAD_SIZE;
use get_protocol::RegisterState;
use get_protocol::SaveGuestVtl2StateFlags;
use get_protocol::SecureBootTemplateType;
use get_protocol::StartVtl0Status;
use get_protocol::UefiConsoleMode;
use get_protocol::VmgsIoStatus;
use get_protocol::dps_json::EfiDiagnosticsLogLevelType;
use get_protocol::dps_json::GuestStateEncryptionPolicy;
use get_protocol::dps_json::GuestStateLifetime;
use get_protocol::dps_json::HclSecureBootTemplateId;
use get_protocol::dps_json::ManagementVtlFeatures;
use get_protocol::dps_json::PcatBootDevice;
use get_resources::ged::FirmwareEvent;
use get_resources::ged::GuestEmulationRequest;
use get_resources::ged::GuestServicingFlags;
use get_resources::ged::ModifyVtl2SettingsError;
use get_resources::ged::SaveRestoreError;
use get_resources::ged::Vtl0StartError;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use jiff::Zoned;
use jiff::civil::DateTime;
use jiff::civil::date;
use jiff::tz::TimeZone;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use power_resources::PowerRequest;
use power_resources::PowerRequestClient;
use scsi_buffers::OwnedRequestBuffers;
use std::io::IoSlice;
use task_control::StopTask;
use test_igvm_agent_lib::TestIgvmAgent;
use thiserror::Error;
use video_core::FramebufferControl;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_ring::RingMem;
use vmcore::save_restore::SavedStateNotSupported;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

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
    #[error("invalid IGVM_ATTEST request")]
    InvalidIgvmAttestRequest,
    #[error("test IGVM agent error")]
    TestIgvmAgent(#[source] test_igvm_agent_lib::Error),
    #[error("failed to write to shared memory")]
    SharedMemoryWriteFailed(#[source] guestmem::GuestMemoryError),
}

impl From<task_control::Cancelled> for Error {
    fn from(value: task_control::Cancelled) -> Self {
        Error::Cancelled(value)
    }
}

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
    /// Suppress attestation.
    pub no_persistent_secrets: bool,
    /// Guest state lifetime
    #[inspect(debug)]
    pub guest_state_lifetime: GuestStateLifetime,
    /// Guest state encryption policy
    #[inspect(debug)]
    pub guest_state_encryption_policy: GuestStateEncryptionPolicy,
    /// Management VTL feature flags
    #[inspect(debug)]
    pub management_vtl_features: ManagementVtlFeatures,
    /// EFI diagnostics log level
    #[inspect(debug)]
    pub efi_diagnostics_log_level: EfiDiagnosticsLogLevelType,
    /// Enable PPI-based SINT ACPI device for ARM64 Linux L1VH
    pub hv_sint_enabled: bool,
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
        /// Perform a default boot even if boot entries exist and fail
        default_boot_always_attempt: bool,
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
    firmware_event_send: Option<mesh::Sender<FirmwareEvent>>,
    #[inspect(skip)]
    framebuffer_control: Option<Box<dyn FramebufferControl>>,
    #[inspect(skip)]
    guest_request_recv: mesh::Receiver<GuestEmulationRequest>,
    #[inspect(skip)]
    waiting_for_vtl0_start: Vec<Rpc<(), Result<(), Vtl0StartError>>>,

    vmgs: Option<VmgsState>,

    #[inspect(with = "Option::is_some")]
    save_restore_buf: Option<Vec<u8>>,
    last_save_restore_buf_len: usize,

    igvm_agent_setting: Option<IgvmAgentTestSetting>,

    /// Test agent implementation for `handle_igvm_attest`
    #[inspect(skip)]
    igvm_agent: TestIgvmAgent,

    test_gsp_by_id: bool,
}

#[derive(Inspect)]
struct VmgsState {
    /// The underlying VMGS disk.
    disk: Disk,
    /// Memory for the disk to DMA to/from.
    mem: GuestMemory,
}

impl GuestEmulationDevice {
    /// Create a new Host side GET device.
    pub fn new(
        config: GuestConfig,
        power_client: PowerRequestClient,
        firmware_event_send: Option<mesh::Sender<FirmwareEvent>>,
        guest_request_recv: mesh::Receiver<GuestEmulationRequest>,
        framebuffer_control: Option<Box<dyn FramebufferControl>>,
        vmgs_disk: Option<Disk>,
        igvm_agent_setting: Option<IgvmAgentTestSetting>,
        test_gsp_by_id: bool,
    ) -> Self {
        Self {
            config,
            power_client,
            firmware_event_send,
            framebuffer_control,
            guest_request_recv,
            vmgs: vmgs_disk.map(|disk| VmgsState {
                disk,
                mem: GuestMemory::allocate(MAX_PAYLOAD_SIZE),
            }),
            save_restore_buf: None,
            waiting_for_vtl0_start: Vec::new(),
            last_save_restore_buf_len: 0,
            igvm_agent_setting,
            igvm_agent: TestIgvmAgent::new(),
            test_gsp_by_id,
        }
    }

    fn send_event(&self, event: FirmwareEvent) {
        if let Some(sender) = &self.firmware_event_send {
            sender.send(event);
        }
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
        guest_memory: GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(GedChannel::new(pipe, guest_memory))
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
    modify: Option<Rpc<(), Result<(), ModifyVtl2SettingsError>>>,
    #[inspect(skip)]
    gm: GuestMemory,
}

struct InProgressSave {
    rpc: Rpc<GuestServicingFlags, Result<(), SaveRestoreError>>,
    buffer: Vec<u8>,
}

enum GedState {
    Init,
    Ready,
    SendingRestore { written: usize },
}

impl<T: RingMem + Unpin> GedChannel<T> {
    fn new(channel: MessagePipe<T>, guest_memory: GuestMemory) -> Self {
        Self {
            channel,
            save: None,
            state: GedState::Init,
            vtl0_start_report: None,
            modify: None,
            gm: guest_memory,
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
                    stop.until_stopped(self.channel.recv_exact(version_request.as_mut_bytes()))
                        .await?
                        .map_err(Error::Vmbus)?;

                    if version_request.message_header.message_id() != HostRequests::VERSION {
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

                    self.channel
                        .try_send_vectored(&[
                            IoSlice::new(host_response_header.as_bytes()),
                            IoSlice::new(&buffer[*written..][..payload_len]),
                        ])
                        .map_err(Error::Vmbus)?;

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
        let header = get_protocol::HeaderRaw::read_from_prefix(message_buf)
            .map_err(|_| Error::MessageTooSmall)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

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
            GuestEmulationRequest::WaitForVtl0Start(rpc) => {
                if let Some(result) = self.vtl0_start_report.clone() {
                    rpc.complete(result);
                } else {
                    state.waiting_for_vtl0_start.push(rpc);
                }
            }
            GuestEmulationRequest::ModifyVtl2Settings(rpc) => {
                let (data, response) = rpc.split();
                if self.modify.is_some() {
                    response.complete(Err(ModifyVtl2SettingsError::OperationInProgress));
                    return Ok(());
                }

                // TODO: support larger payloads.
                if data.len() > MAX_PAYLOAD_SIZE {
                    response.complete(Err(ModifyVtl2SettingsError::LargeSettingsNotSupported));
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
            GuestEmulationRequest::SaveGuestVtl2State(rpc) => {
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
                        capabilities_flags: SaveGuestVtl2StateFlags::new()
                            .with_enable_nvme_keepalive(rpc.input().nvme_keepalive)
                            .with_enable_mana_keepalive(rpc.input().mana_keepalive),
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
                            rpc,
                            buffer: Vec::new(),
                        })
                    }
                    Err(err) => rpc.complete(Err(err)),
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
        match header.message_id() {
            HostRequests::TIME => self.handle_time()?,
            HostRequests::BIOS_BOOT_FINALIZE => self.handle_bios_boot_finalize(message_buf)?,
            HostRequests::VMGS_GET_DEVICE_INFO => self.handle_vmgs_get_device_info(state)?,
            HostRequests::VMGS_READ => self.handle_vmgs_read(state, message_buf).await?,
            HostRequests::VMGS_WRITE => self.handle_vmgs_write(state, message_buf).await?,
            HostRequests::VMGS_FLUSH => self.handle_vmgs_flush(state).await?,
            HostRequests::GUEST_STATE_PROTECTION => {
                self.handle_guest_state_protection(message_buf)?
            }
            HostRequests::GUEST_STATE_PROTECTION_BY_ID => {
                self.handle_guest_state_protection_by_id(state.test_gsp_by_id)?;
            }
            HostRequests::IGVM_ATTEST => self.handle_igvm_attest(message_buf, state)?,
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
                tracing::error!(message_id = ?header.message_id(), "unexpected message");
                return Err(Error::InvalidSequence);
            }
        };
        Ok(())
    }

    fn handle_bios_boot_finalize(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let msg = get_protocol::BiosBootFinalizeRequest::read_from_prefix(message_buf)
            .map_err(|_| Error::MessageTooSmall)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        tracing::trace!(?msg, "Bios Boot Finalize request");

        let response = get_protocol::BiosBootFinalizeResponse::new();
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_time(&mut self) -> Result<(), Error> {
        const WINDOWS_EPOCH: DateTime = date(1601, 1, 1).at(0, 0, 0, 0);

        let now = Zoned::now();

        // utc in TimeResponse is in units of 100ns since the windows epoch
        let since_win_epoch = (WINDOWS_EPOCH
            .to_zoned(TimeZone::UTC)
            .expect("windows epoch value to be valid")
            .timestamp()
            .duration_until(now.timestamp())
            .as_nanos()
            / 100) as i64;

        // tz_offset is in minutes between UTC and local time (as stored
        // in a windows TIME_ZONE_INFORMATION struct)
        let tz_offset = (now.offset().seconds() / 60) as i16;
        let response = get_protocol::TimeResponse::new(0, since_win_epoch, tz_offset, false);

        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_vmgs_get_device_info(
        &mut self,
        state: &mut GuestEmulationDevice,
    ) -> Result<(), Error> {
        let response = if let Some(vmgs) = &state.vmgs {
            get_protocol::VmgsGetDeviceInfoResponse::new(
                VmgsIoStatus::SUCCESS,
                vmgs.disk.sector_count(),
                vmgs.disk.sector_size().try_into().unwrap(),
                vmgs.disk.physical_sector_size().try_into().unwrap(),
                MAX_PAYLOAD_SIZE as u32,
            )
        } else {
            get_protocol::VmgsGetDeviceInfoResponse::new(VmgsIoStatus::DEVICE_ERROR, 0, 0, 0, 0)
        };
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    async fn handle_vmgs_read(
        &mut self,
        state: &mut GuestEmulationDevice,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let message = get_protocol::VmgsReadRequest::read_from_prefix(message_buf)
            .map_err(|_| Error::MessageTooSmall)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let (status, payload) = if let Some(vmgs) = &mut state.vmgs {
            let len = message.sector_count as u64 * vmgs.disk.sector_size() as u64;
            if len > MAX_PAYLOAD_SIZE as u64 {
                return Err(Error::InvalidFieldValue);
            }

            // FUTURE: this IO will block VM state changes. Since this IO may
            // take a long time, consider storing the future and awaiting in a
            // cancellable context.
            match vmgs
                .disk
                .read_vectored(
                    &OwnedRequestBuffers::linear(0, len as usize, true).buffer(&vmgs.mem),
                    message.sector_offset,
                )
                .await
            {
                Ok(()) => (
                    VmgsIoStatus::SUCCESS,
                    &vmgs
                        .mem
                        .inner_buf_mut()
                        .expect("memory should not be aliased")[..len as usize],
                ),
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "vmgs read error"
                    );
                    (VmgsIoStatus::DEVICE_ERROR, &[] as _)
                }
            }
        } else {
            (VmgsIoStatus::DEVICE_ERROR, &[] as _)
        };

        let response = get_protocol::VmgsReadResponse::new(status);
        self.channel
            .try_send_vectored(&[IoSlice::new(response.as_bytes()), IoSlice::new(payload)])
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    async fn handle_vmgs_write(
        &mut self,
        state: &mut GuestEmulationDevice,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let (message, rest) = get_protocol::VmgsWriteRequest::read_from_prefix(message_buf)
            .map_err(|_| Error::MessageTooSmall)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let status = if let Some(vmgs) = &mut state.vmgs {
            let len = message.sector_count as u64 * vmgs.disk.sector_size() as u64;
            if len > MAX_PAYLOAD_SIZE as u64 {
                return Err(Error::InvalidFieldValue);
            }

            vmgs.mem
                .write_at(0, rest.get(..len as usize).ok_or(Error::MessageTooSmall)?)
                .unwrap();

            // FUTURE: this IO will block VM state changes. Since this IO may
            // take a long time, consider storing the future and awaiting in a
            // cancellable context.
            match vmgs
                .disk
                .write_vectored(
                    &OwnedRequestBuffers::linear(0, len as usize, false).buffer(&vmgs.mem),
                    message.sector_offset,
                    false,
                )
                .await
            {
                Ok(()) => VmgsIoStatus::SUCCESS,
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "vmgs write error"
                    );
                    VmgsIoStatus::DEVICE_ERROR
                }
            }
        } else {
            VmgsIoStatus::DEVICE_ERROR
        };

        let response = get_protocol::VmgsWriteResponse::new(status);
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    async fn handle_vmgs_flush(&mut self, state: &mut GuestEmulationDevice) -> Result<(), Error> {
        let status = if let Some(vmgs) = &mut state.vmgs {
            // FUTURE: this IO will block VM state changes. Since this IO may
            // take a long time, consider storing the future and awaiting in a
            // cancellable context.
            match vmgs.disk.sync_cache().await {
                Ok(()) => VmgsIoStatus::SUCCESS,
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "vmgs flush error"
                    );
                    VmgsIoStatus::DEVICE_ERROR
                }
            }
        } else {
            VmgsIoStatus::DEVICE_ERROR
        };

        let response = get_protocol::VmgsFlushResponse::new(status);
        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_guest_state_protection(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let _message = get_protocol::GuestStateProtectionRequest::read_from_prefix(
            &message_buf.as_bytes()[..size_of::<get_protocol::GuestStateProtectionRequest>()],
        )
        .map_err(|_| Error::MessageTooSmall)?
        .0; // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)

        let mut response = get_protocol::GuestStateProtectionResponse::new_zeroed();
        response.message_header = HeaderGeneric::new(HostRequests::GUEST_STATE_PROTECTION);

        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    fn handle_guest_state_protection_by_id(&mut self, test_gsp_by_id: bool) -> Result<(), Error> {
        let mut response = get_protocol::GuestStateProtectionByIdResponse {
            message_header: HeaderGeneric::new(HostRequests::GUEST_STATE_PROTECTION_BY_ID),
            seed: GspCleartextContent::new_zeroed(),
            extended_status_flags: GspExtendedStatusFlags::new()
                .with_no_registry_file(!test_gsp_by_id),
        };

        if test_gsp_by_id {
            const TEST_GSP_SEED_LEN: usize = 32;
            const TEST_GSP_SEED: [u8; TEST_GSP_SEED_LEN] = [
                0x94, 0xBD, 0x3A, 0xA5, 0xF4, 0x51, 0x97, 0x9F, 0x1A, 0x8B, 0x29, 0x41, 0x91, 0x90,
                0x73, 0x1B, 0x30, 0xB0, 0x76, 0xEA, 0x6E, 0xDC, 0x1F, 0x8F, 0x22, 0xC7, 0x07, 0x57,
                0x67, 0xCD, 0x87, 0x5B,
            ];
            response.seed.length = TEST_GSP_SEED_LEN as u32;
            response.seed.buffer[..TEST_GSP_SEED_LEN].copy_from_slice(&TEST_GSP_SEED);
        }

        self.channel
            .try_send(response.as_bytes())
            .map_err(Error::Vmbus)?;
        Ok(())
    }

    /// Stub implementation that simulates the behavior of GED and the host agent.
    /// Used only for test scenarios such as VMM tests.
    fn handle_igvm_attest(
        &mut self,
        message_buf: &[u8],
        state: &mut GuestEmulationDevice,
    ) -> Result<(), Error> {
        tracing::info!(test_setting = ?state.igvm_agent_setting, "Handle IGVM Attest request");

        let request = get_protocol::IgvmAttestRequest::read_from_prefix(message_buf)
            .map_err(|_| Error::MessageTooSmall)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        // Request sanitization (match GED behavior)
        if request.agent_data_length as usize > request.agent_data.len()
            || request.report_length as usize > request.report.len()
            || request.number_gpa as usize > get_protocol::IGVM_ATTEST_MSG_MAX_SHARED_GPA
        {
            Err(Error::InvalidIgvmAttestRequest)?
        }

        let (response_payload, length) = {
            if let Some(setting) = &state.igvm_agent_setting {
                state.igvm_agent.install_plan_from_setting(setting);
            }

            state
                .igvm_agent
                .handle_request(&request.report[..request.report_length as usize])
                .map_err(Error::TestIgvmAgent)?
        };

        // Write the response payload to the guest's shared memory
        self.gm
            .write_at(request.shared_gpa[0], &response_payload)
            .map_err(Error::SharedMemoryWriteFailed)?;

        let response = get_protocol::IgvmAttestResponse {
            message_header: HeaderGeneric::new(HostRequests::IGVM_ATTEST),
            length,
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
            get_protocol::SaveGuestVtl2StateRequest::read_from_prefix(message_buf)
                .map_err(|_| Error::MessageTooSmall)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
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
            save.rpc.complete(r);
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
                    .map_err(|_| Error::MessageTooSmall)?
                    .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                let gpa = message.gpa;
                tracing::debug!("Received map framebuffer request from guest {:#x}", gpa);
                framebuffer_control.map(gpa).await;
                get_protocol::MapFramebufferStatus::SUCCESS
            } else {
                tracing::warn!(
                    "Guest requested framebuffer mapping but no framebuffer control was provided to the GET"
                );
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
                tracing::warn!(
                    "Guest requested framebuffer mapping but no framebuffer control was provided to the GET"
                );
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
            .map_err(|_| Error::MessageTooSmall)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

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
            .map_err(|_| Error::MessageTooSmall)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
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
        match header.message_id() {
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
            .map_err(|_| Error::MessageTooSmall)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
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
                .map_err(|_| Error::MessageTooSmall)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
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
            get_protocol::StartVtl0CompleteNotification::read_from_prefix(message_buf)
                .map_err(|_| Error::MessageTooSmall)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
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
            response.complete(result.clone());
        }
        self.vtl0_start_report = Some(result);
        Ok(())
    }

    fn handle_vtl_crash(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let msg = get_protocol::VtlCrashNotification::read_from_prefix(message_buf)
            .map_err(|_| Error::MessageTooSmall)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        tracing::info!("Guest has reported a system crash {msg:x?}");
        Ok(())
    }

    fn handle_triple_fault(
        &mut self,
        state: &mut GuestEmulationDevice,
        message_buf: &[u8],
    ) -> Result<(), Error> {
        let (msg, remaining) = get_protocol::TripleFaultNotification::read_from_prefix(message_buf)
            .map_err(|_| Error::MessageTooSmall)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        let expected_len = msg.register_count as usize * size_of::<RegisterState>();
        if remaining.len() != expected_len {
            return Err(Error::InvalidFieldValue);
        }
        let registers = <[RegisterState]>::ref_from_bytes(remaining).unwrap();
        tracing::info!("Guest has reported a triple fault {msg:x?} {registers:?}");
        // TODO report and translate registers
        state
            .power_client
            .power_request(PowerRequest::TripleFault { vp: msg.vp_index });
        Ok(())
    }

    fn handle_modify_vtl2_settings_completed(&mut self, message_buf: &[u8]) -> Result<(), Error> {
        let (msg, remaining) =
            get_protocol::ModifyVtl2SettingsCompleteNotification::read_from_prefix(message_buf)
                .map_err(|_| Error::MessageTooSmall)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

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
        modify.complete(r);
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
        let default_boot_always_attempt;
        match state.config.firmware {
            GuestFirmwareConfig::Uefi {
                enable_vpci_boot,
                firmware_debug,
                disable_frontpage: v_disable_frontpage,
                console_mode,
                default_boot_always_attempt: v_default_boot_always_attempt,
            } => {
                vpci_boot_enabled = enable_vpci_boot;
                enable_firmware_debugging = firmware_debug;
                disable_frontpage = v_disable_frontpage;
                firmware_mode_is_pcat = false;
                pcat_boot_device_order = None;
                uefi_console_mode = Some(console_mode);
                default_boot_always_attempt = v_default_boot_always_attempt;
            }
            GuestFirmwareConfig::Pcat { boot_order } => {
                vpci_boot_enabled = false;
                enable_firmware_debugging = false;
                disable_frontpage = false;
                firmware_mode_is_pcat = true;
                pcat_boot_device_order = Some(boot_order);
                uefi_console_mode = None;
                default_boot_always_attempt = false;
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
                bios_guid: if state.test_gsp_by_id {
                    guid::guid!("2b701019-2816-4a85-9692-3981f1af4423")
                } else {
                    Default::default()
                },
                ..Default::default()
            },
            v2: get_protocol::dps_json::HclDevicePlatformSettingsV2 {
                r#static: get_protocol::dps_json::HclDevicePlatformSettingsV2Static {
                    disable_frontpage,
                    vmbus_redirection_enabled: state.config.vmbus_redirection,
                    vtl2_settings: state.config.vtl2_settings.clone(),
                    firmware_mode_is_pcat,
                    no_persistent_secrets: state.config.no_persistent_secrets,
                    legacy_memory_map: false,
                    pause_after_boot_failure: false,
                    pxe_ip_v6: false,
                    measure_additional_pcrs: true,
                    disable_sha384_pcr: false,
                    media_present_enabled_by_default: false,
                    memory_protection_mode: 0,
                    default_boot_always_attempt,
                    vpci_boot_enabled,
                    vpci_instance_filter: None,
                    num_lock_enabled: false,
                    pcat_boot_device_order,
                    smbios: Default::default(),
                    watchdog_enabled: false,
                    always_relay_host_mmio: false,
                    imc_enabled: false,
                    cxl_memory_enabled: false,
                    guest_state_lifetime: state.config.guest_state_lifetime,
                    guest_state_encryption_policy: state.config.guest_state_encryption_policy,
                    management_vtl_features: state.config.management_vtl_features,
                    efi_diagnostics_log_level: state.config.efi_diagnostics_log_level,
                    hv_sint_enabled: state.config.hv_sint_enabled,
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
