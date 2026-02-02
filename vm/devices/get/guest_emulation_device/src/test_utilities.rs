// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Error;
use crate::GedChannel;
use crate::GuestConfig;
use crate::GuestEmulationDevice;
use crate::GuestFirmwareConfig;
use crate::IgvmAgentTestPlan;
use crate::IgvmAgentTestSetting;
use get_protocol::HostNotifications;
use get_protocol::HostRequests;
use get_protocol::SecureBootTemplateType;
use get_protocol::UefiConsoleMode;
use get_protocol::test_utilities::TEST_VMGS_CAPACITY;
use get_resources::ged::GuestEmulationRequest;
use get_resources::ged::GuestServicingFlags;
use guestmem::GuestMemory;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use power_resources::PowerRequest;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::StopTask;
use task_control::TaskControl;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_ring::FlatRingMem;
use vmbus_ring::RingMem;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Debug, Clone)]
pub enum Event {
    Response(Vec<u8>),
    Halt(PowerRequest),
}

/// Response the GED sends after each request received from the host. This
/// allows us to send multiple Notifications/Responses to the Guest in
/// between each request.
///
/// Use `default()` for host notifications to allow the host
/// to read in the notification.
#[derive(Debug, Default, Clone)]
pub struct TestGetResponses {
    responses: Vec<Event>,
}

impl TestGetResponses {
    pub fn new(response: Event) -> Self {
        Self {
            responses: vec![response],
        }
    }

    pub fn add_response(mut self, response: Event) -> Self {
        self.responses.push(response);
        self
    }
}

pub struct TestGedChannel<T: RingMem = GpadlRingMem> {
    channel: MessagePipe<T>,
    vmgs: Vec<u8>,
    responses: Vec<TestGetResponses>,
    version: get_protocol::ProtocolVersion,
    halt_reason: Arc<Mutex<Option<PowerRequest>>>,
}

impl<T: RingMem + Unpin> TestGedChannel<T> {
    pub fn new(
        channel: MessagePipe<T>,
        responses: Vec<TestGetResponses>,
        version: get_protocol::ProtocolVersion,
        halt_reason: Arc<Mutex<Option<PowerRequest>>>,
    ) -> Self {
        Self {
            channel,
            vmgs: vec![0; TEST_VMGS_CAPACITY],
            responses,
            version,
            halt_reason,
        }
    }

    async fn process(&mut self, state: &mut GuestEmulationDevice) -> Result<(), Error> {
        tracing::trace!("Begin GetChannel process()");
        let mut version_accepted = false;

        // Negotiate the version
        while !version_accepted {
            let mut version_request = get_protocol::VersionRequest::new_zeroed();
            self.channel
                .recv_exact(version_request.as_mut_bytes())
                .await
                .map_err(Error::Vmbus)?;

            if version_request.message_header.message_id() != HostRequests::VERSION {
                return Err(Error::InvalidSequence);
            }

            version_accepted = version_request.version == self.version;
            let version_response = get_protocol::VersionResponse::new(version_accepted);

            self.channel
                .send(version_response.as_bytes())
                .await
                .map_err(Error::Vmbus)?;
        }

        tracing::info!("version negotiated successfully!");

        for response_vec in &mut self.responses {
            let mut message_buf = vec![0; get_protocol::MAX_MESSAGE_SIZE];
            let bytes_read = self
                .channel
                .recv(&mut message_buf)
                .await
                .map_err(Error::Vmbus)?;
            if bytes_read == 0 {
                panic!("Read in 0 bytes, likely means guest-side pipe has closed.")
            }

            let header = get_protocol::HeaderRaw::read_from_prefix(&message_buf[..4])
                .map_err(|_| Error::MessageTooSmall)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

            if header.message_version != get_protocol::MessageVersions::HEADER_VERSION_1 {
                return Err(Error::HeaderVersion(header.message_version));
            }

            if header.message_type == get_protocol::MessageTypes::HOST_NOTIFICATION {
                let header: get_protocol::HeaderHostNotification =
                    header.try_into().expect("valid host request");
                match header.message_id() {
                    HostNotifications::EVENT_LOG => {
                        let notification = get_protocol::EventLogNotification::read_from_prefix(
                            &message_buf[..size_of::<get_protocol::EventLogNotification>()],
                        )
                        .unwrap()
                        .0; // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                        self.vmgs[0] = notification.event_log_id.0 as u8;
                    }
                    HostNotifications::POWER_OFF => {
                        state.power_client.power_request(PowerRequest::PowerOff);
                    }
                    HostNotifications::RESET => {
                        state.power_client.power_request(PowerRequest::Reset);
                    }
                    _ => todo!("add when more tests are added"),
                }
            }

            for response in &mut response_vec.responses {
                match response {
                    Event::Response(response) => {
                        use get_protocol::test_utilities::TEST_VMGS_SECTOR_SIZE;
                        // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                        // Check if response needs special handling. Otherwise, send
                        // response directly back to the guest.
                        let response_header =
                            get_protocol::HeaderRaw::read_from_prefix(&response[..4])
                                .unwrap()
                                .0;
                        match response_header.message_type {
                            get_protocol::MessageTypes::HOST_RESPONSE => {
                                let header: get_protocol::HeaderHostRequest =
                                    header.try_into().expect("valid host request");
                                match header.message_id() {
                                    HostRequests::VMGS_READ => {
                                        let request_size =
                                            size_of::<get_protocol::VmgsReadRequest>();
                                        let request =
                                            get_protocol::VmgsReadRequest::read_from_prefix(
                                                &message_buf[..request_size],
                                            )
                                            .unwrap()
                                            .0;
                                        // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                                        let offset = request.sector_offset as usize
                                            * TEST_VMGS_SECTOR_SIZE as usize;
                                        let length = request.sector_count as usize
                                            * TEST_VMGS_SECTOR_SIZE as usize;
                                        response.extend_from_slice(&self.vmgs[offset..][..length])
                                    }
                                    HostRequests::VMGS_WRITE => {
                                        let request_size =
                                            size_of::<get_protocol::VmgsWriteRequest>();
                                        let request =
                                            get_protocol::VmgsWriteRequest::read_from_prefix(
                                                &message_buf[..request_size],
                                            )
                                            .unwrap()
                                            .0;
                                        // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                                        let buf = &message_buf[request_size..];
                                        let offset = request.sector_offset as usize
                                            * TEST_VMGS_SECTOR_SIZE as usize;
                                        let length = request.sector_count as usize
                                            * TEST_VMGS_SECTOR_SIZE as usize;
                                        self.vmgs[offset..][..length]
                                            .copy_from_slice(&buf[..length]);
                                    }
                                    _ => (), // Other requests don't need special handling
                                }
                            }
                            get_protocol::MessageTypes::GUEST_NOTIFICATION => (),
                            _ => todo!("Unhandled scenario"),
                        }

                        self.channel.send(response).await.map_err(Error::Vmbus)?;
                    }
                    Event::Halt(reason) => {
                        assert_eq!(self.halt_reason.lock().unwrap(), *reason);
                    }
                }
            }
        }

        Ok(())
    }
}

/// Create the host Guest Emulation Device and corresponding I/O.
///
/// If `ged_responses` is Some(), then TestGedChannel will be used to
/// control what responses the Host sends. Otherwise, if `ged_responses` is
/// None, we will use the regular GedChannel to automate responses.
pub fn create_host_channel(
    spawn: impl Spawn,
    host_vmbus: MessagePipe<FlatRingMem>,
    ged_responses: Option<Vec<TestGetResponses>>,
    version: get_protocol::ProtocolVersion,
    guest_memory: Option<GuestMemory>,
    igvm_agent_plan: Option<IgvmAgentTestPlan>,
) -> TestGedClient {
    let guest_config = GuestConfig {
        firmware: GuestFirmwareConfig::Uefi {
            firmware_debug: false,
            enable_vpci_boot: false,
            disable_frontpage: false,
            console_mode: UefiConsoleMode::DEFAULT,
            default_boot_always_attempt: false,
        },
        com1: true,
        com2: true,
        vmbus_redirection: false,
        enable_tpm: false,
        vtl2_settings: None,
        secure_boot_enabled: false,
        secure_boot_template: SecureBootTemplateType::SECURE_BOOT_DISABLED,
        enable_battery: false,
        no_persistent_secrets: true,
        guest_state_lifetime: Default::default(),
        guest_state_encryption_policy: Default::default(),
        management_vtl_features: Default::default(),
        efi_diagnostics_log_level: Default::default(),
        hv_sint_enabled: false,
    };

    let halt_reason = Arc::new(Mutex::new(None));
    let halt_reason_clone = halt_reason.clone();

    let halt = {
        move |reason: PowerRequest| {
            let mut halt_reason = halt_reason_clone.lock();
            *halt_reason = Some(reason);
            tracing::info!(?reason, "guest initiated reset via GED");
        }
    };

    let (send, recv) = mesh::channel();

    let mut ged_state: GuestEmulationDevice = GuestEmulationDevice::new(
        guest_config,
        halt.into(),
        None,
        recv,
        None,
        Some(disklayer_ram::ram_disk(TEST_VMGS_CAPACITY as u64, false).unwrap()),
        igvm_agent_plan.map(IgvmAgentTestSetting::TestPlan),
        false,
    );

    if let Some(ged_responses) = ged_responses {
        let mut host_get_channel =
            TestGedChannel::new(host_vmbus, ged_responses, version, halt_reason);
        let task = spawn.spawn("GED host channel", async move {
            host_get_channel.process(&mut ged_state).await
        });

        TestGedClient {
            _task: TestTask::Test(task),
            sender: send,
        }
    } else {
        let mut task = TaskControl::new(ged_state);
        // Optionally provide a guest memory backing so handlers like IGVM_ATTEST can write
        // response payloads into the shared buffer GPAs provided by GET.
        let gm = guest_memory.unwrap_or_else(GuestMemory::empty);
        task.insert(
            spawn,
            "automated GED host channel",
            GedChannel::new(host_vmbus, gm),
        );
        task.start();

        TestGedClient {
            _task: TestTask::Prod(task),
            sender: send,
        }
    }
}

impl AsyncRun<GedChannel<FlatRingMem>> for GuestEmulationDevice {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        channel: &mut GedChannel<FlatRingMem>,
    ) -> Result<(), task_control::Cancelled> {
        match channel.process(stop, self).await {
            Ok(()) => Ok(()),
            Err(Error::Cancelled(err)) => Err(err),
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "error processing GED channel"
                );
                Ok(())
            }
        }
    }
}

/// A test GED client from which a caller can invoke GED-initiated
/// behavior, for example save/restore of guest VTL2 state.
/// Currently used to support GET/GED testing.
/// TODO: add additional genericity to the sender so it
/// can send more than just GuestNotifications; this is not
/// yet needed, however.
pub struct TestGedClient {
    _task: TestTask,
    sender: mesh::Sender<GuestEmulationRequest>,
}

#[expect(dead_code)] // Tasks are spawned and just need to be held.
enum TestTask {
    Test(Task<Result<(), Error>>),
    Prod(TaskControl<GuestEmulationDevice, GedChannel<FlatRingMem>>),
}

impl TestGedClient {
    pub async fn test_save_guest_vtl2_state(&mut self) {
        self.sender
            .call_failable(
                GuestEmulationRequest::SaveGuestVtl2State,
                GuestServicingFlags::default(),
            )
            .await
            .expect("no failure");
    }
}
